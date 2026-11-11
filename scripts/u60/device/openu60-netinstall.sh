#!/bin/sh
# Installs, updates or removes OpenU60 on a ZTE U60 Pro from the router itself, with no
# computer: used by the UFI-TOOLS plugin (desktop/ufi-plugin/) through its root shell, or by
# hand over ssh/adb. Downloads the signed release the app's online update uses, checks the
# Ed25519 signature and every file's SHA-256, then does what the desktop installer does.
#
#   sh openu60-netinstall.sh status
#   sh openu60-netinstall.sh install relay|public [noscreen]
#   sh openu60-netinstall.sh ota         (online update through the agent, official builds)
#   sh openu60-netinstall.sh uninstall
#
# Progress lines: "@@STEP n/total text", "@@OK text", "@@FAIL text", and at the end of a first
# install "@@PASSWORD <password>" (shown once; it is only kept on the router).
set -u
export PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH
BASE="${OPENU60_BASE:-https://github.com/Yakultt-t/openu60/releases/download/router-update}"
WORK=/tmp/openu60-net
NEW=/data/local/tmp/openu60-new
PUBKEY='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAJeAflwHeTHAxEi+kmBVVpcj8GEtHe6/reoNvMcx2AE8=
-----END PUBLIC KEY-----'
umask 077

step() { echo "@@STEP $1 $2"; }
ok() { echo "@@OK $*"; }
fail() { echo "@@FAIL $*"; rm -rf "$WORK"; exit 1; }

agent_running() {
    for d in /proc/[0-9]*; do
        [ "$(tr '\0' '\n' < $d/cmdline 2>/dev/null | head -1)" = /data/zte-agent ] && return 0
    done
    return 1
}

cmd_status() {
    channel=""; build=0
    echo "model=$(uci -q get zwrt_common_info.common_config.hardware_version)"
    echo "firmware=$(uci -q get zwrt_common_info.common_config.integrate_version)"
    [ -x /data/zte-agent ] && echo "installed=yes" || echo "installed=no"
    agent_running && echo "running=yes" || echo "running=no"
    [ -x /data/openu60/screen/openu60-screen ] && echo "screen=yes" || echo "screen=no"
    tok=$(cat /tmp/openu60-local.token 2>/dev/null)
    if [ -n "$tok" ]; then
        st=$(curl -s -m 5 -H "Authorization: Bearer $tok" http://127.0.0.1:9090/api/update/status 2>/dev/null)
        [ -n "$st" ] && {
            channel=$(echo "$st" | jsonfilter -e '@.data.channel' 2>/dev/null)
            build=$(echo "$st" | jsonfilter -e '@.data.build' 2>/dev/null)
            echo "channel=$channel"
            echo "version=$(echo "$st" | jsonfilter -e '@.data.version' 2>/dev/null)"
            echo "build=$build"
        }
    fi
    latest=$(curl -fsL -m 15 "$BASE/manifest.json" 2>/dev/null | jsonfilter -e '@.version' -e '@.build' 2>/dev/null | tr '\n' ' ')
    echo "latest=$latest"
    lb=$(echo "$latest" | awk '{print $2}')
    case "${channel:-}" in public|relay)
        [ -n "$lb" ] && [ "$lb" -gt "${build:-0}" ] 2>/dev/null && echo "ota=yes" ;;
    esac
}

# Online update through the running agent (same signed path and automatic rollback as the
# app's update page). Only for official builds (channel public / relay).
api() { # method path
    tok=$(cat /tmp/openu60-local.token 2>/dev/null)
    curl -s -m 60 -X "$1" -H "Authorization: Bearer $tok" "http://127.0.0.1:9090$2" 2>/dev/null
}

cmd_ota() {
    step 1/3 "检查新版本"
    st=$(api POST /api/update/check)
    [ "$(echo "$st" | jsonfilter -e '@.data.supported' 2>/dev/null)" = true ] || fail "这台路由器上的版本不支持在线更新，请用「重新安装」"
    from=$(echo "$st" | jsonfilter -e '@.data.build')
    to=$(echo "$st" | jsonfilter -e '@.data.latest.build' 2>/dev/null)
    [ "$(echo "$st" | jsonfilter -e '@.data.latest.available' 2>/dev/null)" = true ] || { ok "已是最新版本"; echo "@@DONE"; return; }
    ok "可以从第 $from 版更新到第 $to 版"
    step 2/3 "下载、校验并安装（路由器自己完成）"
    api POST /api/update/apply >/dev/null
    step 3/3 "等待 OpenU60 以新版本重新启动"
    for i in $(seq 1 120); do
        sleep 3
        st=$(api GET /api/update/status)
        state=$(echo "$st" | jsonfilter -e '@.data.state' 2>/dev/null)
        b=$(echo "$st" | jsonfilter -e '@.data.build' 2>/dev/null)
        [ "$state" = failed ] && fail "更新失败：$(echo "$st" | jsonfilter -e '@.data.error' 2>/dev/null)"
        if [ -n "$b" ] && [ "$b" -ge "$to" ] 2>/dev/null; then
            ok "已更新到第 $b 版（$(echo "$st" | jsonfilter -e '@.data.version')）"
            echo "@@DONE"; return
        fi
        case "$state" in downloading) echo "下载中 $(echo "$st" | jsonfilter -e '@.data.done' 2>/dev/null) / $(echo "$st" | jsonfilter -e '@.data.total' 2>/dev/null) 字节" ;; esac
    done
    fail "等了 6 分钟还没有以新版本启动；如果新版本启动不了，路由器会自动恢复旧版本"
}

# Same steps as desktop/openu60-installer/device/openu60-device.sh (keep them in sync).
disable_cloud() {
    for s in zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect; do
        [ -x /etc/init.d/$s ] && { /etc/init.d/$s stop >/dev/null 2>&1; /etc/init.d/$s disable >/dev/null 2>&1; }
    done
    killall -q zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect 2>/dev/null
    uci -q set zwrt_smart_mng.smart_mng.acc_effect_enable=0 && uci commit zwrt_smart_mng
    uci -q set zwrt_tr069.ManagementServer.PeriodicInformEnable=0; uci -q set zwrt_tr069.Switch.monitor_enable=0; uci commit zwrt_tr069
    cp -n /etc/config/zte_topsw_daemon.conf /data/zte_topsw_daemon.conf.orig 2>/dev/null
    sed -i -E "s/^(sync[[:space:]]+ALL[[:space:]]+(zte_dm|zte_topsw_fota_result))$/#\1/" /etc/config/zte_topsw_daemon.conf
}

stop_agent() {
    touch /tmp/zte-agent.stop
    for d in /proc/[0-9]*; do
        [ "$(tr '\0' '\n' < $d/cmdline 2>/dev/null | sed -n 2p)" = /data/local/tmp/zte-agent-run.sh ] && kill ${d#/proc/} 2>/dev/null
    done
    killall zte-agent 2>/dev/null
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        netstat -ltn 2>/dev/null | grep -q ":9090 " || break
        sleep 1
    done
}

boot_hooks() {
    grep -q "zwrt_bsp.usb set" /etc/rc.local || sed -i "/^exit 0/i (sleep 15; ubus call zwrt_bsp.usb set '{\"mode\":\"debug\"}') &" /etc/rc.local
    grep -q "start_zte_agent.sh" /etc/rc.local || sed -i "/^exit 0/i sh /data/local/tmp/start_zte_agent.sh" /etc/rc.local
    grep -q "wifi_uplink.sh restore" /etc/rc.local || sed -i "/^exit 0/i (sleep 45; sh /data/local/tmp/wifi_uplink.sh restore) >/dev/null 2>&1 &" /etc/rc.local
    uci -q get zwrt_zte_mc.reboot_schedule >/dev/null && { uci set zwrt_zte_mc.reboot_schedule.reboot_schedule_enable=0; uci commit zwrt_zte_mc; }
    grep -q "start_zte_agent.sh" /etc/rc.local
}

dest() {
    case "$1" in
        zte-agent) echo /data/zte-agent ;;
        openu60-screen) echo /data/openu60/screen/openu60-screen ;;
        zte-agent-run.sh|wifi_uplink.sh|wifi_uplink_dhcp.sh) echo "/data/local/tmp/$1" ;;
        *) echo "" ;;
    esac
}

cmd_install() {
    channel="${1:-}"; noscreen="${2:-}"
    case "$channel" in public|relay) ;; *) fail "choose relay or public" ;; esac
    total=7
    step 1/$total "检查路由器"
    [ "$(uname -m)" = aarch64 ] || fail "不支持的处理器（$(uname -m)），只适用于中兴 U60 Pro"
    free_kb=$(df -k /data | awk 'END{print $(NF-2)}')
    [ "${free_kb:-0}" -gt 40000 ] || fail "/data 空间不足（需要约 40 MB）"
    echo "硬件版本：$(uci -q get zwrt_common_info.common_config.hardware_version)"

    step 2/$total "下载并校验更新清单"
    rm -rf "$WORK"; mkdir -p "$WORK"
    curl -fsL -m 60 -o "$WORK/manifest.json" "$BASE/manifest.json" || fail "下载失败，请检查路由器能否访问 GitHub"
    curl -fsL -m 60 -o "$WORK/manifest.sig" "$BASE/manifest.json.sig" || fail "下载签名失败"
    printf '%s\n' "$PUBKEY" > "$WORK/key.pem"
    base64 -d "$WORK/manifest.sig" > "$WORK/manifest.bin" 2>/dev/null
    openssl pkeyutl -verify -pubin -inkey "$WORK/key.pem" -rawin -in "$WORK/manifest.json" -sigfile "$WORK/manifest.bin" >/dev/null 2>&1 \
        || fail "更新清单的签名不对，已停止（文件可能被篡改）"
    version=$(jsonfilter -i "$WORK/manifest.json" -e '@.version')
    build=$(jsonfilter -i "$WORK/manifest.json" -e '@.build')
    ok "版本 $version（$build），签名正确"

    step 3/$total "下载程序（约 9 MB）"
    rm -rf "$NEW"; mkdir -p "$NEW"
    names=$(jsonfilter -i "$WORK/manifest.json" -e "@.channels.$channel[*].name")
    [ -n "$names" ] || fail "清单里没有 $channel 版本"
    i=0
    for name in $names; do
        url=$(jsonfilter -i "$WORK/manifest.json" -e "@.channels.$channel[$i].url")
        sum=$(jsonfilter -i "$WORK/manifest.json" -e "@.channels.$channel[$i].sha256")
        i=$((i + 1))
        [ -n "$(dest "$name")" ] || fail "清单里有未知文件 $name"
        [ "$name" = openu60-screen ] && [ "$noscreen" = noscreen ] && { echo "跳过触摸屏程序"; continue; }
        case "$url" in https://*) ;; *) fail "不安全的下载地址" ;; esac
        echo "下载 $name"
        curl -fsL -m 300 -o "$NEW/$name" "$url" || fail "下载 $name 失败"
        [ "$(sha256sum "$NEW/$name" | cut -d' ' -f1)" = "$sum" ] || fail "$name 校验不对，已停止"
    done
    ok "全部文件校验通过"
    if [ "${OPENU60_DRYRUN:-0}" = 1 ]; then rm -rf "$NEW" "$WORK"; echo "@@DONE (dry run, nothing installed)"; return; fi

    step 4/$total "关闭固件自动升级和中兴云服务（固件升级会把 OpenU60 冲掉）"
    disable_cloud
    step 5/$total "停止旧版本并安装"
    stop_agent
    mkdir -p /data/local/tmp /data/openu60
    for name in $(ls "$NEW"); do
        d=$(dest "$name")
        mkdir -p "$(dirname "$d")"
        case "$name" in *.sh) mode=700 ;; *) mode=755 ;; esac
        cp "$NEW/$name" "$d.new" && chmod $mode "$d.new" && mv "$d.new" "$d" || fail "安装 $name 失败"
    done
    rm -rf "$NEW"
    password=""
    if ! grep -q "^export ZTE_AGENT_PASSWORD=" /data/local/tmp/start_zte_agent.sh 2>/dev/null; then
        password=$(head -c 64 /dev/urandom | base64 | tr -dc 'A-HJ-NP-Za-km-z2-9' | head -c 20)
        [ ${#password} -eq 20 ] || fail "生成密码失败"
        cat > /data/local/tmp/start_zte_agent.sh <<EOF
#!/bin/sh
# Starts the zte-agent supervisor detached from whatever shell launched it (adb, rc.local).
export ZTE_AGENT_PASSWORD='$password'
[ -f /data/zte-agent-apns.json ] && export ZTE_AGENT_LIVE_ACTIVITY_CONFIG=/data/zte-agent-apns.json
cd /data
start-stop-daemon -S -b -x /data/local/tmp/zte-agent-run.sh
EOF
        chmod 700 /data/local/tmp/start_zte_agent.sh
    else
        echo "保留现有的 SIM 随行密码"
    fi

    step 6/$total "设置开机自动启动"
    boot_hooks || fail "写入开机启动失败"
    step 7/$total "启动并检查"
    rm -f /tmp/zte-agent.stop
    sh /data/local/tmp/start_zte_agent.sh >/dev/null 2>&1
    up=no
    for i in $(seq 1 40); do
        [ "$(curl -s -o /dev/null -w '%{http_code}' -m 3 http://127.0.0.1:9090/ 2>/dev/null)" = 200 ] && { up=yes; break; }
        sleep 1
    done
    rm -rf "$WORK"
    [ "$up" = yes ] || { tail -20 /tmp/zte-agent.log 2>/dev/null; fail "OpenU60 没有响应，上面是它的日志"; }
    ok "OpenU60 $version 已安装并在运行"
    [ -n "$password" ] && echo "@@PASSWORD $password"
    echo "@@DONE"
}

cmd_uninstall() {
    stop_agent
    sed -i "/start_zte_agent.sh/d; /wifi_uplink.sh restore/d" /etc/rc.local
    sh /data/local/tmp/wifi_uplink.sh mode hotspot >/dev/null 2>&1
    for d in /proc/[0-9]*; do
        c=$(tr '\0' ' ' < $d/cmdline 2>/dev/null)
        case "$c" in "/data/openu60/screen/openu60-screen "*) kill ${d#/proc/};; esac
    done
    sleep 2
    pidof zte_topsw_devui >/dev/null || /etc/init.d/zte_topsw_devui start >/dev/null 2>&1
    rm -f /data/zte-agent /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh /tmp/zte-agent.stop /data/openu60/screen/openu60-screen
    ok "已卸载。/data/openu60 里的数据保留；固件自动升级仍是关闭的。"
    echo "@@DONE"
}

case "${1:-}" in
    status) cmd_status ;;
    install) shift; cmd_install "$@" ;;
    ota) cmd_ota ;;
    uninstall) cmd_uninstall ;;
    *) echo "usage: $0 status | install relay|public [noscreen] | ota | uninstall"; exit 2 ;;
esac
