#!/bin/sh
# 旧接口 /control 回复的 golden（E4 T1，write-op-layer.md E-C1/D26）。
#
#   tests/golden/control_golden.sh record [BIN]   用 BIN 录 golden（只在行为本来就该变时用）
#   tests/golden/control_golden.sh check  [BIN]   对照：HTTP 状态码和回复体逐字节相同
#
# 起一个 datad（tests/mock_ubus.sh 做设备，fixture 同 golden.sh），按 control_cases.txt 的顺序
# 发每个 /control 请求：每个动作的成功回复、参数不对的回复、ubus 调用失败时的回复，外加请求层面的错误。
# 旧触屏只靠这些回复工作（data.c 的 control_reap 看状态码 503），E4 改 /control 时它们不能变。
# 只把临时目录路径和时间字段（normalize.py）换成占位符，其余原样比。
# 之后跑 control_contract.py：挂起到做完才回复、队列满时 503 的回复体、E4 有意改变的几条旧请求行为、写锁。
# 依赖：sh、curl、python3、openssl。
# SPDX-License-Identifier: MIT
set -eu

MODE=${1:?usage: control_golden.sh record|check [BIN]}
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/../.." && pwd)
BIN=${2:-${ZWRT_DATAD_TEST_BIN:-$ROOT/rust/target/debug/zwrt-datad}}
PORT=${CONTROL_GOLDEN_PORT:-19570}
TMP=$(mktemp -d)
PID=
cleanup() {
    [ -z "$PID" ] || { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM
case "$MODE" in record | check) ;; *) echo "usage: control_golden.sh record|check [BIN]" >&2; exit 64 ;; esac
[ -x "$BIN" ] || { echo "control golden: 找不到 $BIN" >&2; exit 1; }

export TZ=UTC LC_ALL=C
export ZWRT_DATAD_UBUS_BIN="$HERE/fail_ubus.sh"
export ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh"
export ZWRT_DATAD_OTA_DISABLE_AUTO=1
export ZWRT_DATAD_MWAN3_INIT=/usr/bin/true
export ZWRT_DATAD_IW_BIN="$ROOT/tests/mock_iw.sh"
export ZWRT_DATAD_HOSTAPD_BIN="$ROOT/tests/mock_hostapd.py"
export ZWRT_DATAD_HOSTAPD_CLI_BIN="$ROOT/tests/mock_hostapd_cli.sh"
export ZWRT_DATAD_SMS_V3E1_URL="http://127.0.0.1:1/goform/goform_set_cmd_process"
export GOLDEN_FAIL_FILE="$TMP/fail"
# E4 事务：契约测试里锁被占的那段要够长，但结束得快
export ZWRT_DATAD_DEADLINE_NETWORK_MODE_MS=20000 ZWRT_DATAD_OP_POLL_MS=300
export MOCK_DATA_OFF_FILE="$TMP/data-off"
: >"$GOLDEN_FAIL_FILE"
UCI_MODE=show
. "$HERE/fixture.sh"
setup_fixture "$TMP/fx"
# sms.send_raw（sender=host）要先拿原厂网页的公钥加密（同 rust_control_integration.sh）
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$TMP/web-private.pem" 2>/dev/null
openssl pkey -in "$TMP/web-private.pem" -pubout -out "$TMP/web-public.pem" 2>/dev/null
export MOCK_WEB_PUBLIC_KEY_FILE="$TMP/web-public.pem"

# -i 5000：用例之间基本不会插进采集轮（插进来也只影响 /state，不影响回复）。
"$BIN" -i 5000 --bind 127.0.0.1 --port "$PORT" --data-dir "$TMP/fx/data" >"$TMP/server.log" 2>&1 &
PID=$!
i=0
while ! curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 600 ] || { tail -n 20 "$TMP/server.log" >&2; echo "control golden: datad 起不来" >&2; exit 1; }
    sleep 0.05
done

OUT=$TMP/control.txt
: >"$OUT"
n=0
TAB=$(printf '\t')
while IFS="$TAB" read -r kind fail body; do
    case "$kind" in '' | '#'*) continue ;; esac
    n=$((n + 1))
    if [ "$fail" = '*' ]; then printf '*\n' >"$GOLDEN_FAIL_FILE"; else : >"$GOLDEN_FAIL_FILE"; fi
    code=$(curl -sS --max-time 30 -o "$TMP/body" -w '%{http_code}' \
        -H 'Content-Type: application/json' --data-binary "$body" "http://127.0.0.1:$PORT/control") ||
        { echo "control golden: 第 $n 条没有回复：$body" >&2; tail -n 20 "$TMP/server.log" >&2; exit 1; }
    {
        printf '## %s %s %s\n> %s\n< %s ' "$n" "$kind" "$fail" "$body" "$code"
        cat "$TMP/body"
        printf '\n'
    } >>"$OUT"
done <"$HERE/control_cases.txt"
: >"$GOLDEN_FAIL_FILE"

# 临时目录路径换成占位符（错误文字里可能带 fixture 路径），再换时间字段。
python3 - "$OUT" "$TMP" "$ROOT" <<'PY'
import sys
path, tmp, root = sys.argv[1:]
text = open(path, encoding="utf-8").read().replace(tmp, "<tmp>").replace(root, "<root>")
open(path, "w", encoding="utf-8").write(text)
PY
python3 "$HERE/normalize.py" text <"$OUT" >"$OUT.norm"

case "$MODE" in
    record) cp "$OUT.norm" "$HERE/control.golden" ;;
    check)
        diff -u "$HERE/control.golden" "$OUT.norm" >&2 ||
            { echo "control golden: /control 回复变了（见上面的 diff；有意改变就单独写测试说明，再 record）" >&2; exit 1; }
        ;;
esac

# D18：pid 文件写的是这个 datad
[ "$(cat "$ZWRT_DATAD_PID_FILE")" = "$PID" ] || { echo "control golden: pid 文件不对" >&2; exit 1; }
python3 "$HERE/control_contract.py" "$PORT" "$GOLDEN_FAIL_FILE" "$ZWRT_DATAD_WRITE_LOCK" "$MOCK_DATA_OFF_FILE" ||
    { tail -n 20 "$TMP/server.log" >&2; exit 1; }
echo "control golden: $n 条回复一致，契约通过"
