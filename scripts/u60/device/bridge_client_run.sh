#!/bin/sh
# $1 key $2 secs: run the bridge test client detached
exec lua /data/local/tmp/bridge_client.lua "$1" "${2:-20}" /tmp/beep.ulaw /tmp/bridge_dl.ulaw >/tmp/bridge_client.log 2>&1 </dev/null
