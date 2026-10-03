#!/usr/bin/env python3
"""旧触屏依赖的 /control 契约（E4 T1，D26），由 control_golden.sh 在 golden 比完后调用。

  control_contract.py PORT FAIL_FILE WRITE_LOCK DATA_OFF_FILE OPS_DIR

请求按触屏 data.c control_send 的原样发（HTTP/1.1 + Connection: close，写完不关，等回复）：
1. 挂起到回复：动作做完之前一个字节都不回，做完回 200 和结果。
2. 队列满：不在事务描述表里的动作多出来的请求立即回 503，回复体逐字节固定（触屏 ui_control_should_fallback 只认 503）。
3. state.set_interval：回复之后采样间隔真的变了（/state 的 ts 在新间隔内前进）。

E4 有意改变的行为（write-op-layer.md「旧客户端」、D14，T2），各一条：
4. 描述表里的动作（network.set_mode）的旧请求在执行者队列满时不回 503：回和今天一样的成功、排队执行。
5. 事务进行中（锁被占）：旧请求「关数据」马上生效、回复逐字节同锁空时；进行中的事务记 preempted。
6. 事务进行中：同一项的旧请求当覆盖写，回复逐字节同今天；别的来源的新写回 409 busy，说清谁在做什么。
7. 「立即退回」：退回写下去并确认，终态 rolled_back/user_revert；数据开着、蜂窝接口探测不通时不确认（T4）。
8. 跨进程写锁（D29）：别人（应急直写脚本）拿着 flock 时，datad 的写等它放；只读的不等。
9. 流水账（T5）：旧请求直接执行的写记一行、密码不落盘；journal.append 的 skipped 合并；journal.list 新的在前。
SPDX-License-Identifier: MIT
"""
import fcntl
import json
import os
import select
import socket
import sys
import threading
import time
import urllib.request

PORT = int(sys.argv[1])
FAIL_FILE = sys.argv[2]
WRITE_LOCK = sys.argv[3]
DATA_OFF = sys.argv[4]
OPS_DIR = sys.argv[5]
SET_MODE = '{"action":"network.set_mode","params":{"mode":"WL_AND_5G"}}'
BAND = '{"action":"band.set_lte","params":{"bands":"1,3"}}'
BUSY_BODY = b'{"action":"band.set_lte","error":{"code":"busy","message":"control queue full"},"ok":false}'
LEGACY_OK = b'{"action":"network.set_mode","ok":true,"result":{"result":"success"}}'


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


def delay(seconds: float, method: str = "nwinfo_set_netselect") -> None:
    with open(FAIL_FILE, "w") as f:
        f.write("delay zte_nwinfo_api %s %s\n" % (method, seconds))


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


def parallel(body: str, n: int):
    results = [None] * n
    socks = [send(body) for _ in results]

    def collect(i):
        try:
            results[i] = split(read_all(socks[i], 20))
        except OSError as e:
            results[i] = (b"error", str(e).encode())

    threads = [threading.Thread(target=collect, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return results


def queue_full() -> None:
    delay(0.4, "nwinfo_set_lte_ext_band")
    results = parallel(BAND, 12)
    busy = [r for r in results if r[0].startswith(b"HTTP/1.1 503")]
    if not busy:
        fail("12 个并发请求一个 503 都没有：%r" % [r[0] for r in results])
    for status, body in results:
        if status.startswith(b"HTTP/1.1 503"):
            if body != BUSY_BODY:
                fail("503 回复体变了：%r" % body)
        elif status != b"HTTP/1.1 200 OK":
            fail("队列没满的请求没回 200：%r %r" % (status, body))


def legacy_described_never_503() -> None:
    """E4 有意改变（4）：描述表里的动作，旧请求在执行者队列满时回成功、排队，不回 503。"""
    delay(0.4)
    for status, body in parallel(SET_MODE, 12):
        if status != b"HTTP/1.1 200 OK" or body != LEGACY_OK:
            fail("队列满时旧的 network.set_mode 回复不对：%r %r" % (status, body))
    clear()
    time.sleep(2)


def post(body: dict):
    status, raw = split(read_all(send(json.dumps(body, separators=(",", ":"))), 15))
    return status, raw


def op_status(op_id: str) -> dict:
    status, raw = post({"action": "op.status", "params": {"op_id": op_id}})
    if status != b"HTTP/1.1 200 OK":
        fail("op.status 没回 200：%r" % raw)
    return json.loads(raw)["result"]


def take_lock() -> str:
    """新客户端把网络模式切到 mock 永远不会报的值：事务停在 verifying，锁一直被占着。"""
    status, raw = post({"action": "network.set_mode", "source": "screen", "params": {"mode": "Only_LTE"}})
    reply = json.loads(raw)
    if status != b"HTTP/1.1 200 OK" or reply["op"]["phase"] != "verifying" or reply["op"]["old"] != "WL_AND_5G":
        fail("新客户端切网络模式的回复不对：%r %r" % (status, raw))
    return reply["op"]["op_id"]


def wait_final(op_id: str) -> dict:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        s = op_status(op_id)
        if s["phase"] not in ("accepted", "applying", "verifying", "rolling_back"):
            return s
        time.sleep(0.2)
    fail("%s 10 秒没结束：%r" % (op_id, op_status(op_id)))


def legacy_data_off_preempts() -> None:
    """E4 有意改变（5，D14）。"""
    data_off = '{"action":"cellular.set","params":{"enabled":0}}'
    free = split(read_all(send(data_off), 10))
    op_id = take_lock()
    held = split(read_all(send(data_off), 10))
    if held != free:
        fail("锁被占时旧请求关数据的回复变了：%r != %r" % (held, free))
    s = op_status(op_id)
    if (s["phase"], s["reason"]) != ("cancelled", "preempted"):
        fail("关数据没有打断进行中的事务：%r" % s)


def legacy_same_item_and_busy() -> None:
    """E4 有意改变（6）。"""
    op_id = take_lock()
    status, raw = post({"action": "network.set_mode", "source": "scenario", "params": {"mode": "Only_5G"}})
    reply = json.loads(raw)
    if status != b"HTTP/1.1 409 Conflict" or reply["error"]["code"] != "busy" or reply["doing"]["source"] != "screen" or reply["doing"]["op_id"] != op_id:
        fail("锁被占时别的来源没收到 busy：%r %r" % (status, raw))
    status, body = split(read_all(send(SET_MODE), 10))
    if status != b"HTTP/1.1 200 OK" or body != LEGACY_OK:
        fail("同一项的旧请求回复变了：%r %r" % (status, body))
    s = op_status(op_id)
    if (s["phase"], s["reason"]) != ("cancelled", "superseded"):
        fail("同一项的旧请求没有覆盖进行中的事务：%r" % s)


def user_revert() -> None:
    """E4（7）：mock 的读回永远是 WL_AND_5G，所以退回一写读回就对上了。
    数据开着时还要在蜂窝接口上探测通，mock 的接口 fixture0 不存在、探测永远失败，退回确认不了；
    把数据关掉（不应当有数据）才按读回 + 注册确认，也不发探测。"""
    op_id = take_lock()
    time.sleep(1)
    s = op_status(op_id)
    if s["phase"] != "verifying" or s["data_ok"] is not False:
        fail("数据开着、探测不通时不该确认：%r" % s)
    open(DATA_OFF, "w").close()
    try:
        status, raw = post({"action": "op.revert", "params": {"op_id": op_id}})
        if status != b"HTTP/1.1 200 OK" or json.loads(raw)["result"]["phase"] != "rolling_back":
            fail("op.revert 回复不对：%r %r" % (status, raw))
        s = wait_final(op_id)
    finally:
        os.remove(DATA_OFF)
    if (s["phase"], s["reason"]) != ("rolled_back", "user_revert") or s["data_ok"] is not True:
        fail("立即退回的终态不对：%r" % s)
    status, raw = post({"action": "op.revert", "params": {"op_id": op_id}})
    if status != b"HTTP/1.1 409 Conflict":
        fail("已结束的事务还能退回：%r %r" % (status, raw))


def write_lock_is_shared() -> None:
    """E4（8）：像 u60-fallback.sh 那样拿着 flock 2 秒。"""
    fd = os.open(WRITE_LOCK, os.O_CREAT | os.O_WRONLY, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        start = time.monotonic()
        read = threading.Thread(target=lambda: post({"action": "op.status", "params": {}}))
        write_result = []
        write = threading.Thread(target=lambda: write_result.append(post({"action": "band.set_lte", "params": {"bands": "1,3"}})))
        write.start()
        read.start()
        read.join(5)
        if read.is_alive() or time.monotonic() - start > 1.0:
            fail("只读请求也被写锁挡住了")
        time.sleep(2 - (time.monotonic() - start))
        if write_result:
            fail("别人拿着写锁时 datad 照样写了：%r" % write_result)
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)
    write.join(10)
    if not write_result or write_result[0][0] != b"HTTP/1.1 200 OK":
        fail("放锁以后写没做完：%r" % write_result)


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


def journal() -> None:
    # 前面用例的事务都结束了再看，免得中途插进一行事务结束
    s = json.loads(post({"action": "op.status"})[1])["result"]
    if s and s.get("phase") in ("accepted", "applying", "verifying", "rolling_back"):
        wait_final(s["op_id"])
    secret = "Contract-Secret-9917"
    status, _ = post({"action": "wifi.configure", "params": {"section": "main_2g", "ssid": "Golden", "key": secret}})
    if status != b"HTTP/1.1 200 OK":
        fail("wifi.configure 没成功：%r" % status)
    for _ in range(100):
        status, raw = post({"action": "journal.append", "source": "scenario",
                            "params": {"item": "network.mode", "result": "skipped", "reason": "user_hold"}})
        if status != b"HTTP/1.1 200 OK":
            fail("journal.append 回复不对：%r %r" % (status, raw))
    status, raw = post({"action": "journal.append", "params": {"item": "esim", "result": "ok"}})
    if status != b"HTTP/1.1 400 Bad Request":
        fail("没有 source 的 journal.append 应该 400：%r %r" % (status, raw))
    post({"action": "journal.append", "source": "web",
          "params": {"action": "esim.switch", "result": "ok", "confirm_code": secret}})
    status, raw = post({"action": "journal.list", "params": {"limit": 3}})
    entries = json.loads(raw)["result"]["entries"]
    got = [(e.get("source"), e.get("action") or e.get("item"), e.get("result"), e.get("skip")) for e in entries]
    want = [("web", "esim.switch", "ok", None),
            ("scenario", "network.mode", "skipped", "start"),
            ("legacy", "wifi.configure", "ok", None)]
    if got != want:
        fail("journal.list 不对：%r" % entries)
    with open(os.path.join(OPS_DIR, "journal.jsonl"), encoding="utf-8") as f:
        if secret in f.read():
            fail("密码写进了流水账")


def main() -> int:
    try:
        hang_until_done()
        queue_full()
        legacy_described_never_503()
        legacy_data_off_preempts()
        legacy_same_item_and_busy()
        user_revert()
        write_lock_is_shared()
        set_interval_applies()
        journal()
    finally:
        clear()
    return 0


if __name__ == "__main__":
    sys.exit(main())
