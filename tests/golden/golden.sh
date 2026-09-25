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

# 每个情形都从同一份干净的 fixture 开始。
setup_fixture() {
    F=$1
    rm -rf "$F"
    mkdir -p "$F/data" "$F/vendor-wifi" "$F/net" "$F/proc" "$F/wifi-runtime" "$F/uci-state"
    mkdir -p "$F/state-net/rmnet_data0/statistics" "$F/state-thermal/thermal_zone0" "$F/zone"
    printf '1000\n' >"$F/state-net/rmnet_data0/statistics/rx_bytes"
    printf '2000\n' >"$F/state-net/rmnet_data0/statistics/tx_bytes"
    printf 'cpuss-0\n' >"$F/state-thermal/thermal_zone0/type"
    printf '42000\n' >"$F/state-thermal/thermal_zone0/temp"
    printf 'fixture-boot-id\n' >"$F/boot-id"
    printf '4102444800 00:11:22:33:44:99 192.168.0.99 historical-offline *\n' >"$F/dhcp.leases"
    printf '1\n' >"$F/sim-slot"
    printf 'fixture\n' >"$F/key.log"
    for base in wlan0 wlan1; do
        printf 'driver=nl80211\ninterface=old\nssid=old\n' >"$F/vendor-wifi/hostapd-$base.conf"
    done
    for file in pwm1 fan-thermal fan-state liquid-thermal liquid-drive zone/mode zone/temp \
        zone/trip_point_0_temp zone/trip_point_0_hyst zone/trip_point_1_temp zone/trip_point_1_hyst \
        zone/trip_point_2_temp zone/trip_point_2_hyst; do : >"$F/$file"; done
    printf '47000\n' >"$F/zone/temp"
    # 宿主机 /proc、/sys 换成固定内容（ZWRT_DATAD_HOST_ROOT）；host/data 故意不建，存储读数固定为 0
    H=$F/host
    mkdir -p "$H/proc/net" "$H/sys/devices/system/cpu/cpu0/cpufreq" \
        "$H/sys/class/power_supply/usb" "$H/sys/class/power_supply/battery"
    printf 'MemTotal:        3906000 kB\nMemFree:          812000 kB\nMemAvailable:    1954000 kB\nBuffers:           41000 kB\nCached:           987000 kB\nSwapTotal:        524284 kB\nSwapFree:         500000 kB\n' >"$H/proc/meminfo"
    printf 'cpu  100 0 50 800 10 0 5 0 0 0\ncpu0 100 0 50 800 10 0 5 0 0 0\n' >"$H/proc/stat"
    printf '  sl  local_address rem_address   st\n   0: 0100007F:24F4 00000000:0000 0A\n   1: 0100007F:24F4 0100007F:D431 01\n' >"$H/proc/net/tcp"
    printf 'header\nrow\n' >"$H/proc/net/tcp6"
    printf 'header\nrow\nrow\n' >"$H/proc/net/udp"
    printf 'header\n' >"$H/proc/net/udp6"
    printf 'header\nrow\nrow\nrow\n' >"$H/proc/net/unix"
    printf '1804800\n' >"$H/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"
    printf '2208000\n' >"$H/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq"
    printf '5000000\n' >"$H/sys/class/power_supply/usb/voltage_now"
    printf '900000\n' >"$H/sys/class/power_supply/usb/current_now"
    printf '4100000\n' >"$H/sys/class/power_supply/battery/voltage_now"
    printf '350000\n' >"$H/sys/class/power_supply/battery/current_now"
    export ZWRT_DATAD_HOST_ROOT="$H" ZWRT_DATAD_PROC_NET_TCP="$H/proc/net/tcp"
    export MOCK_CALL_LOG="$F/calls.log"
    export ZWRT_DATAD_WIFI_RUNTIME_DIR="$F/wifi-runtime"
    export ZWRT_DATAD_VENDOR_WIFI_DIR="$F/vendor-wifi"
    export ZWRT_DATAD_NET_CLASS_DIR="$F/net" MOCK_NET_CLASS_DIR="$F/net"
    export ZWRT_DATAD_NET_CLASS_ROOT="$F/state-net"
    export ZWRT_DATAD_THERMAL_ROOT="$F/state-thermal"
    export ZWRT_DATAD_PROC_ROOT="$F/proc"
    export ZWRT_DATAD_QOS_LOG="$F/key.log" ZWRT_DATAD_QOS_LOG_ROTATED="$F/key.log.0"
    export ZWRT_DATAD_DHCP_LEASES_PATH="$F/dhcp.leases"
    export MOCK_UCI_STATE_DIR="$F/uci-state"
    export MOCK_SIM_SLOT_FILE="$F/sim-slot"
    export ZWRT_DATAD_WIFI_CONFIG="$F/datad_wifi"
    export ZWRT_DATAD_COOLING_CONFIG="$F/cooling.conf"
    export ZWRT_DATAD_FAN_PWM_PATH="$F/pwm1"
    export ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH="$F/fan-thermal"
    export ZWRT_DATAD_FAN_COOLING_STATE_PATH="$F/fan-state"
    export ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH="$F/liquid-thermal"
    export ZWRT_DATAD_LIQUID_DRIVE_PATH="$F/liquid-drive"
    export ZWRT_DATAD_COOLING_ZONE_PATH="$F/zone"
    export ZWRT_DATAD_BOOT_ID_PATH="$F/boot-id"
    if [ "$UCI_MODE" = parse ]; then
        # R19：/etc/config 由 mock 内容生成；savedir 里 wireless 是 0 字节（= 没有未保存改动，照样解析），
        # dhcp 非空（= 有未保存改动，退回 uci show）。
        mkdir -p "$F/etc-config" "$F/uci-save"
        unset ZWRT_DATAD_UCI_PARSE
        export ZWRT_DATAD_UCI_CONFIG_DIR="$F/etc-config" ZWRT_DATAD_UCI_SAVEDIR="$F/uci-save"
        python3 "$HERE/uci_from_mock.py" "$ROOT/tests/mock_uci.sh" "$F/etc-config" >"$F/parsed-packages"
        : >"$F/uci-save/wireless"
        printf "dhcp.lan.leasetime='1h'\n" >"$F/uci-save/dhcp"
    else
        export ZWRT_DATAD_UCI_PARSE=0
        unset ZWRT_DATAD_UCI_CONFIG_DIR ZWRT_DATAD_UCI_SAVEDIR
    fi
}

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
