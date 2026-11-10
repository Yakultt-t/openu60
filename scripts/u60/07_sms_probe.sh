#!/bin/bash
# Verify the STD firmware's SMS encryption from the Mac: logs into the router web API with the
# admin password (prompted, never stored), registers a session key and decrypts the SMS list.
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
printf "Router admin password (not echoed): "; read -rs ROUTER_PW; echo
AGENT_PW=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "$U60_HOME/agent-credentials.txt")
ROUTER_PW="$ROUTER_PW" AGENT_PW="$AGENT_PW" node "$HERE/tools_sms_probe.mjs"
