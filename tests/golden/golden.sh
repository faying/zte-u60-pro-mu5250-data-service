#!/bin/sh
# 旧接口 /state、/events 的 golden（T1 / R11）。
#
#   tests/golden/golden.sh record [BIN]   用 BIN 录 golden（只在行为本来就该变时用）
#   tests/golden/golden.sh check  [BIN]   对照：只允许时间字段不同，其余逐字节相同
#
# 每个情形（正常 + 每个 ubus 对象读失败）起一次 datad（tests/mock_ubus.sh 做设备），
# 抓 /state 响应体和 /events 第一个事件块的原始字节，把时间字段的值换成占位符后存/比。
# 归一化在原始文本上按字段名替换，不重新序列化 JSON。时间字段清单见 normalize.py。
# check 把全部情形跑两遍（T11/R19）：一遍 ZWRT_DATAD_UCI_PARSE=0，uci 全走 mock `uci show`/`uci get`；
# 一遍把 mock 的内容写成 /etc/config 文件（uci_from_mock.py），datad 自己解析，两遍都必须和同一份 golden 一致，
# 并检查解析那一遍对有配置文件的包没有 fork `uci show`（dhcp 故意放一个非空 delta，必须退回 `uci show`）。
# 依赖：sh、curl、python3（和 tests/rust_control_integration.sh 一样）。
# SPDX-License-Identifier: MIT
set -eu

MODE=${1:?usage: golden.sh record|check [BIN]}
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/../.." && pwd)
BIN=${2:-${ZWRT_DATAD_TEST_BIN:-$ROOT/rust/target/debug/zwrt-datad}}
PORT=${GOLDEN_PORT:-19560}
TMP=$(mktemp -d)
PID=
cleanup() {
    [ -z "$PID" ] || { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }
    [ -z "${GOLDEN_KEEP_OUT:-}" ] || { rm -rf "$GOLDEN_KEEP_OUT"; mkdir -p "$GOLDEN_KEEP_OUT"; cp -r "$TMP/show" "$TMP/parse" "$GOLDEN_KEEP_OUT"; }
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

export TZ=UTC LC_ALL=C
export ZWRT_DATAD_UBUS_BIN="$HERE/fail_ubus.sh"
export ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh"
export ZWRT_DATAD_OTA_DISABLE_AUTO=1
export ZWRT_DATAD_MWAN3_INIT=/usr/bin/true
export ZWRT_DATAD_IW_BIN="$ROOT/tests/mock_iw.sh"
export ZWRT_DATAD_HOSTAPD_BIN="$ROOT/tests/mock_hostapd.py"
export ZWRT_DATAD_HOSTAPD_CLI_BIN="$ROOT/tests/mock_hostapd_cli.sh"
export ZWRT_DATAD_SMS_V3E1_URL="http://127.0.0.1:1/goform/goform_set_cmd_process"

# 每个情形都从同一份干净的 fixture 开始（fixture.sh）。
. "$HERE/fixture.sh"

# capture 情形名 [失败对象]：写 $TMP/<show|parse>/<情形>.state.json、<情形>.events.txt
capture() {
    name=$1
    export GOLDEN_FAIL_OBJECT=${2:-}
    export GOLDEN_UBUS_LOG="$TMP/objects.$name.log"
    : >"$GOLDEN_UBUS_LOG"
    setup_fixture "$TMP/fx"
    OUT=$TMP/$UCI_MODE
    # -i 5000：抓取期间不会有第二次采样，/state 和 /events 首条都是启动时那份快照。
    "$BIN" -i 5000 --bind 127.0.0.1 --port "$PORT" --data-dir "$TMP/fx/data" >"$TMP/server.$name.log" 2>&1 &
    PID=$!
    i=0
    while ! curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -lt 600 ] || { tail -n 20 "$TMP/server.$name.log" >&2; echo "golden: $name 起不来" >&2; exit 1; }
        sleep 0.05
    done
    curl -fsS "http://127.0.0.1:$PORT/state" >"$OUT/$name.state.json"
    curl -sN --max-time 1 "http://127.0.0.1:$PORT/events" >"$OUT/$name.events.raw" || true
    # /v2/screen（新接口，不进 golden）：能回、版本对、从同一份快照算出结论
    # HTTP/1.0 + Connection: close, as the touch screen asks (screen_feed.c)
    curl -fsS --http1.0 "http://127.0.0.1:$PORT/v2/screen" >"$TMP/$name.screen.json" ||
        { echo "golden: $name 的 /v2/screen 没回" >&2; exit 1; }
    python3 - "$TMP/$name.screen.json" "$OUT/$name.state.json" "$name" <<'PY' || exit 1
import json, sys
s = json.load(open(sys.argv[1])); st = json.load(open(sys.argv[2]))
assert s["v"] == 1 and s["ts"] == st["ts"], "v/ts"
assert s["net"]["story"]["headline"], "empty headline"
if sys.argv[3] == "normal":
    assert s["net"]["story"]["headline"] == "顺畅", s["net"]["story"]
PY
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    PID=
    python3 "$HERE/normalize.py" first-event <"$OUT/$name.events.raw" >"$OUT/$name.events.txt"
    rm -f "$OUT/$name.events.raw"
    python3 "$HERE/normalize.py" text <"$OUT/$name.state.json" >"$OUT/$name.state.norm" &&
        mv "$OUT/$name.state.norm" "$OUT/$name.state.json"
    # /events 首条必须是完整快照：事件名 state，data 与 /state 相同（归一化后）
    python3 "$HERE/normalize.py" same-as-state "$OUT/$name.state.json" <"$OUT/$name.events.txt" ||
        { echo "golden: $name 的 /events 首条不是完整快照 state 事件" >&2; exit 1; }
    [ "$UCI_MODE" = parse ] && check_parse_path "$name"
    return 0
}

# 解析那一遍：有配置文件、delta 为空的包不许 fork uci；dhcp（非空 delta）必须 fork。
check_parse_path() {
    log=$TMP/fx/calls.log
    grep -q . "$TMP/fx/parsed-packages" || { echo "golden: $1 没生成任何配置文件" >&2; exit 1; }
    while IFS= read -r pkg; do
        [ "$pkg" = dhcp ] && continue
        if grep -q -e "^uci	-q show $pkg\$" -e "^uci	-q get $pkg\." "$log" 2>/dev/null; then
            echo "golden: $1 解析路径没生效：$pkg 仍 fork 了 uci" >&2; exit 1
        fi
    done <"$TMP/fx/parsed-packages"
    grep -q "^uci	-q show dhcp\$" "$log" || { echo "golden: $1 dhcp 有未保存改动却没退回 uci show" >&2; exit 1; }
}

mkdir -p "$TMP/show" "$TMP/parse"
[ -x "$BIN" ] || { echo "golden: 找不到 $BIN" >&2; exit 1; }

run_all() {
UCI_MODE=$1
case "$MODE" in
    record)
        capture normal
        # 对象清单来自正常情形启动时实际读到的 ubus 对象（去重、排序）
        sort -u "$TMP/objects.normal.log" >"$HERE/objects.txt"
        ;;
    check)
        capture normal
        sort -u "$TMP/objects.normal.log" | cmp -s - "$HERE/objects.txt" ||
            { echo "golden: 启动时读的 ubus 对象清单变了（见 objects.txt）" >&2; exit 1; }
        ;;
    *) echo "usage: golden.sh record|check [BIN]" >&2; exit 64 ;;
esac

while IFS= read -r obj; do
    [ -n "$obj" ] || continue
    capture "fail-$obj" "$obj"
done <"$HERE/objects.txt"
}

run_all show
[ "$MODE" = record ] || run_all parse

if [ "$MODE" = record ]; then
    rm -f "$HERE"/*.state.json "$HERE"/*.events.txt
    cp "$TMP/show/"* "$HERE/"
    echo "golden: 已录 $(ls "$TMP/show" | grep -c '\.state\.json$') 个情形 → $HERE"
    exit 0
fi

fail=0
for mode in show parse; do
for f in "$HERE"/*.state.json "$HERE"/*.events.txt; do
    b=$(basename "$f")
    if [ ! -f "$TMP/$mode/$b" ]; then
        echo "golden($mode): 缺少 $b" >&2; fail=1
    elif ! cmp -s "$f" "$TMP/$mode/$b"; then
        echo "golden($mode): $b 不一致（时间字段之外有差异）" >&2
        diff "$f" "$TMP/$mode/$b" | head -n 20 | cut -c1-400 >&2 || true
        fail=1
    fi
done
for f in "$TMP/$mode/"*; do
    [ -f "$HERE/$(basename "$f")" ] || { echo "golden($mode): 多出情形 $(basename "$f")（ubus 对象清单变了？）" >&2; fail=1; }
done
done
[ "$fail" = 0 ] || exit 1
echo "golden: $(ls "$TMP/show" | grep -c '\.state\.json$') 个情形与 golden 一致（uci show 路径和解析路径各一遍）"
