#!/bin/sh
# Supervisor for zte-agent, started once at boot by start_zte_agent.sh (via rc.local).
#
# - Restarts the agent 3 s after any exit (crash, panic, OOM kill); after 5 exits within a
#   minute of starting it waits 60 s between tries so a broken binary cannot spin the CPU.
# - A watchdog asks the agent's HTTP port every 60 s; three misses in a row (3 minutes with
#   no answer at all) mean it is hung, so it is killed and restarted.
# - The log lives in RAM (/tmp): it is trimmed to its last 512 KB whenever it passes 2 MB,
#   the previous run's tail is kept in /tmp/zte-agent.prev.log.
# - /tmp/zte-agent.stop makes the supervisor exit (the deploy script uses it).
LOG=/tmp/zte-agent.log
PREV=/tmp/zte-agent.prev.log
STOP=/tmp/zte-agent.stop
BIN=/data/zte-agent

# "ubus listen" helpers forked by a dead agent can keep :9090 open; reap the orphans
# (parent pid 1) only, never the vendor's own listeners.
reap_orphans() {
    for d in /proc/[0-9]*; do
        [ "$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null | cut -c1-11)" = "ubus listen" ] || continue
        [ "$(cut -d' ' -f4 "$d/stat" 2>/dev/null)" = 1 ] && kill "${d#/proc/}" 2>/dev/null
    done
}

agent_pid() {
    for d in /proc/[0-9]*; do
        [ "$(tr '\0' '\n' < "$d/cmdline" 2>/dev/null | head -1)" = "$BIN" ] && { echo "${d#/proc/}"; return; }
    done
}

watchdog() {
    misses=0
    sleep 120
    while [ ! -f "$STOP" ]; do
        size=$(wc -c < "$LOG" 2>/dev/null || echo 0)
        if [ "$size" -gt 2097152 ]; then
            tail -c 524288 "$LOG" > "$PREV.tmp" && mv "$PREV.tmp" "$PREV" && : > "$LOG"
            echo "$(date '+%F %T') [supervisor] log trimmed" >> "$LOG"
        fi
        code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' http://127.0.0.1:9090/api/auth/login 2>/dev/null)
        if [ -z "$code" ] || [ "$code" = "000" ]; then misses=$((misses + 1)); else misses=0; fi
        if [ "$misses" -ge 3 ]; then
            pid=$(agent_pid)
            echo "$(date '+%F %T') [supervisor] no HTTP answer for 3 minutes; restarting agent ${pid:-?}" >> "$LOG"
            [ -n "$pid" ] && kill "$pid" 2>/dev/null
            sleep 5
            pid=$(agent_pid)
            [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
            misses=0
            sleep 60
        fi
        sleep 60
    done
}

rm -f "$STOP"
watchdog &
fails=0
while [ ! -f "$STOP" ]; do
    if [ -s "$LOG" ]; then tail -c 524288 "$LOG" > "$PREV.tmp" && mv "$PREV.tmp" "$PREV"; fi
    : > "$LOG"
    reap_orphans
    started=$(date +%s)
    "$BIN" >> "$LOG" 2>&1 < /dev/null
    code=$?
    echo "$(date '+%F %T') [supervisor] zte-agent exited with status $code" >> "$LOG"
    [ -f "$STOP" ] && break
    if [ $(( $(date +%s) - started )) -lt 60 ]; then fails=$((fails + 1)); else fails=0; fi
    if [ "$fails" -ge 5 ]; then sleep 60; else sleep 3; fi
done
