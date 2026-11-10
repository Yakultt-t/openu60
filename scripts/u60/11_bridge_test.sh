#!/bin/bash
# End-to-end test of the agent's call-audio bridge from the device itself (no phone needed):
# dials <number>, opens an audio session, runs the Lua test client (beeps up, records down),
# then pulls the downlink recording to call_audio_results/bridge_<number>.wav and analyses it.
# usage: ./11_bridge_test.sh <number|none> <seconds>
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
NUM="${1:?usage: $0 <number|none> <seconds>}"; SECS="${2:-20}"
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
status() { api /api/call/audio/status | python3 -c 'import json,sys; d=json.load(sys.stdin)["data"]; print("  audio:", {k:d.get(k) for k in ("active","call_state","odu_running","odu_starts","phone","rx_phone","tx_phone","rx_dsp","dropped","bad_key")})'; }
adb shell 'rm -f /tmp/bridge_dl.ulaw /tmp/bridge_client.log'
if [ "$NUM" != "none" ]; then
  echo "== dial $NUM"; api /api/call/dial --data-binary "{\"number\":\"$NUM\"}"; echo
fi
echo "== audio session"; S=$(api /api/call/audio/start -X POST); echo "$S"
KEY=$(echo "$S" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["key"])')
if [ "$NUM" != "none" ]; then
  for i in $(seq 1 20); do ST=$(calls | head -1); echo "  t=$((i*2))s: ${ST:-none}"; case "$ST" in active*) break;; esac; sleep 2; done
fi
echo "== client for $SECS s"
adb shell "start-stop-daemon -S -b -x /data/local/tmp/bridge_client_run.sh -- $KEY $SECS; true"
for i in $(seq 2 2 "$SECS"); do sleep 2; printf "  +%ss call: %s" "$i" "$(calls | head -1)"; status; done
sleep 2; adb shell 'cat /tmp/bridge_client.log'
if [ "$NUM" != "none" ]; then echo "== hangup"; api /api/call/hangup; echo; fi
sleep 1; status
echo "== agent log:"; adb shell 'grep -a "\[voice\]\|\[call\]" /tmp/zte-agent.log | tail -12'
adb pull /tmp/bridge_dl.ulaw "$OUT/bridge_$NUM.ulaw" >/dev/null 2>&1 && python3 - "$OUT/bridge_$NUM.ulaw" "$OUT/bridge_$NUM.wav" <<'PY'
import sys, wave, struct, math
raw = open(sys.argv[1], 'rb').read()
def ulaw(b):
    b = ~b & 0xff; s = b & 0x80; e = (b >> 4) & 7; m = b & 0xf
    x = ((m << 3) + 0x84) << e; x -= 0x84
    return -x if s else x
pcm = [ulaw(b) for b in raw]
w = wave.open(sys.argv[2], 'wb'); w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000)
w.writeframes(struct.pack('<%dh' % len(pcm), *pcm)); w.close()
print("downlink: %.1f s" % (len(pcm)/8000))
for i in range(0, len(pcm), 8000):
    seg = pcm[i:i+8000]
    if not seg: break
    rms = math.sqrt(sum(x*x for x in seg)/len(seg))
    print("  sec %2d rms %5d nonzero %3.0f%%" % (i//8000, rms, 100*sum(1 for x in seg if x)/len(seg)))
PY
