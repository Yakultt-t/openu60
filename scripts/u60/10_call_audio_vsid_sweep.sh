#!/bin/bash
# Call-audio experiments using ZTE's own ODU voice bridge (zte_route_hal_voice_test -p dummy -r dummy -t 0):
# the DSP's downlink (far end) comes out as PCMU RTP on 192.168.253.1:48894 -> 192.168.253.100:48894, and
# PCMU RTP sent to 192.168.253.1:48894 goes into the uplink. Both addresses live on the device's lo.
# One dial per item VSID:MODE, where MODE says when the probe (voice graph) starts:
#   before  = probe running before dialling;  active = started only after the call is active
#   restart = probe before dialling, stopped and started again once active (ZTE's voice_call.sh restart)
# Per item: tcpdump captures both RTP directions, a Lua sender injects 1 kHz beeps into the uplink once the
# call is active, then the pcap + filtered device log are pulled into call_audio_results/ and analysed.
# usage: ./10_call_audio_vsid_sweep.sh <number> <seconds> [VSID:MODE ...]   default: 11C05000:before 11C05000:restart 11C05000:active
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
NUM="${1:?usage: $0 <number> <seconds> [VSID:MODE ...]}"; SECS="${2:-20}"; shift 2
ITEMS=("$@"); [ ${#ITEMS[@]} -gt 0 ] || ITEMS=(11C05000:before 11C05000:restart 11C05000:active)
OUT="$U60_HOME/call_audio_results"; mkdir -p "$OUT"
PW=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "$U60_HOME/agent-credentials.txt")
adb forward tcp:19090 tcp:9090 >/dev/null
TOKEN=$(curl -sf -m 5 http://127.0.0.1:19090/api/auth/login -H 'Content-Type: application/json' \
  --data-binary "$(printf '%s' "$PW" | python3 -c 'import json,sys; print(json.dumps({"password": sys.stdin.read()}))')" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["token"])') || { echo "login failed"; exit 1; }
api() { curl -sf -m 5 "http://127.0.0.1:19090$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "${@:2}"; }
calls() { api /api/call/status | python3 -c 'import json,sys
for c in json.load(sys.stdin)["data"]["calls"]:
    if c.get("mode")==0: print(c["stat"], c.get("number",""))' 2>/dev/null; }
stop_probe() { adb shell 'for p in $(pgrep zte_route_hal_voice_test); do kill -INT $p; done; sleep 2; for p in $(pgrep zte_route_hal_voice_test); do kill -9 $p; done; true'; }
start_probe() { adb shell "start-stop-daemon -S -b -x /data/local/tmp/odu_probe_run.sh -- $1; sleep 4; renice 15 -p \$(pgrep zte_route_hal_voice_test) >/dev/null 2>&1; echo '-- probe:'; pgrep -l zte_route_hal_voice_test; grep -a 'rtp_listen\|VoiceStart' /tmp/syslog.full | sed 's/^.\{0,16\}//' | tail -2"; }
wait_state() { local t=0; ST=""; while [ $t -lt $2 ]; do ST=$(calls | head -1); echo "  t=${t}s: ${ST:-none}"; case "$ST" in $1) return 0;; esac; sleep 2; t=$((t+2)); done; return 1; }
adb shell 'ip addr add 192.168.253.1/24 dev lo 2>/dev/null; ip addr add 192.168.253.100/24 dev lo 2>/dev/null; true'
SUMMARY=()
for ITEM in "${ITEMS[@]}"; do
  VSID="${ITEM%%:*}"; MODE="${ITEM#*:}"; [ "$MODE" = "$ITEM" ] && MODE=before
  TAG="${VSID}_${MODE}_${NUM}"
  echo; echo "########## $VSID mode=$MODE ##########"
  stop_probe
  adb shell "kill \$(cat /tmp/audio_logcollect.pid 2>/dev/null) 2>/dev/null; killall tcpdump 2>/dev/null; : > /tmp/syslog.full; rm -f /tmp/syslog.full.0 /tmp/rtp_call.pcap; start-stop-daemon -S -b -x /data/local/tmp/audio_logcollect_run.sh; start-stop-daemon -S -b -x /data/local/tmp/tcpdump_run.sh; true"
  case "$MODE" in before|restart) start_probe "$VSID";; esac
  echo "== dial $NUM"; api /api/call/dial --data-binary "{\"number\":\"$NUM\"}"; echo
  if ! wait_state 'active*' 40; then echo "call never became active"; api /api/call/hangup; SUMMARY+=("$ITEM: call not active"); stop_probe; adb shell 'killall tcpdump 2>/dev/null; true'; continue; fi
  case "$MODE" in active) start_probe "$VSID";; restart) echo "== restarting probe during the call"; stop_probe; start_probe "$VSID";; esac
  echo "== active; injecting beeps into the uplink and capturing for $SECS s"
  adb shell "start-stop-daemon -S -b -x /data/local/tmp/rtp_send_run.sh -- $SECS; true"
  for i in $(seq 5 5 $SECS); do sleep 5; echo "  +${i}s: pcap=$(adb shell 'stat -c %s /tmp/rtp_call.pcap 2>/dev/null || echo 0') B, call: $(calls | head -1)"; done
  echo "== hangup"; api /api/call/hangup; echo; sleep 2
  stop_probe
  adb shell "killall tcpdump 2>/dev/null; kill \$(cat /tmp/audio_logcollect.pid 2>/dev/null) 2>/dev/null; cat /tmp/rtp_send.log; grep -v 'writing to hal\|start error' /tmp/odu_probe.log | tail -4; echo \"-- counts: writes=\$(grep -c 'writing to hal' /tmp/odu_probe.log) readerrs=\$(grep -o 'start error' /tmp/odu_probe.log | wc -l)\""
  adb pull /tmp/rtp_call.pcap "$OUT/rtp_$TAG.pcap" >/dev/null 2>&1
  adb pull /tmp/audio_filtered.raw "$OUT/log_$TAG.raw" >/dev/null 2>&1 && awk '!seen[$0]++' "$OUT/log_$TAG.raw" > "$OUT/log_$TAG.txt" && rm -f "$OUT/log_$TAG.raw"
  echo "-- filtered log: $(wc -l < "$OUT/log_$TAG.txt" 2>/dev/null || echo 0) lines; errors:"
  grep -a -i "error\|fail\|not process" "$OUT/log_$TAG.txt" 2>/dev/null | sed 's/^.\{0,16\}//; s/\[[0-9]*\]//' | awk '!seen[$0]++' | head -10
  echo "-- RTP analysis:"
  R=$(python3 "$HERE/tools/rtp_pcap_analyze.py" "$OUT/rtp_$TAG.pcap" "$OUT/rtp_$TAG.wav" 2>&1); echo "$R" | head -70
  DL=$(echo "$R" | awk '/flow 192.168.253.1:/{f=1} /flow 192.168.253.100:/{f=0} f && $1 ~ /^[0-9]+$/ && $3+0 > 50 {c++} END{print c+0}')
  SUMMARY+=("$ITEM: downlink seconds with audio = $DL (see rtp_$TAG.wav)")
done
echo; echo "########## SUMMARY ##########"; printf '%s\n' "${SUMMARY[@]}"
