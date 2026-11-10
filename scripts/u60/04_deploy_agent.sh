#!/bin/bash
# U60 Pro: fix the boot sync list for the disabled FOTA daemons, persist ADB at boot,
# deploy open-u60-pro's zte-agent with its supervisor and start it.
# Usage: ./04_deploy_agent.sh <path-to-zte-agent-binary>
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
BIN="${1:?usage: $0 <zte-agent binary>}"
CRED="$U60_HOME/agent-credentials.txt"
adb devices | grep -qw device || { echo "no adb device" >&2; exit 1; }

# Agent password: generated once, kept in a 600 file in $U60_HOME (outside the repo), never printed.
if [ ! -s "$CRED" ]; then
  umask 077
  printf 'ZTE_AGENT_PASSWORD=%s\n' "$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 24)" > "$CRED"
fi
AGENT_PASSWORD=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "$CRED")

echo "== 1. daemon.conf: comment out the two daemons we disabled (keeps zte_topsw_daemon from waiting on them)"
adb shell 'cp -n /etc/config/zte_topsw_daemon.conf /data/zte_topsw_daemon.conf.orig; sed -i -E "s/^(sync[[:space:]]+ALL[[:space:]]+(zte_dm|zte_topsw_fota_result))$/#\1/" /etc/config/zte_topsw_daemon.conf; grep -nE "zte_dm|fota_result" /etc/config/zte_topsw_daemon.conf; ls -la /zteoverlay/etc-upper_a/config/zte_topsw_daemon.conf'

echo "== 2. push agent"
adb shell '
# Stop the supervisor first (it would restart the old binary), then the agent.
touch /tmp/zte-agent.stop
for d in /proc/[0-9]*; do
  [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | sed -n 2p)" = /data/local/tmp/zte-agent-run.sh ] && kill ${d#/proc/} 2>/dev/null
done
killall zte-agent 2>/dev/null
# The agent forks "ubus listen" helpers that inherit the :9090 listening socket; if they
# outlive the agent the new binary cannot bind (it exits silently). Reap them and wait.
for p in $(pgrep -f "ubus listen" 2>/dev/null); do kill $p 2>/dev/null; done
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
  netstat -ltn 2>/dev/null | grep -q ":9090 " || break
  sleep 1
done
netstat -ltn 2>/dev/null | grep ":9090 " && echo "WARNING: port 9090 still bound"
mkdir -p /data/local/tmp'
adb push "$BIN" /data/zte-agent >/dev/null && adb shell 'chmod 755 /data/zte-agent; sha256sum /data/zte-agent'

echo "== 3. startup script (password lives only on the device, mode 600)"
SAFE=$(printf '%s' "$AGENT_PASSWORD" | sed "s/'/'\\\\''/g")
cat > "$U60_HOME/.start_zte_agent.sh" <<STARTEOF
#!/bin/sh
# Starts zte-agent detached from whatever shell launched it (adb, rc.local).
export ZTE_AGENT_PASSWORD='$SAFE'
# Push (APNs) config written by 12_setup_apns.sh; kept across deploys.
[ -f /data/zte-agent-apns.json ] && export ZTE_AGENT_LIVE_ACTIVITY_CONFIG=/data/zte-agent-apns.json
cd /data
# adbd kills the whole process group when the shell session ends, so the agent must get
# its own session: start-stop-daemon -b does a setsid+fork (busybox here has no setsid).
# It refuses when the executable is "already running", so use a dedicated wrapper, not /bin/sh.
if command -v start-stop-daemon >/dev/null 2>&1; then
  start-stop-daemon -S -b -x /data/local/tmp/zte-agent-run.sh
else
  ( trap '' HUP INT; exec /data/zte-agent >/tmp/zte-agent.log 2>&1 </dev/null ) &
fi
STARTEOF
adb push "$U60_HOME/.start_zte_agent.sh" /data/local/tmp/start_zte_agent.sh >/dev/null; rm -f "$U60_HOME/.start_zte_agent.sh"
# Supervisor: restarts the agent after a crash or hang and keeps its RAM log bounded.
adb push "$HERE/device/zte-agent-run.sh" /data/local/tmp/zte-agent-run.sh >/dev/null
adb shell 'chmod 700 /data/local/tmp/start_zte_agent.sh /data/local/tmp/zte-agent-run.sh'

echo "== 3b. Wi-Fi uplink helper scripts"
adb push "$HERE/device/wifi_uplink.sh" /data/local/tmp/wifi_uplink.sh >/dev/null
adb push "$HERE/device/wifi_uplink_dhcp.sh" /data/local/tmp/wifi_uplink_dhcp.sh >/dev/null
adb shell 'chmod 700 /data/local/tmp/wifi_uplink.sh /data/local/tmp/wifi_uplink_dhcp.sh'

echo "== 4. rc.local: ADB debug at boot + agent at boot (idempotent)"
adb shell '
grep -q "zwrt_bsp.usb set" /etc/rc.local || sed -i "/^exit 0/i (sleep 15; ubus call zwrt_bsp.usb set '"'"'{\"mode\":\"debug\"}'"'"') &" /etc/rc.local
grep -q "start_zte_agent.sh" /etc/rc.local || sed -i "/^exit 0/i sh /data/local/tmp/start_zte_agent.sh" /etc/rc.local
grep -q "wifi_uplink.sh restore" /etc/rc.local || sed -i "/^exit 0/i (sleep 45; sh /data/local/tmp/wifi_uplink.sh restore) >/dev/null 2>&1 &" /etc/rc.local
# The firmware weekly reboot runs on UTC (the router time zone), so it lands in the
# afternoon in Australia and takes the uplink and relay down; the agent supervisor
# and guards make it unnecessary.
uci -q get zwrt_zte_mc.reboot_schedule >/dev/null && { uci set zwrt_zte_mc.reboot_schedule.reboot_schedule_enable=0; uci commit zwrt_zte_mc; }
echo "--- rc.local tail:"; tail -5 /etc/rc.local'

echo "== 5. start agent and verify login"
adb shell 'rm -f /tmp/zte-agent.stop; sh /data/local/tmp/start_zte_agent.sh'
adb forward tcp:19090 tcp:9090 >/dev/null
TOKEN=""
for i in $(seq 1 40); do
  RESP=$(curl -sf -m 3 http://127.0.0.1:19090/api/auth/login -H 'Content-Type: application/json' \
         --data-binary "$(printf '%s' "$AGENT_PASSWORD" | python3 -c 'import json,sys; print(json.dumps({"password": sys.stdin.read()}))')") || true
  TOKEN=$(printf '%s' "$RESP" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["data"]["token"])
except Exception: pass' 2>/dev/null)
  [ -n "$TOKEN" ] && break; sleep 1
done
if [ -z "$TOKEN" ]; then echo "AGENT LOGIN FAILED"; adb shell 'tail -30 /tmp/zte-agent.log'; adb forward --remove tcp:19090; exit 1; fi
echo "agent login OK"
echo "--- /api/device:"; curl -sf -m 5 http://127.0.0.1:19090/api/device -H "Authorization: Bearer $TOKEN" | head -c 1500; echo
adb forward --remove tcp:19090 >/dev/null
echo "--- agent log tail:"; adb shell 'tail -5 /tmp/zte-agent.log'
