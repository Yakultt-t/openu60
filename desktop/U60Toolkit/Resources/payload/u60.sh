#!/bin/bash
# U60 刷机助手 installer: backup, install/update and remove zte-agent on a ZTE U60 Pro over ADB.
# Driven by the U60 Toolkit Mac app; also usable by hand:
#   u60.sh status
#   u60.sh backup-config <dir>          # identity, partitions, all UCI config, /etc overlay, agent data
#   u60.sh backup-full <dir>            # + raw eMMC images (~7.6 GB, verified by md5)
#   AGENT_PASSWORD=... u60.sh install   # disable FOTA/cloud, install agent + supervisor + boot hooks
#   u60.sh uninstall                    # stop and remove the agent and its boot hooks
# Optional for install: APNS_P8=<AuthKey_XXXX.p8> APNS_ENV=production|sandbox TEAM_ID=... BUNDLE_ID=...
# Progress lines for the app: "@@STEP <n>/<total> <title>", "@@OK <text>", "@@FAIL <text>".
# Secrets (passwords, keys) are passed in the environment and never printed.
set -u
PAYLOAD="$(cd "$(dirname "$0")" && pwd)"
ADB="${ADB:-adb}"
step() { echo "@@STEP $1 $2"; }
ok() { echo "@@OK $*"; }
fail() { echo "@@FAIL $*"; exit 1; }
need_device() {
    "$ADB" start-server >/dev/null 2>&1
    "$ADB" devices | awk 'NR>1 && $2=="device"' | grep -q . || fail "没有连上路由器的 ADB。请插好 USB 线并先开启调试模式。"
}
sh_dev() { "$ADB" shell "$@"; }

cmd_status() {
    "$ADB" start-server >/dev/null 2>&1
    if ! "$ADB" devices | awk 'NR>1 && $2=="device"' | grep -q .; then
        echo "adb=none"; return 0
    fi
    echo "adb=connected"
    sh_dev '
    echo "model=$(uci -q get zwrt_common_info.common_config.hardware_version)"
    echo "firmware=$(uci -q get zwrt_common_info.common_config.integrate_version)"
    [ -x /data/zte-agent ] && echo "agent=installed" || echo "agent=absent"
    running=no; for d in /proc/[0-9]*; do [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | head -1)" = /data/zte-agent ] && running=yes; done; echo "agent_running=$running"
    grep -q start_zte_agent.sh /etc/rc.local && echo "boot_hook=yes" || echo "boot_hook=no"
    [ -f /data/zte-agent-apns.json ] && echo "apns=yes" || echo "apns=no"
    ls /etc/rc.d 2>/dev/null | grep -q zte_dm && echo "fota=on" || echo "fota=off"
    ' 2>/dev/null | tr -d '\r'
}

# backup_common <dir> <first step number> <total steps>
backup_common() {
    local out="$1" n="$2" total="$3"
    mkdir -p "$out" || fail "无法创建文件夹 $out"
    step $n/$total "读取设备信息和分区表"
    sh_dev 'uci get zwrt_common_info.common_config.integrate_version; uci get zwrt_common_info.common_config.hardware_version; cat /etc/adb_devid 2>/dev/null' > "$out/identity.txt" 2>&1
    sh_dev 'cat /proc/partitions; echo; ls -l /dev/block/bootdevice/by-name/ 2>/dev/null; echo; mount; echo; df -h' > "$out/partitions.txt" 2>&1
    step $((n+1))/$total "导出全部系统配置"
    sh_dev 'uci export' > "$out/uci-export-all.txt" 2>&1
    step $((n+2))/$total "打包 /etc 持久分区"
    "$ADB" exec-out 'tar -C /zteoverlay -cf - . 2>/dev/null' > "$out/zteoverlay.tar" || fail "打包持久分区失败"
    step $((n+3))/$total "打包 U60 助手 数据"
    "$ADB" exec-out 'cd /data && tar -cf - openu60 local/tmp/*.sh local/tmp/*.json zte-agent-push.json zte-agent-apns.json apns.p8 2>/dev/null' > "$out/agent-data.tar"
    chmod -R go-rwx "$out"
}

cmd_backup_config() {
    need_device
    backup_common "${1:?usage: backup-config <dir>}" 1 4
    ok "配置备份已保存"
}

# Reads one block device in 256 MB chunks: the router compresses each chunk on the fly (much of
# the eMMC is empty), the Mac writes it at its offset, and both sides compare MD5 per chunk, so a
# bad chunk is re-read instead of the whole 7 GB. Prints "@@PROGRESS <done> <total>" in bytes.
CHUNK_MB=256
dump_device() {
    local dev="$1" img="$2" base_done="$3" grand_total="$4" manifest="$5"
    local sectors size chunks i want got tries
    sectors=$(sh_dev "cat /sys/block/$dev/size" | tr -d '\r')
    [ -n "$sectors" ] && [ "$sectors" -gt 0 ] 2>/dev/null || fail "读不到 $dev 的大小"
    size=$((sectors * 512))
    chunks=$(( (size + CHUNK_MB*1024*1024 - 1) / (CHUNK_MB*1024*1024) ))
    local per=$((CHUNK_MB / 4))   # 4 MB blocks per chunk
    : > "$img"
    for ((i = 0; i < chunks; i++)); do
        tries=0
        while :; do
            tries=$((tries + 1))
            want=$(sh_dev "dd if=/dev/$dev bs=4M skip=$((i*per)) count=$per 2>/dev/null | md5sum" | tr -d '\r' | awk '{print $1}')
            "$ADB" exec-out "dd if=/dev/$dev bs=4M skip=$((i*per)) count=$per 2>/dev/null | gzip -1" \
                | gunzip 2>/dev/null | dd of="$img" bs=4m seek=$((i*per)) conv=notrunc 2>/dev/null
            got=$(dd if="$img" bs=4m skip=$((i*per)) count=$per 2>/dev/null | md5 -q)
            [ -n "$want" ] && [ "$want" = "$got" ] && break
            "$ADB" get-state >/dev/null 2>&1 || fail "USB 连接断开了，请插好线后重新备份"
            [ -n "$want" ] || fail "读取 $dev 失败，请插好线后重新备份"
            if [ "$tries" -ge 3 ]; then
                # The router keeps running while it is read, so a block it is busy writing
                # (logs, the SMS database) can change between the two reads.
                echo "注意：$dev 第 $((i+1)) 块在读取时一直在被路由器写入，已保存最后一次读到的内容"
                want="changed-during-read:$got"
                break
            fi
            echo "$dev 第 $((i+1)) 块校验不一致（路由器正在写入），重新读取（第 $tries 次）"
        done
        echo "$dev $i $want" >> "$manifest"
        local done_bytes=$(( (i+1) * CHUNK_MB*1024*1024 ))
        [ "$done_bytes" -gt "$size" ] && done_bytes=$size
        echo "@@PROGRESS $((base_done + done_bytes)) $grand_total"
    done
    local have; have=$(stat -f %z "$img")
    [ "$have" = "$size" ] || fail "$dev 镜像大小不对（$have / $size 字节）"
}

cmd_backup_full() {
    need_device
    local out="${1:?usage: backup-full <dir>}"
    backup_common "$out" 1 6
    step 5/6 "读取整块存储芯片（约 7.6 GB，分块校验）"
    local total=0 dev s
    for dev in mmcblk0boot0 mmcblk0boot1 mmcblk0; do
        s=$(sh_dev "cat /sys/block/$dev/size" | tr -d '\r')
        total=$((total + s * 512))
    done
    local manifest="$out/chunk-md5.txt" done=0
    echo "# device chunk(${CHUNK_MB}MB) md5" > "$manifest"
    echo "@@PROGRESS 0 $total"
    for dev in mmcblk0boot0 mmcblk0boot1 mmcblk0; do
        echo "正在读取 /dev/$dev"
        dump_device "$dev" "$out/$dev.img" "$done" "$total" "$manifest"
        done=$((done + $(stat -f %z "$out/$dev.img")))
    done
    step 6/6 "计算整体校验值"
    for dev in mmcblk0boot0 mmcblk0boot1 mmcblk0; do
        echo "$(md5 -q "$out/$dev.img")  $dev.img"
    done > "$out/image-md5.txt"
    chmod -R go-rwx "$out"
    ok "完整备份完成，每一块都已校验"
}

stop_agent() {
    sh_dev '
    touch /tmp/zte-agent.stop
    for d in /proc/[0-9]*; do
      [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | sed -n 2p)" = /data/local/tmp/zte-agent-run.sh ] && kill ${d#/proc/} 2>/dev/null
    done
    killall zte-agent 2>/dev/null
    for p in $(pgrep -f "ubus listen" 2>/dev/null); do kill $p 2>/dev/null; done
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
      netstat -ltn 2>/dev/null | grep -q ":9090 " || break
      sleep 1
    done' >/dev/null 2>&1
}

cmd_install() {
    need_device
    local total=6
    [ -n "${APNS_P8:-}" ] && total=7
    step 1/$total "检查路由器"
    model=$(sh_dev 'uci -q get zwrt_common_info.common_config.hardware_version' | tr -d '\r')
    arch=$(sh_dev 'uname -m' | tr -d '\r')
    [ "$arch" = aarch64 ] || fail "不支持的处理器（$arch），这个版本只适用于 U60 Pro（aarch64）"
    echo "硬件版本：${model:-未知}"
    # Long device names wrap the df line; "available" is always third from the end.
    free_kb=$(sh_dev 'df -k /data' | tr -d '\r' | awk 'END{print $(NF-2)}')
    [ "${free_kb:-0}" -gt 40000 ] || fail "/data 空间不足"

    # Keep an existing password on updates so paired phones stay signed in.
    if [ -z "${AGENT_PASSWORD:-}" ]; then
        AGENT_PASSWORD=$(sh_dev "sed -n \"s/^export ZTE_AGENT_PASSWORD='\\(.*\\)'\$/\\1/p\" /data/local/tmp/start_zte_agent.sh 2>/dev/null" | tr -d '\r')
        [ -n "$AGENT_PASSWORD" ] || fail "没有设置 app 登录密码，路由器上也还没有"
        echo "保留路由器上现有的 app 登录密码"
    fi

    step 2/$total "关闭固件自动升级和中兴云服务"
    sh_dev '
    for s in zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect; do
      [ -x /etc/init.d/$s ] && { /etc/init.d/$s stop >/dev/null 2>&1; /etc/init.d/$s disable >/dev/null 2>&1; }
    done
    killall -q zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect 2>/dev/null
    uci -q set zwrt_smart_mng.smart_mng.acc_effect_enable=0 && uci commit zwrt_smart_mng
    uci -q set zwrt_tr069.ManagementServer.PeriodicInformEnable=0; uci -q set zwrt_tr069.Switch.monitor_enable=0; uci commit zwrt_tr069
    cp -n /etc/config/zte_topsw_daemon.conf /data/zte_topsw_daemon.conf.orig 2>/dev/null
    sed -i -E "s/^(sync[[:space:]]+ALL[[:space:]]+(zte_dm|zte_topsw_fota_result))$/#\1/" /etc/config/zte_topsw_daemon.conf
    ' >/dev/null 2>&1
    ok "已关闭固件升级和云上报（固件升级会把 U60 助手 冲掉）"

    step 3/$total "停止正在运行的 U60 助手"
    stop_agent

    step 4/$total "复制 U60 助手 程序和辅助脚本"
    sh_dev 'mkdir -p /data/local/tmp /data/openu60' >/dev/null
    "$ADB" push "$PAYLOAD/zte-agent" /data/zte-agent.new >/dev/null || fail "复制程序失败"
    sh_dev 'chmod 755 /data/zte-agent.new && mv /data/zte-agent.new /data/zte-agent' >/dev/null || fail "无法安装程序文件"
    for f in zte-agent-run.sh wifi_uplink.sh wifi_uplink_dhcp.sh; do
        "$ADB" push "$PAYLOAD/$f" "/data/local/tmp/$f" >/dev/null || fail "复制 $f 失败"
    done
    # Touchscreen UI (started by the agent; press the power key three times to switch to it).
    if [ -f "$PAYLOAD/openu60-screen" ]; then
        sh_dev 'mkdir -p /data/openu60/screen' >/dev/null
        "$ADB" push "$PAYLOAD/openu60-screen" /data/openu60/screen/openu60-screen.new >/dev/null || fail "复制屏幕程序失败"
        sh_dev 'chmod 755 /data/openu60/screen/openu60-screen.new && mv /data/openu60/screen/openu60-screen.new /data/openu60/screen/openu60-screen' >/dev/null
        echo "已安装新屏幕界面（连按三下电源键切换）"
    fi
    local tmp; tmp=$(mktemp)
    local safe; safe=$(printf '%s' "$AGENT_PASSWORD" | sed "s/'/'\\\\''/g")
    cat > "$tmp" <<STARTEOF
#!/bin/sh
# Starts the zte-agent supervisor detached from whatever shell launched it (adb, rc.local).
export ZTE_AGENT_PASSWORD='$safe'
[ -f /data/zte-agent-apns.json ] && export ZTE_AGENT_LIVE_ACTIVITY_CONFIG=/data/zte-agent-apns.json
cd /data
start-stop-daemon -S -b -x /data/local/tmp/zte-agent-run.sh
STARTEOF
    "$ADB" push "$tmp" /data/local/tmp/start_zte_agent.sh >/dev/null; rm -f "$tmp"
    sh_dev 'chmod 700 /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh /data/local/tmp/wifi_uplink.sh /data/local/tmp/wifi_uplink_dhcp.sh' >/dev/null

    step 5/$total "设置开机自动启动"
    sh_dev '
    grep -q "zwrt_bsp.usb set" /etc/rc.local || sed -i "/^exit 0/i (sleep 15; ubus call zwrt_bsp.usb set '"'"'{\"mode\":\"debug\"}'"'"') &" /etc/rc.local
    grep -q "start_zte_agent.sh" /etc/rc.local || sed -i "/^exit 0/i sh /data/local/tmp/start_zte_agent.sh" /etc/rc.local
    grep -q "wifi_uplink.sh restore" /etc/rc.local || sed -i "/^exit 0/i (sleep 45; sh /data/local/tmp/wifi_uplink.sh restore) >/dev/null 2>&1 &" /etc/rc.local
    # The firmware weekly reboot runs on UTC (the router time zone), so it lands in the
    # afternoon in Australia and takes the uplink and relay down; the agent supervisor
    # and guards make it unnecessary.
    uci -q get zwrt_zte_mc.reboot_schedule >/dev/null && { uci set zwrt_zte_mc.reboot_schedule.reboot_schedule_enable=0; uci commit zwrt_zte_mc; }
    grep -c "start_zte_agent.sh" /etc/rc.local' >/dev/null
    ok "已写入开机启动（/etc/rc.local）"

    local n=6
    if [ -n "${APNS_P8:-}" ]; then
        step 6/$total "安装推送密钥"
        [ -s "$APNS_P8" ] && grep -q "BEGIN PRIVATE KEY" "$APNS_P8" || fail "这不是苹果推送的 .p8 密钥文件"
        local key_id="${KEY_ID:-$(basename "$APNS_P8" | sed -n 's/^AuthKey_\([A-Z0-9]\{10\}\)\.p8$/\1/p')}"
        [ -n "$key_id" ] || fail "无法从文件名识别密钥 ID（应为 AuthKey_XXXXXXXXXX.p8）"
        "$ADB" push "$APNS_P8" /data/apns.p8 >/dev/null || fail "复制密钥失败"
        tmp=$(mktemp)
        printf '{"enabled":true,"team_id":"%s","key_id":"%s","bundle_id":"%s","key_file":"/data/apns.p8","environment":"%s","interval_seconds":30,"stale_after_seconds":120,"push_priority":5}\n' \
            "${TEAM_ID:?TEAM_ID required}" "$key_id" "${BUNDLE_ID:?BUNDLE_ID required}" "${APNS_ENV:-production}" > "$tmp"
        "$ADB" push "$tmp" /data/zte-agent-apns.json >/dev/null; rm -f "$tmp"
        sh_dev 'chmod 600 /data/apns.p8 /data/zte-agent-apns.json' >/dev/null
        ok "推送密钥已安装"
        n=7
    fi

    step $n/$total "启动并检查 U60 助手"
    sh_dev 'rm -f /tmp/zte-agent.stop; sh /data/local/tmp/start_zte_agent.sh' >/dev/null 2>&1
    "$ADB" forward tcp:19091 tcp:9090 >/dev/null
    local token=""
    for i in $(seq 1 40); do
        token=$(curl -sf -m 3 http://127.0.0.1:19091/api/auth/login -H 'Content-Type: application/json' \
            --data-binary "$(printf '%s' "$AGENT_PASSWORD" | /usr/bin/python3 -c 'import json,sys; print(json.dumps({"password": sys.stdin.read()}))' 2>/dev/null || printf '{"password":"%s"}' "$AGENT_PASSWORD")" \
            | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
        [ -n "$token" ] && break
        sleep 1
    done
    if [ -z "$token" ]; then
        "$ADB" forward --remove tcp:19091 >/dev/null 2>&1
        sh_dev 'tail -20 /tmp/zte-agent.log' 2>/dev/null
        fail "U60 助手 没有响应"
    fi
    info=$(curl -sf -m 5 http://127.0.0.1:19091/api/tunnel/info -H "Authorization: Bearer $token")
    "$ADB" forward --remove tcp:19091 >/dev/null 2>&1
    echo "设备 ID：$(printf '%s' "$info" | sed -n 's/.*"device_id":"\([^"]*\)".*/\1/p')"
    ok "U60 助手 已安装并在运行"
}

cmd_uninstall() {
    need_device
    step 1/3 "停止 U60 助手"
    stop_agent
    step 2/3 "移除开机启动"
    sh_dev '
    sed -i "/start_zte_agent.sh/d; /wifi_uplink.sh restore/d" /etc/rc.local
    sh /data/local/tmp/wifi_uplink.sh mode hotspot >/dev/null 2>&1' >/dev/null 2>&1
    step 3/3 "删除 U60 助手 程序"
    sh_dev 'for d in /proc/[0-9]*; do c=$(tr "\0" " " < $d/cmdline 2>/dev/null); case "$c" in "/data/openu60/screen/openu60-screen "*) kill ${d#/proc/};; esac; done; sleep 2; pidof zte_topsw_devui >/dev/null || /etc/init.d/zte_topsw_devui start' >/dev/null 2>&1
    sh_dev 'rm -f /data/zte-agent /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh /tmp/zte-agent.stop /data/openu60/screen/openu60-screen' >/dev/null
    ok "U60 助手 已卸载。/data/openu60 里的数据保留；固件自动升级仍是关闭的，需要时再手动打开。"
}

case "${1:-}" in
    status) cmd_status ;;
    backup-config) shift; cmd_backup_config "$@" ;;
    backup-full) shift; cmd_backup_full "$@" ;;
    install) cmd_install ;;
    uninstall) cmd_uninstall ;;
    *) echo "usage: $0 status|backup-config <dir>|backup-full <dir>|install|uninstall"; exit 2 ;;
esac
