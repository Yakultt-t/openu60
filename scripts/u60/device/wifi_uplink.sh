#!/bin/sh
# Wi-Fi uplink (station / repeater mode) for the U60 Pro STD firmware, whose own
# zwrt_wlan sta_* path is dead (/sbin/wifi_sta_opt.sh was removed from this build).
# Installed at /data/local/tmp/wifi_uplink.sh and driven by zte-agent.
#
#   wifi_uplink.sh scan                      -> JSON array of nearby networks
#   WIFI_PSK=... wifi_uplink.sh connect SSID [BSSID]
#   wifi_uplink.sh disconnect
#   wifi_uplink.sh status                    -> JSON object
#   WIFI_PSK=... wifi_uplink.sh mode hotspot|client|repeater [SSID [BSSID]]
#   wifi_uplink.sh restore                   -> re-apply the saved mode after boot
#
IFACE=wlan4
# The phy index is not stable across reboots (phy0 after a clean boot, phy1 after a driver
# reload), so resolve it from the running AP interface or sysfs each time.
# Re-resolved every time it is needed (resolve_phy): a firmware Wi-Fi reload renumbers it.
resolve_phy() {
    PHY=$(iw dev wlan2 info 2>/dev/null | sed -n 's/.*wiphy \([0-9]*\).*/phy\1/p' | head -1)
    [ -n "$PHY" ] || PHY=$(iw dev wlan0 info 2>/dev/null | sed -n 's/.*wiphy \([0-9]*\).*/phy\1/p' | head -1)
    [ -n "$PHY" ] || PHY=$(ls /sys/class/ieee80211/ 2>/dev/null | head -1)
    [ -n "$PHY" ] || PHY=phy0
}
resolve_phy
SCAN_IFACES="wlan2 wlan0"
CONF=/tmp/wpa_uplink.conf
CTRL=/var/run/wpa_uplink
PIDFILE=/tmp/wpa_uplink.pid
DHCP_PID=/tmp/udhcpc_uplink.pid
DHCP_SCRIPT=/data/local/tmp/wifi_uplink_dhcp.sh
STATE=/tmp/wifi_uplink.state
MODE_FILE=/data/local/tmp/wifi_mode.conf
LOG=/tmp/wifi_uplink.log
ROUTE_METRIC=10

log() { echo "$(date '+%H:%M:%S') $*" >> "$LOG"; }

# kill_matching <regex>: SIGKILL processes whose command line matches (this busybox has no pkill).
kill_matching() {
    for p in $(ps w | awk -v re="$1" -v me=$$ '$1 != me && $0 ~ re && !/awk/ {print $1}'); do
        kill -9 "$p" 2>/dev/null
    done
}

json_escape() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

ensure_iface() {
    resolve_phy
    if ! iw dev "$IFACE" info >/dev/null 2>&1; then
        iw phy "$PHY" interface add "$IFACE" type managed 2>>"$LOG" || {
            log "cannot add $IFACE on $PHY (phys: $(ls /sys/class/ieee80211/ 2>/dev/null | tr '\n' ' '))"
            return 1
        }
    fi
    ifconfig "$IFACE" up 2>>"$LOG"
}

# iw scan on one AP vif, giving up after SCAN_TIMEOUT s. Right after boot the 5 GHz AP is
# in its DFS radar wait; a scan then can fail and the next one can block forever, which
# once left the boot-time restore hung for over an hour (2026-11-10). busybox here has no
# `timeout`, and a killed child lingers as a zombie, so completion is signalled by a file.
SCAN_TIMEOUT=${SCAN_TIMEOUT:-20}
scan_iface() {
    f=/tmp/wifi_scan.$1.$$
    rm -f "$f" "$f.done"
    ( iw dev "$1" scan > "$f" 2>/dev/null; touch "$f.done" ) </dev/null >/dev/null 2>&1 &
    i=0
    while [ ! -f "$f.done" ]; do
        if [ $i -ge $SCAN_TIMEOUT ]; then
            log "scan on $1 gave no answer in ${SCAN_TIMEOUT}s; killed"
            kill_matching "iw dev $1 scan"
            break
        fi
        sleep 1; i=$((i + 1))
    done
    cat "$f" 2>/dev/null
    rm -f "$f" "$f.done"
}

do_scan() {
    # Scan from the AP vifs (allowed while serving clients) so no station vif is needed yet.
    # In client mode both hotspot vifs can be down; the station vif can scan then.
    ifs=""
    for i in $SCAN_IFACES; do
        ip link show "$i" 2>/dev/null | grep -q "state UP\|,UP" && ifs="$ifs $i"
    done
    if [ -z "$ifs" ]; then
        ensure_iface >/dev/null 2>&1 && ifs="$IFACE"
    fi
    for i in $ifs; do scan_iface "$i"; done | awk '
    function flush() {
        if (bssid != "" && ssid != "") {
            key = ssid "|" bssid
            if (!(key in seen)) {
                seen[key] = 1
                sec = "Open"
                if (rsn) { sec = (sae ? "WPA3" : "WPA2") }
                else if (wpa) { sec = "WPA" }
                gsub(/\\/, "\\\\", ssid); gsub(/"/, "\\\"", ssid)
                printf "%s{\"ssid\":\"%s\",\"bssid\":\"%s\",\"freq\":%d,\"signal\":%d,\"security\":\"%s\"}", (n++ ? "," : ""), ssid, bssid, freq, signal, sec
            }
        }
        bssid = ""; ssid = ""; freq = 0; signal = 0; rsn = 0; wpa = 0; sae = 0
    }
    BEGIN { n = 0; printf "[" }
    /^BSS / { flush(); bssid = substr($2, 1, 17) }
    /^\tfreq:/ { freq = $2 }
    /^\tsignal:/ { signal = int($2) }
    /^\tSSID:/ { ssid = substr($0, 8) }
    /^\tRSN:/ { rsn = 1 }
    /^\tWPA:/ { wpa = 1 }
    /Authentication suites:.*SAE/ { sae = 1 }
    END { flush(); printf "]\n" }'
}

write_conf() {
    SSID="$1"; BSSID="$2"
    umask 077
    {
        echo "ctrl_interface=$CTRL"
        echo "network={"
        echo "    ssid=\"$SSID\""
        echo "    scan_ssid=1"
        [ -n "$BSSID" ] && echo "    bssid=$BSSID"
        if [ -n "$WIFI_PSK" ]; then
            echo "    psk=\"$WIFI_PSK\""
            echo "    key_mgmt=WPA-PSK WPA-PSK-SHA256 SAE"
            echo "    ieee80211w=1"
        else
            echo "    key_mgmt=NONE"
        fi
        echo "}"
    } > "$CONF"
}

stop_supplicant() {
    [ -f "$DHCP_PID" ] && kill "$(cat "$DHCP_PID")" 2>/dev/null; rm -f "$DHCP_PID"
    [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null; rm -f "$PIDFILE"
    kill_matching "wpa_supplicant.*-i $IFACE( |$)"
}

teardown_net() {
    ip route del default dev "$IFACE" 2>/dev/null
    ip addr flush dev "$IFACE" 2>/dev/null
    iptables -t nat -D POSTROUTING -o "$IFACE" -j MASQUERADE 2>/dev/null
    iptables -D FORWARD -o "$IFACE" -j ACCEPT 2>/dev/null
    iptables -D FORWARD -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null
    iptables -D INPUT -i "$IFACE" -p udp --dport 68 -j ACCEPT 2>/dev/null
    iptables -D INPUT -i "$IFACE" -p tcp --dport 9090 -j ACCEPT 2>/dev/null
    iptables -D INPUT -i "$IFACE" -p udp --dport 9091 -j ACCEPT 2>/dev/null
    iptables -D INPUT -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null
}

setup_firewall() {
    iptables -t nat -C POSTROUTING -o "$IFACE" -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -o "$IFACE" -j MASQUERADE
    iptables -C FORWARD -o "$IFACE" -j ACCEPT 2>/dev/null || iptables -I FORWARD -o "$IFACE" -j ACCEPT
    iptables -C FORWARD -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -I FORWARD -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
    iptables -C INPUT -i "$IFACE" -p udp --dport 68 -j ACCEPT 2>/dev/null || iptables -I INPUT -i "$IFACE" -p udp --dport 68 -j ACCEPT
    iptables -C INPUT -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -I INPUT -i "$IFACE" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
    # Lets the phone reach the agent from the home LAN side too.
    iptables -C INPUT -i "$IFACE" -p tcp --dport 9090 -j ACCEPT 2>/dev/null || iptables -I INPUT -i "$IFACE" -p tcp --dport 9090 -j ACCEPT
    # Call audio (zte-agent voice bridge, UDP).
    iptables -C INPUT -i "$IFACE" -p udp --dport 9091 -j ACCEPT 2>/dev/null || iptables -I INPUT -i "$IFACE" -p udp --dport 9091 -j ACCEPT
}

do_connect() {
    SSID="$1"; BSSID="$2"
    [ -n "$SSID" ] || { echo '{"ok":false,"error":"missing ssid"}'; return 1; }
    log "connect ssid=$SSID bssid=${BSSID:-any}"
    stop_supplicant
    teardown_net
    ensure_iface || { echo '{"ok":false,"error":"cannot create station interface"}'; return 1; }
    write_conf "$SSID" "$BSSID"
    mkdir -p "$CTRL"
    wpa_supplicant -B -i "$IFACE" -D nl80211 -c "$CONF" -P "$PIDFILE" -f "$LOG" || {
        echo '{"ok":false,"error":"wpa_supplicant failed to start"}'; return 1; }
    i=0
    while [ $i -lt 30 ]; do
        st=$(wpa_cli -p "$CTRL" -i "$IFACE" status 2>/dev/null | sed -n 's/^wpa_state=//p')
        [ "$st" = "COMPLETED" ] && break
        sleep 1; i=$((i + 1))
    done
    if [ "$st" != "COMPLETED" ]; then
        reason=$(grep -oE "(WRONG_KEY|CTRL-EVENT-ASSOC-REJECT.*|CTRL-EVENT-AUTH-REJECT.*|No suitable network found)" "$LOG" | tail -1)
        log "association failed: state=$st $reason"
        stop_supplicant
        printf '{"ok":false,"error":"association failed (%s)%s"}\n' "${st:-unknown}" "${reason:+: $reason}"
        return 1
    fi
    log "associated, requesting DHCP"
    setup_firewall
    udhcpc -i "$IFACE" -p "$DHCP_PID" -s "$DHCP_SCRIPT" -b -t 6 -T 3 -O staticroutes >/dev/null 2>&1
    i=0
    while [ $i -lt 20 ]; do
        ip -4 addr show dev "$IFACE" 2>/dev/null | grep -q "inet " && break
        sleep 1; i=$((i + 1))
    done
    printf 'ssid=%s\nbssid=%s\n' "$SSID" "$BSSID" > "$STATE"
    do_status
}

# --- hotspot BSS control (hostapd_cli, no firmware reload) ----------------------
# The driver allows one station + access points only across bands ("sap-sta conc will
# fail" on the same band), and the firmware's reload tears the station down, so the
# hotspot is switched per band in place through hostapd.
HAPD_CTRL=/data/vendor/wifi/hostapd

bss_set() {   # bss_set <2g|5g> <on|off>
    case "$1" in 2g) ifn=wlan0 ;; 5g) ifn=wlan2 ;; *) return 1 ;; esac
    if [ "$2" = on ]; then hostapd_cli -p "$HAPD_CTRL" -i "$ifn" enable >/dev/null 2>&1
    else hostapd_cli -p "$HAPD_CTRL" -i "$ifn" disable >/dev/null 2>&1; fi
    log "hotspot $1 -> $2"
}

# Full firmware Wi-Fi reload: rebuilds hostapd for both bands (and destroys the station
# vif, so callers reconnect afterwards). Used when hostapd refuses to re-enable a BSS.
wifi_reload() {
    log "firmware wifi reload (forced off/on so hostapd is rebuilt)"
    # The firmware's reload only restarts hostapd for a band whose config changed, so a
    # BSS that hostapd left in DISABLED would survive a plain reload. Toggle both off/on.
    uci set wireless.wifi0.disabled=1; uci set wireless.main_2g.disabled=1
    uci set wireless.wifi1.disabled=1; uci set wireless.main_5g.disabled=1
    uci commit wireless
    ubus call zwrt_wlan reload >/dev/null 2>&1
    sleep 8
    uci set wireless.wifi0.disabled=0; uci set wireless.main_2g.disabled=0
    uci set wireless.wifi1.disabled=0; uci set wireless.main_5g.disabled=0
    uci commit wireless
    ubus call zwrt_wlan reload >/dev/null 2>&1
    i=0
    while [ $i -lt 60 ]; do
        [ "$(bss_state 2g)" = on ] && [ "$(bss_state 5g)" = on ] && break
        sleep 2; i=$((i + 2))
    done
    sleep 2
}

# Make sure the hotspot bands requested are really up; fall back to a reload if not.
# ensure_bss <2g on|off> <5g on|off>  -> returns 1 if a reload happened (station is gone)
ensure_bss() {
    bss_set 2g "$1"; bss_set 5g "$2"; sleep 3
    ok=1
    [ "$1" = on ] && [ "$(bss_state 2g)" != on ] && ok=0
    [ "$2" = on ] && [ "$(bss_state 5g)" != on ] && ok=0
    [ $ok = 1 ] && return 0
    log "hotspot band failed to come up (2g=$(bss_state 2g) 5g=$(bss_state 5g)); reloading"
    stop_supplicant; teardown_net
    wifi_reload
    bss_set 2g "$1"; bss_set 5g "$2"; sleep 2
    return 1
}

bss_state() {   # prints on/off for <2g|5g>
    case "$1" in 2g) ifn=wlan0 ;; *) ifn=wlan2 ;; esac
    st=$(hostapd_cli -p "$HAPD_CTRL" -i "$ifn" status 2>/dev/null | sed -n 's/^state=//p')
    [ "$st" = "ENABLED" ] && echo on || echo off
}

scan_band() {   # 2g|5g for a network seen in one scan, empty when it was not seen
    sc=$(do_scan | tr '{' '\n' | grep -F "\"ssid\":\"$(printf '%s' "$1" | sed 's/[\\"]/\\&/g')\"")
    f=$(printf '%s\n' "$sc" | grep -F "$2" | sed -n 's/.*"freq":\([0-9]*\).*/\1/p' | head -1)
    [ -z "$f" ] && f=$(printf '%s\n' "$sc" | sed -n 's/.*"freq":\([0-9]*\).*/\1/p' | head -1)
    [ -n "$f" ] || return 0
    [ "$f" -ge 4900 ] && echo 5g || echo 2g
}

target_band() {   # 2g|5g for a network; when no scan sees it, the band it was last joined on
    b=$(scan_band "$1" "$2")
    [ -z "$b" ] && b=$(scan_band "$1" "$2")
    [ -z "$b" ] && b=$(sed -n 's/^band=//p' "$MODE_FILE" 2>/dev/null | head -1)
    echo "${b:-2g}"
}

# --- hotspot radios (firmware reload; only used to recover a fully-off state) ---------
# wifi0 = 2.4 GHz, wifi1 = 5 GHz. Changes go through the firmware's own reload so its
# hostapd/bridge bookkeeping stays consistent.
wait_for_wifi() {   # the firmware reload tears the whole stack down; wait for the 5G AP vif
    i=0
    while [ $i -lt 60 ]; do
        iw dev wlan2 info >/dev/null 2>&1 && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}

set_radios() {   # set_radios <2g on|off> <5g on|off>
    d2=1; d5=1
    [ "$1" = on ] && d2=0
    [ "$2" = on ] && d5=0
    cur2=$(uci -q get wireless.wifi0.disabled); cur5=$(uci -q get wireless.wifi1.disabled)
    [ "$cur2" = "$d2" ] && [ "$cur5" = "$d5" ] && return 0
    uci set wireless.wifi0.disabled="$d2"
    uci set wireless.main_2g.disabled="$d2"
    uci set wireless.wifi1.disabled="$d5"
    uci set wireless.main_5g.disabled="$d5"
    uci commit wireless
    log "radios: 2g=$1 5g=$2 (reload)"
    ubus call zwrt_wlan reload >/dev/null 2>&1 &
}

uplink_band() {   # prints 2g or 5g for the current association
    f=$(wpa_cli -p "$CTRL" -i "$IFACE" status 2>/dev/null | sed -n 's/^freq=//p')
    [ -n "$f" ] && [ "$f" -ge 4900 ] && echo 5g || echo 2g
}

save_mode() {     # save_mode <mode> <ssid> <bssid> [band]
    umask 077
    printf 'mode=%s\nssid=%s\nbssid=%s\npsk=%s\n' "$1" "$2" "$3" "$WIFI_PSK" > "$MODE_FILE"
    [ -n "$4" ] && echo "band=$4" >> "$MODE_FILE"
}

remember_band() {   # keep the band of the current association for scans that come up empty
    b=$(uplink_band)
    sed -i '/^band=/d' "$MODE_FILE" 2>/dev/null
    echo "band=$b" >> "$MODE_FILE"
}

current_mode() { sed -n 's/^mode=//p' "$MODE_FILE" 2>/dev/null | head -1; }

do_mode() {
    mode="$1"; ssid="$2"; bssid="$3"
    case "$mode" in
        hotspot)
            log "mode hotspot"
            # Answer first, then switch: leaving the Wi-Fi takes the request's way back with
            # it when the app reaches the router there, and bringing both bands up can need
            # a firmware reload (over a minute), longer than any client waits.
            ( sleep 1; do_disconnect ) >/dev/null 2>&1 &
            printf '{"ok":true,"mode":"hotspot","pending":true}\n'
            ;;
        client|repeater)
            [ -n "$ssid" ] || { echo '{"ok":false,"error":"missing ssid"}'; return 1; }
            # Already on this network: only the router's own hotspot changes (repeater keeps
            # it on the free band, client turns it off), no reconnect.
            cur=$(do_status)
            if printf '%s' "$cur" | grep -q '"connected":true' && \
               [ "$(printf '%s' "$cur" | sed -n 's/.*"ssid":"\([^"]*\)".*/\1/p')" = "$(json_escape "$ssid")" ]; then
                freq=$(printf '%s' "$cur" | sed -n 's/.*"freq":\([0-9]*\).*/\1/p')
                other=5g; [ "${freq:-0}" -ge 4900 ] && other=2g
                if [ "$mode" = repeater ]; then bss_set "$other" on; else bss_set "$other" off; fi
                [ -z "${WIFI_PSK:-}" ] && WIFI_PSK=$(sed -n 's/^psk=//p' "$MODE_FILE" 2>/dev/null)
                save_mode "$mode" "$ssid" "$bssid" "$( [ "${freq:-0}" -ge 4900 ] && echo 5g || echo 2g)"
                log "mode $mode on the current network (hotspot $other $( [ "$mode" = repeater ] && echo on || echo off))"
                do_status
                return 0
            fi
            band=$(target_band "$ssid" "$bssid")
            # Free the uplink band: the hotspot on the other band keeps serving the phone.
            if [ "$band" = 5g ]; then ensure_bss on off; else ensure_bss off on; fi
            sleep 2
            out=$(do_connect "$ssid" "$bssid")
            if ! printf '%s\n' "$out" | grep -q '"up":true'; then
                ensure_bss on on
                printf '%s\n' "$out"; return 1
            fi
            save_mode "$mode" "$ssid" "$bssid" "$band"
            if [ "$mode" = client ]; then
                # Give the app ~20 s to read the uplink IP before the last hotspot band goes away.
                other=2g; [ "$band" = 2g ] && other=5g
                ( sleep 20; bss_set "$other" off ) >/dev/null 2>&1 &
            fi
            printf '%s\n' "$out"
            ;;
        *) echo '{"ok":false,"error":"unknown mode"}'; return 1 ;;
    esac
}

do_restore() {
    [ -f "$MODE_FILE" ] || exit 0
    mode=$(current_mode); ssid=$(sed -n 's/^ssid=//p' "$MODE_FILE"); bssid=$(sed -n 's/^bssid=//p' "$MODE_FILE")
    WIFI_PSK=$(sed -n 's/^psk=//p' "$MODE_FILE"); export WIFI_PSK
    log "restore mode=$mode ssid=$ssid"
    [ "$mode" = hotspot ] && exit 0
    # Safety: after a boot, bring the uplink back but leave BOTH hotspot radios on, so the
    # phone can always reach the router even if the uplink or the app fails. The app can
    # re-apply client/repeater mode (which turns radios off) once it is back in touch.
    wait_for_wifi
    # The 5 GHz AP spends about a minute in its DFS radar wait after boot, and scans during
    # it can come back empty; give the home network a few tries to show up.
    band=""; n=0
    while [ -z "$band" ] && [ $n -lt 4 ]; do
        [ $n -gt 0 ] && sleep 15
        band=$(scan_band "$ssid" "$bssid"); n=$((n + 1))
    done
    [ -z "$band" ] && band=$(sed -n 's/^band=//p' "$MODE_FILE" | head -1) && log "restore: $ssid not seen in $n scans; using saved band ${band:-none}"
    [ -n "$band" ] || band=2g
    if [ "$band" = 5g ]; then ensure_bss on off; else ensure_bss off on; fi
    sleep 2
    out=$(do_connect "$ssid" "$bssid")
    log "restore result: $out"
    if printf '%s\n' "$out" | grep -q '"up":true'; then
        remember_band
        # Client mode means the router's own hotspot is off. The other band stays on through
        # the restore so the router can be reached if the uplink fails; once the uplink has
        # held for two minutes the mode the owner chose applies again. (It used to stay on
        # for good, and the app then showed the router as a repeater after every reboot.)
        if [ "$mode" = client ]; then
            other=2g; [ "$band" = 2g ] && other=5g
            ( sleep 120
              [ "$(current_mode)" = client ] || exit 0
              do_status | grep -q '"up":true' || exit 0
              bss_set "$other" off
              log "restore: uplink held for 2 min; client mode, hotspot $other off" ) >/dev/null 2>&1 &
        fi
    else
        ensure_bss on on
    fi
}

do_disconnect() {
    log "disconnect"
    stop_supplicant
    teardown_net
    rm -f "$STATE" "$CONF"
    ifconfig "$IFACE" down 2>/dev/null
    sed -i '/# uplink$/d' /tmp/resolv.conf.d/resolv.conf.auto 2>/dev/null
    save_mode hotspot "" ""
    ensure_bss on on
    echo '{"ok":true}'
}

do_status() {
    st=$(wpa_cli -p "$CTRL" -i "$IFACE" status 2>/dev/null)
    state=$(printf '%s\n' "$st" | sed -n 's/^wpa_state=//p')
    cssid=$(printf '%s\n' "$st" | sed -n 's/^ssid=//p')
    cbssid=$(printf '%s\n' "$st" | sed -n 's/^bssid=//p')
    freq=$(printf '%s\n' "$st" | sed -n 's/^freq=//p')
    ipaddr=$(ip -4 addr show dev "$IFACE" 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)
    gw=$(ip route show dev "$IFACE" 2>/dev/null | sed -n 's/^default via \([0-9.]*\).*/\1/p' | head -1)
    sig=$(iw dev "$IFACE" link 2>/dev/null | sed -n 's/.*signal: \(-\?[0-9]*\).*/\1/p')
    connected=false; [ "$state" = "COMPLETED" ] && connected=true
    up=false; [ "$connected" = true ] && [ -n "$ipaddr" ] && up=true
    mode=$(current_mode); [ -n "$mode" ] || mode=hotspot
    r2=$(bss_state 2g); r5=$(bss_state 5g)
    printf '{"ok":true,"connected":%s,"up":%s,"state":"%s","ssid":"%s","bssid":"%s","freq":%s,"signal":%s,"ip":"%s","gateway":"%s","mode":"%s","hotspot_2g":"%s","hotspot_5g":"%s"}\n' \
        "$connected" "$up" "${state:-DISCONNECTED}" "$(json_escape "$cssid")" "$cbssid" "${freq:-0}" "${sig:-0}" "$ipaddr" "$gw" "$mode" "$r2" "$r5"
}

case "$1" in
    scan) do_scan ;;
    connect) do_connect "$2" "$3" ;;
    disconnect) do_disconnect ;;
    status) do_status ;;
    mode) do_mode "$2" "$3" "$4" ;;
    restore) do_restore ;;
    *) echo "usage: $0 scan|connect SSID [BSSID]|disconnect|status" >&2; exit 2 ;;
esac
