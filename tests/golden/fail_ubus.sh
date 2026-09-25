#!/bin/sh
# ubus 包装：GOLDEN_FAIL_OBJECT 指定的 ubus 对象一律读失败（exit 1），
# 其余转给 tests/mock_ubus.sh。GOLDEN_UBUS_LOG 记下每次调用的对象名。
set -eu
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# `ubus listen`（V2-31 的短信事件订阅）不是读取，不记对象、不按对象失败，直接交给 mock。
[ "${1:-}" = listen ] && exec "$HERE/../mock_ubus.sh" "$@"
service=
skip=0
for arg in "$@"; do
    if [ "$skip" = 1 ]; then skip=0; continue; fi
    case "$arg" in
        -t) skip=1 ;;
        -v | list | call) ;;
        *) service=$arg; break ;;
    esac
done
if [ -n "$service" ] && [ -n "${GOLDEN_UBUS_LOG:-}" ]; then
    printf '%s\n' "$service" >>"$GOLDEN_UBUS_LOG"
fi
if [ -n "$service" ] && [ "$service" = "${GOLDEN_FAIL_OBJECT:-}" ]; then
    exit 1
fi
exec "$HERE/../mock_ubus.sh" "$@"
