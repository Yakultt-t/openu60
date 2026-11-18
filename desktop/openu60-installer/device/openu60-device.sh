#!/bin/sh
# Router-side half of the OpenU60 installer (Windows / macOS / Linux command-line tool).
# The installer pushes this file to /data/local/tmp/openu60-device.sh and runs one
# subcommand at a time, so the host never has to quote shell code through adb.
# Same steps as desktop/U60Toolkit/Resources/payload/u60.sh (the Mac app); keep them in sync.
# Files the installer has already pushed for "install": /data/local/tmp/openu60-new/*
set -u
NEW=/data/local/tmp/openu60-new

agent_running() {
    for d in /proc/[0-9]*; do
        [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | head -1)" = /data/zte-agent ] && return 0
    done
    return 1
}

cmd_status() {
    echo "model=$(uci -q get zwrt_common_info.common_config.hardware_version)"
    echo "firmware=$(uci -q get zwrt_common_info.common_config.integrate_version)"
    echo "arch=$(uname -m)"
    [ -x /data/zte-agent ] && echo "agent=installed" || echo "agent=absent"
    agent_running && echo "agent_running=yes" || echo "agent_running=no"
    grep -q start_zte_agent.sh /etc/rc.local && echo "boot_hook=yes" || echo "boot_hook=no"
    ls /etc/rc.d 2>/dev/null | grep -q zte_dm && echo "fota=on" || echo "fota=off"
    grep -q "^export ZTE_AGENT_PASSWORD=" /data/local/tmp/start_zte_agent.sh 2>/dev/null && echo "password=set" || echo "password=none"
    # Long device names wrap the df line; "available" is always third from the end.
    echo "free_kb=$(df -k /data | awk 'END{print $(NF-2)}')"
}

cmd_identity() {
    uci get zwrt_common_info.common_config.integrate_version
    uci get zwrt_common_info.common_config.hardware_version
    cat /etc/adb_devid 2>/dev/null
    echo
    cat /proc/partitions; echo
    ls -l /dev/block/bootdevice/by-name/ 2>/dev/null; echo
    mount; echo
    df -h
}

cmd_disable_cloud() {
    for s in zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect; do
        [ -x /etc/init.d/$s ] && { /etc/init.d/$s stop >/dev/null 2>&1; /etc/init.d/$s disable >/dev/null 2>&1; }
    done
    killall -q zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect 2>/dev/null
    uci -q set zwrt_smart_mng.smart_mng.acc_effect_enable=0 && uci commit zwrt_smart_mng
    uci -q set zwrt_tr069.ManagementServer.PeriodicInformEnable=0; uci -q set zwrt_tr069.Switch.monitor_enable=0; uci commit zwrt_tr069
    cp -n /etc/config/zte_topsw_daemon.conf /data/zte_topsw_daemon.conf.orig 2>/dev/null
    sed -i -E "s/^(sync[[:space:]]+ALL[[:space:]]+(zte_dm|zte_topsw_fota_result))$/#\1/" /etc/config/zte_topsw_daemon.conf
    echo ok
}

cmd_stop_agent() {
    touch /tmp/zte-agent.stop
    for d in /proc/[0-9]*; do
        [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | sed -n 2p)" = /data/local/tmp/zte-agent-run.sh ] && kill ${d#/proc/} 2>/dev/null
    done
    killall zte-agent 2>/dev/null
    for p in $(pgrep -f "ubus listen" 2>/dev/null); do kill $p 2>/dev/null; done
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        netstat -ltn 2>/dev/null | grep -q ":9090 " || break
        sleep 1
    done
    echo ok
}

# Moves the files pushed to $NEW into place. The start script arrives as
# $NEW/start_zte_agent.sh only when the password changes; otherwise the old one stays.
cmd_place_files() {
    mkdir -p /data/local/tmp /data/openu60
    [ -f $NEW/zte-agent ] || { echo "missing zte-agent"; exit 1; }
    chmod 755 $NEW/zte-agent && mv $NEW/zte-agent /data/zte-agent || exit 1
    for f in zte-agent-run.sh wifi_uplink.sh wifi_uplink_dhcp.sh start_zte_agent.sh; do
        [ -f $NEW/$f ] && mv $NEW/$f /data/local/tmp/$f
    done
    if [ -f $NEW/openu60-screen ]; then
        mkdir -p /data/openu60/screen
        chmod 755 $NEW/openu60-screen && mv $NEW/openu60-screen /data/openu60/screen/openu60-screen
        echo "screen=installed"
    fi
    chmod 700 /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh /data/local/tmp/wifi_uplink.sh /data/local/tmp/wifi_uplink_dhcp.sh 2>/dev/null
    rm -rf $NEW
    echo ok
}

cmd_boot_hooks() {
    # Older versions switched USB to debug mode at every boot. Debug is the firmware's factory
    # composition (stored as "factory") and drops the ECM network an iPhone uses for USB
    # tethering, so it is no longer kept on: the installer turns ADB on when it needs it.
    sed -i "/zwrt_bsp.usb set/d" /etc/rc.local
    grep -q "start_zte_agent.sh" /etc/rc.local || sed -i "/^exit 0/i sh /data/local/tmp/start_zte_agent.sh" /etc/rc.local
    grep -q "wifi_uplink.sh restore" /etc/rc.local || sed -i "/^exit 0/i (sleep 45; sh /data/local/tmp/wifi_uplink.sh restore) >/dev/null 2>&1 &" /etc/rc.local
    # The firmware weekly reboot runs on UTC (the router time zone), so it can land in the
    # middle of the day elsewhere; the agent supervisor and guards make it unnecessary.
    uci -q get zwrt_zte_mc.reboot_schedule >/dev/null && { uci set zwrt_zte_mc.reboot_schedule.reboot_schedule_enable=0; uci commit zwrt_zte_mc; }
    grep -q "start_zte_agent.sh" /etc/rc.local && echo ok
}

# Back to the firmware's normal USB mode ("user": network sharing for computers and iPhones)
# and drop the boot-time switch to debug mode. ADB goes away with it, so the switch runs
# detached a moment after this command has answered.
cmd_usb_normal() {
    sed -i "/zwrt_bsp.usb set/d" /etc/rc.local
    # setsid: a new session, so the switch survives adbd (and this shell) going away.
    setsid sh -c "sleep 2; ubus call zwrt_bsp.usb set '{\"mode\":\"user\"}'" </dev/null >/dev/null 2>&1 &
    echo ok
}

cmd_start() {
    rm -f /tmp/zte-agent.stop
    sh /data/local/tmp/start_zte_agent.sh >/dev/null 2>&1
    echo ok
}

cmd_uninstall() {
    cmd_stop_agent >/dev/null
    sed -i "/start_zte_agent.sh/d; /wifi_uplink.sh restore/d; /zwrt_bsp.usb set/d" /etc/rc.local
    sh /data/local/tmp/wifi_uplink.sh mode hotspot >/dev/null 2>&1
    for d in /proc/[0-9]*; do
        c=$(tr "\0" " " < $d/cmdline 2>/dev/null)
        case "$c" in "/data/openu60/screen/openu60-screen "*) kill ${d#/proc/};; esac
    done
    sleep 2
    pidof zte_topsw_devui >/dev/null || /etc/init.d/zte_topsw_devui start >/dev/null 2>&1
    rm -f /data/zte-agent /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh /tmp/zte-agent.stop /data/openu60/screen/openu60-screen
    echo ok
}

# The agent's own API over loopback, with the token it leaves for on-device clients.
vu_api() {
    curl -s -m 40 -X "$1" "http://127.0.0.1:9090$2" -H "Authorization: Bearer $(cat /tmp/openu60-local.token 2>/dev/null)" \
        -H "Content-Type: application/json" ${3:+-d "$3"}
}

case "${1:-}" in
    status) cmd_status ;;
    identity) cmd_identity ;;
    disable-cloud) cmd_disable_cloud ;;
    stop-agent) cmd_stop_agent ;;
    place-files) cmd_place_files ;;
    boot-hooks) cmd_boot_hooks ;;
    start) cmd_start ;;
    uninstall) cmd_uninstall ;;
    usb-normal) cmd_usb_normal ;;
    # Voice unlock (China-market units, experimental): through the agent's local API.
    vu-status) vu_api GET /api/modem/voice-unlock ;;
    vu-on) vu_api PUT /api/modem/voice-unlock '{"enabled":true}' ;;
    vu-off) vu_api PUT /api/modem/voice-unlock '{"enabled":false}' ;;
    vu-log) vu_api GET /api/modem/voice-unlock/log ;;
    vu-agentlog) grep -h "voice-unlock" /tmp/zte-agent.prev.log /tmp/zte-agent.log 2>/dev/null | tail -200 ;;
    log) tail -20 /tmp/zte-agent.log 2>/dev/null ;;
    # The installer keeps an existing password on updates so paired phones stay signed in;
    # it needs it once to check that the new agent answers.
    password) sed -n "s/^export ZTE_AGENT_PASSWORD='\(.*\)'$/\1/p" /data/local/tmp/start_zte_agent.sh 2>/dev/null ;;
    *) echo "usage: $0 status|identity|disable-cloud|stop-agent|place-files|boot-hooks|start|uninstall|usb-normal|vu-status|vu-on|vu-off|vu-log|vu-agentlog|log|password"; exit 2 ;;
esac
