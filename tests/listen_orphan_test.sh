#!/bin/sh
# datad 被 SIGTERM / SIGKILL 杀掉后，`ubus listen` 子进程必须在 2 秒内消失（不能变成孤儿）。
# mock 的 listen 像真 ubus 一样不看父进程、一直挂着，只把自己的 pid 写进文件。仅 Linux。
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN=${1:-${ZWRT_DATAD_TEST_BIN:-$ROOT/rust/target/debug/zwrt-datad}}
PORT=${LISTEN_ORPHAN_PORT:-19480}
TMP=$(mktemp -d)
PID=
cleanup() {
    [ -z "$PID" ] || kill -9 "$PID" 2>/dev/null || true
    for f in "$TMP"/listen.*.pid; do [ -f "$f" ] && kill -9 "$(cat "$f")" 2>/dev/null || true; done
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

cat >"$TMP/ubus" <<MOCK
#!/bin/sh
if [ "\$1" = listen ]; then
    echo \$\$ >"$TMP/listen.\$\$.pid"
    trap '' TERM HUP
    while :; do sleep 1; done
fi
exec "$ROOT/tests/mock_ubus.sh" "\$@"
MOCK
chmod +x "$TMP/ubus"
export ZWRT_DATAD_UBUS_BIN="$TMP/ubus"
export ZWRT_DATAD_UCI_BIN="$ROOT/tests/mock_uci.sh"
export ZWRT_DATAD_OTA_DISABLE_AUTO=1
export ZWRT_DATAD_MWAN3_INIT=/usr/bin/true
export ZWRT_DATAD_IW_BIN="$ROOT/tests/mock_iw.sh"
export ZWRT_DATAD_HOSTAPD_CLI_BIN="$ROOT/tests/mock_hostapd_cli.sh"
export ZWRT_DATAD_WIFI_RUNTIME_DIR="$TMP/wifi-runtime"
export ZWRT_DATAD_VENDOR_WIFI_DIR="$TMP/vendor-wifi"
export MOCK_UCI_STATE_DIR="$TMP/uci-state"
mkdir -p "$TMP/data" "$ZWRT_DATAD_VENDOR_WIFI_DIR"

one_round() { # $1 = TERM | KILL
    rm -f "$TMP"/listen.*.pid
    "$BIN" --bind 127.0.0.1 --port "$PORT" --data-dir "$TMP/data" >"$TMP/server.log" 2>&1 &
    PID=$!
    i=0
    while ! ls "$TMP"/listen.*.pid >/dev/null 2>&1; do
        i=$((i + 1)); [ "$i" -lt 200 ] || { echo "FAIL: listen 没起来"; tail -n 20 "$TMP/server.log"; exit 1; }
        sleep 0.05
    done
    sleep 0.2
    child=$(cat "$TMP"/listen.*.pid)
    kill -0 "$child"
    kill "-$1" "$PID"
    wait "$PID" 2>/dev/null || true
    PID=
    i=0
    while kill -0 "$child" 2>/dev/null && [ "$(awk '{print $3}' /proc/"$child"/stat 2>/dev/null)" != Z ]; do
        i=$((i + 1)); [ "$i" -lt 20 ] || { echo "FAIL: SIG$1 后 listen 子进程 $child 2 秒内没退出"; exit 1; }
        sleep 0.1
    done
    echo "ok: SIG$1 后 listen 子进程已退出"
}
one_round TERM
one_round KILL
one_round INT
echo "listen_orphan_test: 通过"
