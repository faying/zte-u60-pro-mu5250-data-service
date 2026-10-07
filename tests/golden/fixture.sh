# golden.sh 和 control_golden.sh 共用的 fixture：setup_fixture 目录 → 一套干净的假设备。
# 调用方先设好 HERE（本目录）、ROOT（仓库根）、UCI_MODE（show|parse）。
# SPDX-License-Identifier: MIT

# 每个情形都从同一份干净的 fixture 开始。
setup_fixture() {
    F=$1
    rm -rf "$F"
    mkdir -p "$F/data" "$F/uci-state"
    mkdir -p "$F/state-net/rmnet_data0/statistics" "$F/state-thermal/thermal_zone0"
    printf '1000\n' >"$F/state-net/rmnet_data0/statistics/rx_bytes"
    printf '2000\n' >"$F/state-net/rmnet_data0/statistics/tx_bytes"
    printf 'cpuss-0\n' >"$F/state-thermal/thermal_zone0/type"
    printf '42000\n' >"$F/state-thermal/thermal_zone0/temp"
    printf 'fixture-boot-id\n' >"$F/boot-id"
    printf '4102444800 00:11:22:33:44:99 192.168.0.99 historical-offline *\n' >"$F/dhcp.leases"
    printf '1\n' >"$F/sim-slot"
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
    export ZWRT_DATAD_NET_CLASS_ROOT="$F/state-net"
    export ZWRT_DATAD_THERMAL_ROOT="$F/state-thermal"
    export ZWRT_DATAD_DHCP_LEASES_PATH="$F/dhcp.leases"
    export MOCK_UCI_STATE_DIR="$F/uci-state"
    export MOCK_SIM_SLOT_FILE="$F/sim-slot"
    # V2-48：插线状态（cc、data_role、powerbank），空目录 = 什么都没插
    mkdir -p "$F/usb"
    export MOCK_USB_STATE_DIR="$F/usb"
    export ZWRT_DATAD_BOOT_ID_PATH="$F/boot-id"
    # E4 事务落盘目录（pending.json）：每个情形一个干净的
    export ZWRT_DATAD_OPS_DIR="$F/ops"
    # E4 T3：跨进程写锁、pid 文件（真机在 /var/run）
    export ZWRT_DATAD_WRITE_LOCK="$F/write.lock" ZWRT_DATAD_PID_FILE="$F/zwrt-datad.pid"
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
