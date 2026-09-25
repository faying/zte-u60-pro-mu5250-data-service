#!/usr/bin/env python3
"""golden 归一化：只把时间字段的值换成占位符，其余字节原样保留。

  normalize.py text                 stdin 的 JSON 文本 → stdout
  normalize.py first-event          stdin 的 SSE 流 → 第一个事件块（到第一个空行），data 已归一化
  normalize.py same-as-state FILE   stdin 的事件块必须是 `event: state` 且 data 与 FILE 逐字节相同

不 json.loads/dumps（会抹掉浮点格式、转义、空白差异），按字段名在原始文本上替换。
SPDX-License-Identifier: MIT
"""
import re
import sys

# 时间字段：值取决于“什么时候跑”，允许不同。只放真正的时间值，别拿它掩盖别的抖动。
TIME_KEYS = [
    "ts",  # 快照时间戳（SystemTime::now）
]

_PATTERN = re.compile(
    r'("(?:' + "|".join(re.escape(k) for k in TIME_KEYS) + r')":)(-?[0-9]+(?:\.[0-9]+)?|"[^"]*"|null)'
)


def normalize(text: str) -> str:
    return _PATTERN.sub(lambda m: m.group(1) + '"<time>"', text)


def main() -> int:
    mode = sys.argv[1]
    raw = sys.stdin.buffer.read().decode("utf-8")
    if mode == "text":
        sys.stdout.write(normalize(raw))
        return 0
    if mode == "first-event":
        raw = raw.replace("\r\n", "\n")
        end = raw.find("\n\n")
        if end < 0:
            sys.stderr.write("normalize: /events 没有完整的事件块\n")
            return 1
        sys.stdout.write(normalize(raw[: end + 2]))
        return 0
    if mode == "same-as-state":
        state = open(sys.argv[2], encoding="utf-8").read()
        lines = [l for l in raw.split("\n") if l and not l.startswith(":") and not l.startswith("retry:")]
        events = [l[len("event:"):].strip() for l in lines if l.startswith("event:")]
        data = "\n".join(l[len("data:"):].lstrip(" ") for l in lines if l.startswith("data:"))
        return 0 if events == ["state"] and data == state else 1
    sys.stderr.write("normalize: unknown mode\n")
    return 64


if __name__ == "__main__":
    sys.exit(main())
