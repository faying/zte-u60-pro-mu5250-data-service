# golden.sh 和 control_golden.sh 共用的 fixture：setup_fixture 目录 → 一套干净的假设备。
# 调用方先设好 HERE（本目录）、ROOT（仓库根）、UCI_MODE（show|parse）。
# SPDX-License-Identifier: MIT

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
    # E4 事务落盘目录（pending.json）：每个情形一个干净的
    export ZWRT_DATAD_OPS_DIR="$F/ops"
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
