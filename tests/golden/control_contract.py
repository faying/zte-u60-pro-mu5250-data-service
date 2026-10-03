#!/usr/bin/env python3
"""旧触屏依赖的 /control 契约（E4 T1，D26），由 control_golden.sh 在 golden 比完后调用。

  control_contract.py PORT FAIL_FILE

请求按触屏 data.c control_send 的原样发（HTTP/1.1 + Connection: close，写完不关，等回复）：
1. 挂起到回复：动作做完之前一个字节都不回，做完回 200 和结果。
2. 队列满：多出来的请求立即回 503，回复体逐字节固定（触屏 ui_control_should_fallback 只认 503）。
3. state.set_interval：回复之后采样间隔真的变了（/state 的 ts 在新间隔内前进）。
SPDX-License-Identifier: MIT
"""
import json
import select
import socket
import sys
import threading
import time
import urllib.request

PORT = int(sys.argv[1])
FAIL_FILE = sys.argv[2]
SET_MODE = '{"action":"network.set_mode","params":{"mode":"WL_AND_5G"}}'
BUSY_BODY = b'{"action":"network.set_mode","error":{"code":"busy","message":"control queue full"},"ok":false}'


def request(body: str) -> bytes:
    return (
        "POST /control HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n"
        "Content-Type: application/json\r\nContent-Length: %d\r\n"
        "Connection: close\r\n\r\n%s" % (PORT, len(body), body)
    ).encode()


def send(body: str) -> socket.socket:
    s = socket.create_connection(("127.0.0.1", PORT), timeout=2)
    s.sendall(request(body))
    return s


def read_all(s: socket.socket, timeout: float) -> bytes:
    s.settimeout(timeout)
    chunks = []
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    s.close()
    return b"".join(chunks)


def split(raw: bytes):
    head, _, body = raw.partition(b"\r\n\r\n")
    return head.split(b"\r\n", 1)[0], body


def delay(seconds: float) -> None:
    with open(FAIL_FILE, "w") as f:
        f.write("delay zte_nwinfo_api nwinfo_set_netselect %s\n" % seconds)


def clear() -> None:
    open(FAIL_FILE, "w").close()


def fail(msg: str) -> None:
    sys.stderr.write("control contract: %s\n" % msg)
    sys.exit(1)


def hang_until_done() -> None:
    delay(1.5)
    s = send(SET_MODE)
    ready, _, _ = select.select([s], [], [], 1.0)
    if ready:
        fail("动作还没做完就回了字节（旧触屏会当成已经回复）")
    status, body = split(read_all(s, 10))
    if status != b"HTTP/1.1 200 OK":
        fail("挂起后的回复状态不对：%r" % status)
    reply = json.loads(body)
    if reply.get("ok") is not True or reply.get("action") != "network.set_mode":
        fail("挂起后的回复体不对：%r" % body)


def queue_full() -> None:
    delay(0.4)
    results = [None] * 12
    socks = [send(SET_MODE) for _ in results]

    def collect(i):
        try:
            results[i] = split(read_all(socks[i], 20))
        except OSError as e:
            results[i] = (b"error", str(e).encode())

    threads = [threading.Thread(target=collect, args=(i,)) for i in range(len(socks))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    busy = [r for r in results if r[0].startswith(b"HTTP/1.1 503")]
    if not busy:
        fail("12 个并发请求一个 503 都没有：%r" % [r[0] for r in results])
    for status, body in results:
        if status.startswith(b"HTTP/1.1 503"):
            if body != BUSY_BODY:
                fail("503 回复体变了：%r" % body)
        elif status != b"HTTP/1.1 200 OK":
            fail("队列没满的请求没回 200：%r %r" % (status, body))


def state_ts() -> float:
    with urllib.request.urlopen("http://127.0.0.1:%d/state" % PORT, timeout=5) as r:
        return json.load(r)["ts"]


def set_interval_applies() -> None:
    clear()
    status, body = split(read_all(send('{"action":"state.set_interval","params":{"milliseconds":500}}'), 10))
    if status != b"HTTP/1.1 200 OK":
        fail("state.set_interval 没回 200：%r" % body)
    start = state_ts()
    deadline = time.monotonic() + 3.0
    while state_ts() == start:
        if time.monotonic() > deadline:
            fail("state.set_interval 500 之后 3 秒 /state 没更新")
        time.sleep(0.1)
    read_all(send('{"action":"state.set_interval","params":{"milliseconds":5000}}'), 10)


def main() -> int:
    try:
        hang_until_done()
        queue_full()
        set_interval_applies()
    finally:
        clear()
    return 0


if __name__ == "__main__":
    sys.exit(main())
