#!/usr/bin/env python3
"""Legacy access counter (u60-platform item 3): /state, /events and each
/control action are counted at GET /debug/legacy-hits, /events streams are
also counted while open, and the counts survive a restart (legacy-hits.json
in the data dir).

Usage: rust_legacy_hits_test.py PATH_TO_ZWRT_DATAD
"""
import http.client
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def request(port, method, path, body=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    headers = {"content-type": "application/json"} if body is not None else {}
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    data = response.read()
    connection.close()
    return response.status, data


def hits(port):
    status, data = request(port, "GET", "/debug/legacy-hits")
    assert status == 200, status
    return json.loads(data)


def wait_ready(port, proc):
    for _ in range(100):
        if proc.poll() is not None:
            raise AssertionError(f"zwrt-datad exited early: {proc.returncode}")
        try:
            if request(port, "GET", "/healthz")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(0.1)
    raise AssertionError("zwrt-datad did not start")


def start(binary, base, port):
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
    wait_ready(port, proc)
    return proc


def stop(proc):
    proc.terminate()
    try:
        proc.wait(timeout=8)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def main():
    binary = pathlib.Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="datad-legacy-hits-") as name:
        base = pathlib.Path(name)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        proc = start(binary, base, port)
        try:
            first = hits(port)
            assert first["since"] > 0 and first["hits"] == {}, first
            assert (base / "data/legacy-hits.json").exists()
            # /v2 不算旧接口。
            assert request(port, "GET", "/v2/state")[0] == 200
            for _ in range(2):
                assert request(port, "GET", "/state")[0] == 200
            request(port, "POST", "/control", json.dumps({"action": "wifi.status"}))
            request(port, "POST", "/control", json.dumps({"action": "wifi.status", "source": "test"}))
            stream = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            stream.request("GET", "/events")
            assert stream.getresponse().status == 200
            now = hits(port)
            assert now["open_events"] == 1, now
            got = {k: v["count"] for k, v in now["hits"].items()}
            assert got == {
                "/state": 2,
                "/events": 1,
                "control:wifi.status": 1,
                "control:wifi.status+source": 1,
            }, got
            # 本机调用者能查到进程（只在有 /proc 的系统上）。
            if pathlib.Path("/proc/net/tcp").exists():
                time.sleep(0.5)
                callers = hits(port)["hits"]["/state"]["callers"]
                assert callers and callers[0].startswith("python"), callers
            stream.close()
            for _ in range(50):
                if hits(port)["open_events"] == 0:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError("open_events did not drop after close")
        finally:
            stop(proc)
        # 重启后接着数，since 不变。
        proc = start(binary, base, port)
        try:
            again = hits(port)
            assert again["since"] == first["since"], (again, first)
            assert again["hits"]["/state"]["count"] == 2, again
        finally:
            stop(proc)
    print("legacy hits: /state, /events, /control per action counted, open streams tracked, kept across restart PASS")


if __name__ == "__main__":
    main()
