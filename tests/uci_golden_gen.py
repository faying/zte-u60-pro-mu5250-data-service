#!/usr/bin/env python3
"""用真 uci 生成 tests/uci_cases.expected（rust/src/uci.rs 的逐行对照金标准）。

    python3 tests/uci_golden_gen.py

在 Docker 里（alpine，按 digest 固定）从源码编 libubox + uci，版本与 U60 Pro（MU5250）设备上的
uci 2023-08-10-5781664d 相同，然后对 tests/uci_cases.txt 里的每个样例跑
`uci -c <dir> -t <空 savedir> show <包>`，以及对输出里每个 `包.节.选项` 跑 `uci get`。

输出格式：若干以 NUL 结尾的字段，
    case <包名> <退出码> <show 输出>  然后若干  get <路径> <退出码> <get 输出>  最后 end
SPDX-License-Identifier: MIT
"""
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
IMAGE = "alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6"
LIBUBOX = "75a3b870cace1171faf57bd55e5a9a2f1564f757"  # 2023-05-23，OpenWrt 23.05 用的那版
UCI = "5781664d5087ccc4b5ab58505883231212dbedbc"  # 2023-08-10，设备 uci 版本


def cases(path):
    """(名字, 内容字节)。和 uci.rs 测试里的 split_cases 规则相同。"""
    out, name, flags, lines = [], None, [], []

    def flush():
        if name is None:
            return
        data = "".join(lines)
        if "nonl" in flags and data.endswith("\n"):
            data = data[:-1]
        if "crlf" in flags:
            data = data.replace("\n", "\r\n")
        out.append((name, data.encode()))

    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("=== "):
                flush()
                parts = line[4:].split()
                name, flags, lines = parts[0], parts[1:], []
            elif name is not None:
                lines.append(line)
    flush()
    return out


SCRIPT = r"""
set -e
apk add -q --no-progress build-base cmake git json-c-dev >/dev/null
cd /tmp
git clone -q https://github.com/openwrt/libubox && git -C libubox checkout -q {libubox}
git clone -q https://github.com/openwrt/uci && git -C uci checkout -q {uci}
(cd libubox && cmake -S . -B b -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_LUA=OFF -DBUILD_EXAMPLES=OFF -DCMAKE_INSTALL_PREFIX=/usr >/dev/null && make -C b -s install >/dev/null)
(cd uci && cmake -S . -B b -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DBUILD_LUA=OFF -DCMAKE_INSTALL_PREFIX=/usr >/dev/null && make -C b -s install >/dev/null)
mkdir -p /tmp/save
cd /work/conf
for p in *; do
    set +e
    uci -c /work/conf -t /tmp/save show "$p" >/work/out/$p.show 2>/dev/null
    echo $? >/work/out/$p.code
    : >/work/out/$p.gets
    i=0
    cut -d= -f1 /work/out/$p.show | grep -E '^[^.]+\.[^.]+\.[^.]+$' | while IFS= read -r path; do
        i=$((i + 1))
        printf '%s\n' "$path" >/work/out/$p.get.$i.path
        uci -c /work/conf -t /tmp/save get "$path" >/work/out/$p.get.$i.out 2>/dev/null
        echo $? >/work/out/$p.get.$i.code
    done
    set -e
done
"""


def main():
    all_cases = cases(os.path.join(HERE, "uci_cases.txt"))
    with tempfile.TemporaryDirectory() as tmp:
        os.makedirs(os.path.join(tmp, "conf"))
        os.makedirs(os.path.join(tmp, "out"))
        for name, data in all_cases:
            with open(os.path.join(tmp, "conf", name), "wb") as f:
                f.write(data)
        subprocess.run(
            ["docker", "run", "--rm", "-v", f"{tmp}:/work", IMAGE, "sh", "-c",
             SCRIPT.format(libubox=LIBUBOX, uci=UCI)],
            check=True,
        )
        out = bytearray()

        def field(b):
            assert b"\0" not in b
            out.extend(b + b"\0")

        def read(p):
            with open(os.path.join(tmp, "out", p), "rb") as f:
                return f.read()

        for name, _ in all_cases:
            field(b"case")
            field(name.encode())
            field(read(f"{name}.code").strip())
            field(read(f"{name}.show"))
            i = 1
            while os.path.exists(os.path.join(tmp, "out", f"{name}.get.{i}.path")):
                field(b"get")
                field(read(f"{name}.get.{i}.path").rstrip(b"\n"))
                field(read(f"{name}.get.{i}.code").strip())
                field(read(f"{name}.get.{i}.out"))
                i += 1
        field(b"end")
    with open(os.path.join(HERE, "uci_cases.expected"), "wb") as f:
        f.write(out)
    print(f"uci_golden_gen: {len(all_cases)} 个样例 → tests/uci_cases.expected", file=sys.stderr)


if __name__ == "__main__":
    main()
