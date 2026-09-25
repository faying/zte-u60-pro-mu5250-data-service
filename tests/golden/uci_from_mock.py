#!/usr/bin/env python3
"""把 tests/mock_uci.sh 的 `uci -q show` / `uci -q get` 输出写成 /etc/config 格式（golden 的解析路径用）。

    uci_from_mock.py <mock_uci.sh> <输出目录>

golden.sh 的 parse 一遍用它：datad 自己解析这些文件（R19），结果必须和 mock `uci show` 那一遍逐字节相同。
mock 读失败的包不写文件（datad 会退回 mock，结果同样相同）。按当前环境变量调用 mock，所以每个情形都要重跑。
打印写了文件的包名，每行一个。
SPDX-License-Identifier: MIT
"""
import os
import re
import subprocess
import sys

SHOW = ["zwrt_zte_mdm", "zwrt_common_info", "network", "dhcp", "zwrt_data_commit", "system",
        "zwrt_web", "zwrt_tr069", "zwrt_router", "zte_nwinfo", "wireless", "mwan3"]
# 采集里单独 `uci get` 的路径（state.rs 的 uci_value）
GETS = ["zwrt_router.network.opms_wan_mode", "zwrt_router.icgmwan.IcgDevId",
        "zwrt_router.icgmwan.residual_flow", "zwrt_router.icgmwan.count_flow_today",
        "zwrt_deviceui.Device.fan_switch_status",
        "zwrt_deviceui.Device.liquid_cooling_switch_status"]


def mock(bin_, *args):
    env = dict(os.environ)
    env.pop("MOCK_CALL_LOG", None)
    r = subprocess.run([bin_, *args], capture_output=True, env=env)
    return r.returncode, r.stdout.decode()


def items(raw):
    """`'a' 'b'` → ['a', 'b']；不带引号的原样一项。"""
    if not raw.startswith("'"):
        return [raw]
    out = re.findall(r"'((?:[^']|'\\'')*)'", raw)
    return [x.replace("'\\''", "'") for x in out]


def q(v):
    return "'" + v.replace("'", "'\\''") + "'"


def main():
    bin_, outdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)
    pkgs = {}  # 包 → {节: (类型, [(选项, [值...])])}
    for p in SHOW:
        code, text = mock(bin_, "-q", "show", p)
        if code != 0:
            continue
        secs = pkgs.setdefault(p, {})
        for line in text.splitlines():
            key, sep, raw = line.partition("=")
            parts = key.split(".")
            if not sep or len(parts) not in (2, 3) or parts[0] != p:
                continue
            sec = secs.setdefault(parts[1], [parts[1], []])
            if len(parts) == 2:
                sec[0] = raw
            else:
                sec[1].append((parts[2], items(raw)))
    for path in GETS:
        code, text = mock(bin_, "-q", "get", path)
        if code != 0:
            continue
        p, s, o = path.split(".")
        pkgs.setdefault(p, {}).setdefault(s, [s, []])[1].append((o, [text.rstrip("\n")]))
    for p, secs in pkgs.items():
        lines = []
        for name, (stype, opts) in secs.items():
            lines.append(f"config {stype} {q(name)}")
            for o, vals in opts:
                if len(vals) == 1:
                    lines.append(f"\toption {o} {q(vals[0])}")
                else:
                    lines.extend(f"\tlist {o} {q(v)}" for v in vals)
            lines.append("")
        with open(os.path.join(outdir, p), "w", encoding="utf-8") as f:
            f.write("\n".join(lines))
        print(p)


if __name__ == "__main__":
    main()
