#!/bin/bash
# U60 Pro (MU5250): read firmware info, enable USB debug mode (ADB root shell), verify.
# SPDX-License-Identifier: GPL-3.0-only
# macOS-safe port of amenekowo/mu5250_tweaking files/enable_debugging.sh (GPL-3.0),
# so this file stays under GPL-3.0; the rest of the repository is MIT.
# The admin password is read interactively and never written to disk.
set -u
GATEWAY="${GATEWAY:-192.168.0.1}"

die() { echo "ERROR: $*" >&2; exit 1; }
ts() { python3 -c 'import time; print(int(time.time()*1000))'; }
ubus() { # session object method json
  curl -sf -m 10 -X POST "http://$GATEWAY/ubus/?t=$(ts)" -H 'Content-Type: application/json' \
    -H "Referer: http://$GATEWAY/" -H "Origin: http://$GATEWAY" -H 'X-Requested-With: XMLHttpRequest' \
    -d "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"call\",\"params\":[\"$1\",\"$2\",\"$3\",$4]}]"
}
ANON="00000000000000000000000000000000"

echo "== 1. Unauthenticated device info (zwrt_web web_login_info)"
INFO=$(ubus "$ANON" zwrt_web web_login_info '{"":""}') || die "cannot reach http://$GATEWAY/ubus/ (connect to the U60 Pro Wi-Fi first)"
echo "$INFO" | jq '.[0].result[1]'
SALT=$(echo "$INFO" | jq -r '.[0].result[1].zte_web_sault // empty')
[ -n "$SALT" ] || die "no zte_web_sault in response"

echo
printf "Router admin password (not echoed): "
read -rs PASSWORD; echo
[ -n "$PASSWORD" ] || die "empty password"
H1=$(printf '%s' "$PASSWORD" | openssl dgst -sha256 | awk '{print toupper($NF)}')
H2=$(printf '%s%s' "$H1" "$SALT" | openssl dgst -sha256 | awk '{print toupper($NF)}')
unset PASSWORD

echo "== 2. Login"
LOGIN=$(ubus "$ANON" zwrt_web web_login "{\"password\":\"$H2\"}") || die "login request failed"
SESSION=$(echo "$LOGIN" | jq -r '.[0].result[1].ubus_rpc_session // empty')
[ -n "$SESSION" ] || { echo "$LOGIN" | jq .; die "login failed (wrong password?)"; }
echo "session ok"

echo "== 3. Current USB mode"
ubus "$SESSION" zwrt_bsp.usb list '{}' | jq '.[0].result[1]'

echo "== 4. Enable debug mode (adds ffs.adb to the USB gadget)"
ubus "$SESSION" zwrt_bsp.usb set '{"mode":"debug"}' | jq '.[0].result'

echo "== 5. Waiting for ADB over USB (plug the USB cable into the Mac if not yet)"
adb kill-server >/dev/null 2>&1
for i in $(seq 1 30); do
  if adb devices | grep -qw device; then break; fi
  sleep 1
done
adb devices
adb devices | grep -qw device || die "adb did not see the device; check cable/port, then: adb kill-server; adb devices"

echo "== 6. Root shell checks"
adb shell 'id; uname -a; echo; uci get zwrt_common_info.common_config.integrate_version; uci get zwrt_common_info.common_config.wa_inner_version; uci get zwrt_common_info.common_config.hardware_version; echo; ubus call zwrt_bsp.usb list; ls -l /sys/kernel/config/usb_gadget/g1/configs/c.1/f6; echo; cat /proc/partitions | head -5; ls -l /dev/mmcblk0*  | head -5; df -h / /data /overlay 2>/dev/null'
echo
echo "Done. Next: ./02_backup.sh"
