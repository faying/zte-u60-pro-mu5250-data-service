#!/usr/bin/env python3
"""SSE connection lifecycle (P2-4 of the 2026-10-04 cross-service audit).

1. Shutdown: with /v2/events clients connected and silent (one
   reading, one not reading), SIGTERM makes zwrt-datad exit cleanly (code 0)
   well inside procd's term_timeout (~5 s), and the reading client sees the
   stream end instead of hanging.
2. Write timeout: clients that stop reading (tiny receive buffer, never read)
   are dropped once their writes stall for the write timeout, and their SSE
   slots come back (a new stream gets 200 where it got 503), while a client
   that keeps reading stays connected.

Usage: rust_sse_lifecycle_test.py PATH_TO_ZWRT_DATAD   (starts it against the mocks)
"""
import http.client
import os
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
WRITE_TIMEOUT_MS = 1500
EXIT_LIMIT = 1.5
EXIT_LIMIT_STALLED = 4.0
FREE_LIMIT = 20


def close(sock):
    # 并行用例 fork 出的子进程也拿着 fd，只 close 不一定真断（见 bf363bd）。
    try:
        sock.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    sock.close()


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def start(base, port, extra_env=None):
    env = dict(
        os.environ,
        ZWRT_DATAD_DIR=str(base / "data"),
        ZWRT_DATAD_UBUS_BIN=str(ROOT / "tests/mock_ubus.sh"),
        ZWRT_DATAD_UCI_BIN=str(ROOT / "tests/mock_uci.sh"),
        MOCK_CALL_LOG=str(base / "calls.log"),
        MOCK_UCI_STATE_DIR=str(base / "uci-state"),
        **(extra_env or {}),
    )
    proc = subprocess.Popen(
        [str(BINARY), "-i", "500", "-p", str(port)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if proc.poll() is not None:
            raise AssertionError(f"zwrt-datad exited early: {proc.returncode}")
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            connection.request("GET", "/v2/state")
            response = connection.getresponse()
            response.read()
            connection.close()
            if response.status == 200:
                return proc
        except OSError:
            pass
        time.sleep(0.1)
    stop(proc)
    raise AssertionError("zwrt-datad did not start")


def stop(proc):
    if proc.poll() is None:
        proc.kill()
    proc.wait()


def raw_stream(port, path, rcvbuf=None):
    """Open an SSE request on a raw socket; returns the socket, nothing read yet."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if rcvbuf:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
    sock.settimeout(5)
    sock.connect(("127.0.0.1", port))
    sock.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".encode())
    return sock


def status_of(port, path):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return connection, response
    except Exception:
        connection.close()
        raise


class Reader(threading.Thread):
    """Reads an SSE stream continuously; records bytes, last read time and EOF."""

    def __init__(self, port, path):
        super().__init__(daemon=True)
        self.sock = raw_stream(port, path)
        self.sock.settimeout(30)
        self.bytes = 0
        self.last = time.monotonic()
        self.eof = threading.Event()

    def run(self):
        try:
            while True:
                chunk = self.sock.recv(65536)
                if not chunk:
                    break
                self.bytes += len(chunk)
                self.last = time.monotonic()
        except OSError:
            pass
        self.eof.set()


def check_shutdown(with_stalled):
    """with_stalled: also hold a stream whose reader stopped and whose server
    send buffer is full, so the end of the stream can never be delivered and
    only the shutdown deadline (SHUTDOWN_GRACE, 3 s) gets the process out."""
    limit = EXIT_LIMIT_STALLED if with_stalled else EXIT_LIMIT
    with tempfile.TemporaryDirectory(prefix="datad-sse-life-") as name:
        port = free_port()
        extra = {"ZWRT_DATAD_TEST_SNDBUF": "4096"} if with_stalled else None
        proc = start(pathlib.Path(name), port, extra)
        silent = None
        stuck = None
        try:
            reader = Reader(port, "/v2/events")
            reader.start()
            # 只连着、不读、不发：/v2/events。
            silent = raw_stream(port, "/v2/events")
            deadline = time.monotonic() + 5
            while reader.bytes == 0 and time.monotonic() < deadline:
                time.sleep(0.05)
            assert reader.bytes > 0, "/v2/events sent nothing"
            if with_stalled:
                stuck = raw_stream(port, "/v2/events", rcvbuf=1024)
                time.sleep(3)  # 让它的发送缓冲写满（默认写超时 30 s，不会先被断）
            time.sleep(0.5)
            started = time.monotonic()
            proc.send_signal(signal.SIGTERM)
            try:
                code = proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                raise AssertionError("zwrt-datad still running 8 s after SIGTERM")
            took = time.monotonic() - started
            assert code == 0, f"exit code {code} (SIGKILL/crash instead of a clean exit)"
            assert took < limit, f"exit took {took:.2f}s (limit {limit}s)"
            assert reader.eof.wait(3), "/v2/events client did not see the stream end"
            what = "3 SSE clients (one stalled, buffer full)" if with_stalled else "2 SSE clients"
            print(f"shutdown: exited 0 in {took:.2f}s with {what} connected")
        finally:
            for sock in (silent, stuck):
                if sock is not None:
                    close(sock)
            stop(proc)


def check_write_timeout():
    with tempfile.TemporaryDirectory(prefix="datad-sse-life-") as name:
        port = free_port()
        # 服务端发送缓冲压到 4 KB（测试开关），不然回环口上要好几分钟才写满。
        proc = start(
            pathlib.Path(name),
            port,
            {
                "ZWRT_DATAD_WRITE_TIMEOUT_MS": str(WRITE_TIMEOUT_MS),
                "ZWRT_DATAD_TEST_SNDBUF": "4096",
            },
        )
        stalled = []
        try:
            reader = Reader(port, "/v2/events")
            reader.start()
            # 15 个读到一半就不读的客户端：接收缓冲开到最小，一个字节都不读。
            for i in range(15):
                stalled.append(raw_stream(port, "/v2/events", rcvbuf=1024))
            time.sleep(0.5)
            connection, response = status_of(port, "/v2/events")
            assert response.status == 503, response.status
            response.read()
            connection.close()

            started = time.monotonic()
            freed = None
            while time.monotonic() - started < FREE_LIMIT:
                connection, response = status_of(port, "/v2/events")
                if response.status == 200:
                    freed = time.monotonic() - started
                    connection.close()
                    break
                response.read()
                connection.close()
                time.sleep(0.5)
            # 要明显早于 TCP_USER_TIMEOUT（60 s，Linux）：证明是写超时放的名额。
            assert freed is not None, f"no SSE slot was released by a stalled client within {FREE_LIMIT} s"
            # 一直在读的客户端不能被误伤。
            assert not reader.eof.is_set(), "the reading client was dropped"
            assert time.monotonic() - reader.last < 3, "the reading client stopped receiving"

            # 被丢掉的连接对端看得到：读完缓冲里的东西后是 EOF 或 RST。/v2/events 的流在广播落后时
            # 先结束（名额先放），连接要等写超时才关，所以轮着读所有连接，最多等 15 秒。
            closed = set()
            for sock in stalled:
                sock.setblocking(False)
            until = time.monotonic() + 15
            while not closed and time.monotonic() < until:
                for sock in stalled:
                    if sock in closed:
                        continue
                    try:
                        while True:
                            chunk = sock.recv(65536)
                            if not chunk:
                                closed.add(sock)
                                break
                    except (BlockingIOError, socket.timeout):
                        pass
                    except OSError:
                        closed.add(sock)
                time.sleep(0.1)
            dropped = len(closed)
            assert dropped > 0, "no stalled connection was closed by the server"
            print(
                f"write timeout: first slot freed {freed:.1f}s after filling; "
                f"{dropped}/15 stalled connections closed; reading client kept"
            )
        finally:
            for sock in stalled:
                close(sock)
            stop(proc)


def main():
    global BINARY
    BINARY = pathlib.Path(sys.argv[1]).resolve()
    check_shutdown(with_stalled=False)
    check_shutdown(with_stalled=True)
    check_write_timeout()
    print("sse lifecycle: clean exit on SIGTERM with open streams, stalled readers dropped PASS")


if __name__ == "__main__":
    main()
