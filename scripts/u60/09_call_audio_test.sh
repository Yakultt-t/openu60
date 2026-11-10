#!/bin/bash
# Automated call-audio test: restart the device probe with a VSID, dial a number through the agent,
# wait while the probe records both directions + plays beeps, hang up, pull and analyse the wav.
# usage: ./09_call_audio_test.sh <VSID> <number> <seconds>
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
VSID="${1:-11C05000}"; NUM="${2:-1194}"; SECS="${3:-25}"
PW=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "$U60_HOME/agent-credentials.txt")
adb forward tcp:19090 tcp:9090 >/dev/null
TOKEN=$(curl -sf -m 5 http://127.0.0.1:19090/api/auth/login -H 'Content-Type: application/json' \
  --data-binary "$(printf '%s' "$PW" | python3 -c 'import json,sys; print(json.dumps({"password": sys.stdin.read()}))')" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["token"])') || { echo "login failed"; exit 1; }
api() { curl -sf -m 5 "http://127.0.0.1:19090$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "${@:2}"; }
calls() { api /api/call/status | python3 -c 'import json,sys
for c in json.load(sys.stdin)["data"]["calls"]:
    if c.get("mode")==0: print(c["stat"], c.get("number",""))' 2>/dev/null; }
echo "== restart probe with VSID $VSID"
adb shell "for p in \$(pgrep zte_route_hal_voice_test); do kill -INT \$p; done; sleep 2; for p in \$(pgrep zte_route_hal_voice_test); do kill -9 \$p; done; rm -f /tmp/call_rec.wav; start-stop-daemon -S -b -x /data/local/tmp/voice_probe_run.sh -- $VSID; sleep 4; renice 15 -p \$(pgrep zte_route_hal_voice_test) >/dev/null 2>&1; pgrep -l zte_route_hal_voice_test; tail -c 120 /tmp/voice_graph.log 2>/dev/null"
echo "== dial $NUM"
api /api/call/dial --data-binary "{\"number\":\"$NUM\"}"; echo
t=0; st=""
while [ $t -lt 40 ]; do st=$(calls | head -1); echo "  t=${t}s: ${st:-none}"; case "$st" in active*) break;; esac; sleep 2; t=$((t+2)); done
case "$st" in active*) ;; *) echo "call never became active"; api /api/call/hangup; exit 2;; esac
echo "== active; recording for $SECS s (wav size every 5 s)"
for i in $(seq 5 5 $SECS); do sleep 5; echo "  +${i}s: $(adb shell 'stat -c %s /tmp/call_rec.wav 2>/dev/null || echo nofile') bytes, call: $(calls | head -1)"; done
echo "== hangup"; api /api/call/hangup; echo
sleep 2
adb shell "for p in \$(pgrep zte_route_hal_voice_test); do kill -INT \$p; done; sleep 2; ls -la /tmp/call_rec.wav"
mkdir -p "$U60_HOME/call_audio_results"; adb pull /tmp/call_rec.wav "$U60_HOME/call_audio_results/call_${VSID}_${NUM}.wav" >/dev/null 2>&1
python3 - "$U60_HOME/call_audio_results/call_${VSID}_${NUM}.wav" <<'PY'
import sys,wave,struct,math,os
p=sys.argv[1]
if not os.path.exists(p) or os.path.getsize(p)<100: print("RESULT: no audio data recorded"); sys.exit()
w=wave.open(p,'rb'); n=w.getnframes(); sr=w.getframerate(); ch=w.getnchannels(); s=struct.unpack('<%dh'%(n*ch), w.readframes(n))
print(f"RESULT: frames={n} sr={sr} ch={ch} dur={n/sr:.1f}s")
blk=sr; 
for i in range(0,len(s)-blk,blk*ch):
    seg=s[i:i+blk*ch]; rms=math.sqrt(sum(x*x for x in seg)/len(seg)); peak=max(abs(x) for x in seg)
    print(f"  t={i//(sr*ch):3d}s rms={rms:7.1f} peak={peak:6d}")
PY
