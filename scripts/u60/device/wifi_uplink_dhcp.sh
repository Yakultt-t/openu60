#!/bin/sh
# udhcpc hook for the Wi-Fi uplink: configure the address and a low-metric default route
# so traffic prefers the home Wi-Fi over the cellular bearer while it is up.
METRIC=10
case "$1" in
    deconfig)
        ip addr flush dev "$interface" 2>/dev/null
        ip route del default dev "$interface" 2>/dev/null
        sed -i '/# uplink$/d' /tmp/resolv.conf.d/resolv.conf.auto 2>/dev/null
        ;;
    bound|renew)
        ip addr flush dev "$interface" 2>/dev/null
        ip addr add "$ip/${mask:-24}" dev "$interface"
        if [ -n "$router" ]; then
            for r in $router; do ip route replace default via "$r" dev "$interface" metric $METRIC; break; done
        fi
        echo "$dns" > /tmp/wifi_uplink.dns
        # dnsmasq re-reads this file by itself: put the uplink resolvers first so LAN
        # clients keep resolving names when cellular data is off.
        R=/tmp/resolv.conf.d/resolv.conf.auto
        if [ -n "$dns" ] && [ -f "$R" ]; then
            sed -i '/# uplink$/d' "$R"
            tmp=$(mktemp); for d in $dns; do echo "nameserver $d # uplink"; done > "$tmp"; cat "$R" >> "$tmp"; cat "$tmp" > "$R"; rm -f "$tmp"
        fi
        ;;
esac
exit 0
