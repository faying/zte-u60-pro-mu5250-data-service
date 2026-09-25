#!/usr/bin/env python3
"""SSE client limit: 16 concurrent /events streams, the 17th gets 503, and a
closed stream frees its slot. /v2/events shares the same 16 slots
(docs/STATE_V2.md section 3) and starts with a snapshot event.

Usage: rust_sse_limit_test.py PATH_TO_ZWRT_DATAD   (starts it against the mocks)
       rust_sse_limit_test.py PORT                 (tests an already running instance)
"""
import http.client
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def connect(port, path="/events"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    connection.request("GET", path)
    return connection, connection.getresponse()


def check(port):
    clients = []
    try:
        for i in range(16):
            path = "/events" if i < 8 else "/v2/events"
            connection, response = connect(port, path)
            assert response.status == 200, (path, response.status)
            if path == "/v2/events":
                assert response.getheader("content-type") == "text/event-stream"
                assert response.readline() == b"event: snapshot\n"
            clients.append((connection, response))
        for path in ("/events", "/v2/events"):
            rejected, response = connect(port, path)
            assert response.status == 503, (path, response.status)
            response.read()
            rejected.close()

        # 关一个 /events、再关一个 /v2/events：两种连接都要把名额还回来。
        # 新连接一直占着名额，所以第二次必须是刚关掉的那个 /v2 连接腾出来的。
        for closing in (clients.pop(0), clients.pop()):
            closing[0].close()
            clients.append(wait_slot(port))
    finally:
        for connection, _ in clients:
            connection.close()


def wait_slot(port):
    deadline = time.monotonic() + 8
    while True:
        connection, response = connect(port, "/v2/events")
        if response.status == 200:
            return connection, response
        response.read()
        connection.close()
        assert response.status == 503, response.status
        if time.monotonic() >= deadline:
            raise AssertionError("SSE slot was not released after disconnect")
        time.sleep(0.2)


def wait_ready(port, proc):
    for _ in range(100):
        if proc.poll() is not None:
            raise AssertionError(f"zwrt-datad exited early: {proc.returncode}")
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            connection.request("GET", "/state")
            response = connection.getresponse()
            response.read()
            connection.close()
            if response.status == 200:
                return
        except OSError:
            pass
        time.sleep(0.1)
    raise AssertionError("zwrt-datad did not start")


def run_binary(binary):
    with tempfile.TemporaryDirectory(prefix="datad-sse-limit-") as name:
        base = pathlib.Path(name)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = dict(
            os.environ,
            ZWRT_DATAD_DIR=str(base / "data"),
            ZWRT_DATAD_UBUS_BIN=str(ROOT / "tests/mock_ubus.sh"),
            ZWRT_DATAD_UCI_BIN=str(ROOT / "tests/mock_uci.sh"),
            ZWRT_DATAD_OTA_DISABLE_AUTO="1",
            ZWRT_DATAD_MWAN3_INIT="/usr/bin/true",
            MOCK_CALL_LOG=str(base / "calls.log"),
            MOCK_UCI_STATE_DIR=str(base / "uci-state"),
        )
        proc = subprocess.Popen(
            [str(binary), "-i", "500", "-p", str(port)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_ready(port, proc)
            check(port)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


def main():
    target = sys.argv[1]
    if target.isdigit():
        check(int(target))
    else:
        run_binary(pathlib.Path(target).resolve())
    print("sse limit: 16 streams (/events + /v2/events) accepted, 17th rejected with 503, slot released PASS")


if __name__ == "__main__":
    main()
