#!/bin/bash
# Watches the agent's call status; when a voice call becomes active it records the far end
# (downlink) with hal_rec_test and plays beeps into the uplink with hal_play_test on the device.
# Needs: adb, the persistent voice graph (zte_route_hal_voice_test -i 11C05000 -d 2,2147483652) running.
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
LOG="${LOG:-$U60_HOME/call_audio_watch.log}"
MAXWAIT="${MAXWAIT:-900}"
PW=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "$U60_HOME/agent-credentials.txt")
adb forward tcp:19090 tcp:9090 >/dev/null
TOKEN=$(curl -sf -m 5 http://127.0.0.1:19090/api/auth/login -H 'Content-Type: application/json' \
  --data-binary "$(printf '%s' "$PW" | python3 -c 'import json,sys; print(json.dumps({"password": sys.stdin.read()}))')" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["token"])') || { echo "login failed" >>"$LOG"; exit 1; }
log() { echo "$(date '+%H:%M:%S') $*" >>"$LOG"; }
voice_active() {
  curl -sf -m 3 http://127.0.0.1:19090/api/call/status -H "Authorization: Bearer $TOKEN" | python3 -c '
import json,sys
d=json.load(sys.stdin)["data"]["calls"]
print("yes" if any(c.get("mode")==0 and c.get("stat")=="active" for c in d) else "no")' 2>/dev/null
}
log "watch start (max ${MAXWAIT}s)"
t=0
while [ $t -lt $MAXWAIT ]; do
  s=$(voice_active)
  [ "$s" = "yes" ] && break
  sleep 1; t=$((t+1))
done
[ "$s" = "yes" ] || { log "no voice call within ${MAXWAIT}s, exit"; exit 2; }
log "VOICE CALL ACTIVE -> start uplink beeps + downlink record"
adb shell 'logger -t MARK "call active: start play+rec"; start-stop-daemon -S -b -x /data/local/tmp/play_beeps_run.sh; start-stop-daemon -S -b -x /data/local/tmp/rec_run.sh -- 3 20 dl'
sleep 22
log "downlink record done; files: $(adb shell 'ls -la /data/audio/rec/ 2>/dev/null' | tr '\n' ' ')"
if [ "$(voice_active)" = "yes" ]; then
  log "call still active -> uplink record 8s (should contain the beeps)"
  adb shell 'start-stop-daemon -S -b -x /data/local/tmp/rec_run.sh -- 2 8 ul'
  sleep 10
fi
# wait for call end (max 120 s), then stop tools
t=0
while [ $t -lt 120 ] && [ "$(voice_active)" = "yes" ]; do sleep 1; t=$((t+1)); done
adb shell 'for p in $(pgrep hal_play_test) $(pgrep hal_rec_test); do kill -INT $p; done; sleep 1; for p in $(pgrep hal_play_test) $(pgrep hal_rec_test); do kill -9 $p; done; logger -t MARK "call ended: tools stopped"'
log "call ended; files: $(adb shell 'ls -la /data/audio/rec/ 2>/dev/null' | tr '\n' ' ')"
mkdir -p "$U60_HOME/call_audio_results"; rm -f "$U60_HOME/call_audio_results"/*.wav
adb pull /data/audio/rec "$U60_HOME/call_audio_results/" >/dev/null 2>&1
adb pull /tmp/play_test.log "$U60_HOME/call_audio_results/" >/dev/null 2>&1
adb pull /tmp/rec_dl.log "$U60_HOME/call_audio_results/" >/dev/null 2>&1
adb pull /tmp/rec_ul.log "$U60_HOME/call_audio_results/" >/dev/null 2>&1
log "pulled to $U60_HOME/call_audio_results"
