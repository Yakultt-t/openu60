#!/bin/bash
# Stop and disable the U60 Pro's FOTA checker and ZTE cloud reporting. All changes are reversible:
#   adb shell '/etc/init.d/<svc> enable; /etc/init.d/<svc> start'
# /etc is an overlay on the zteoverlay partition, so rc.d symlink and uci changes persist across reboots.
set -u
adb devices | grep -qw device || { echo "no adb device" >&2; exit 1; }

adb shell '
set -x
# 1. FOTA: zte_dm polls for firmware daily (CYCLE=1440); zte_topsw_fota_result reports upgrade results.
for s in zte_dm zte_topsw_fota_result; do /etc/init.d/$s stop; /etc/init.d/$s disable; done
killall -q zte_dm zte_topsw_fota_result 2>/dev/null
# 2. ZTE cloud link to rot-dispatch*.ztehome.com.cn / ztesmarthome.com:30443:
#    zte_smart_manage (app link), zte_topsw_get_brand (kept a live socket after smart_manage died),
#    scp_agent (remote management agent, S99).
for s in zte_smart_manage zte_topsw_get_brand scp_agent; do /etc/init.d/$s stop 2>/dev/null; /etc/init.d/$s disable; done
killall -q zte_smart_manage zte_topsw_get_brand scp_agent 2>/dev/null
uci set zwrt_smart_mng.smart_mng.acc_effect_enable=0; uci commit zwrt_smart_mng
# 3. TR-069: CWMP already disabled (EnableCWMP=0); make sure periodic inform/monitor are off too.
uci set zwrt_tr069.ManagementServer.PeriodicInformEnable=0
uci set zwrt_tr069.Switch.monitor_enable=0
uci commit zwrt_tr069
# 4. "Cutoff protect": pings public hosts and REBOOTS the router when they fail after a
#    re-register. It fired after a voice call (data dropped briefly) and would fire whenever
#    the router sits on Wi-Fi uplink with cellular data off. Not in the boot sync list.
/etc/init.d/zte_cutoff_protect stop; /etc/init.d/zte_cutoff_protect disable
killall -q zte_cutoff_protect 2>/dev/null
set +x
echo "== verify"
echo "rc.d entries left:"; ls /etc/rc.d | grep -E "zte_dm|fota_result|smart_manage|get_brand|scp_agent" || echo "(none)"
echo "processes left:"; ps w | grep -E "zte_dm|fota_result|smart_manage|get_brand|scp_agent" | grep -v grep || echo "(none)"
echo "outbound TCP:"; netstat -tn 2>/dev/null | grep -vE "127.0.0.1|192.168.0\." | grep ESTABLISHED || echo "(none)"
echo "uci:"; uci get zwrt_smart_mng.smart_mng.acc_effect_enable; uci get zwrt_tr069.ManagementServer.EnableCWMP; uci get zwrt_tr069.ManagementServer.PeriodicInformEnable
'
