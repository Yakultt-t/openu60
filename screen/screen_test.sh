#!/bin/sh
# Screen bring-up test: stops the stock UI, runs screen-test for N seconds, then always
# restores the stock UI (also on error or if the test is killed). Run detached.
N="${1:-12}"
ROT="${2:-0}"
LOG=/tmp/screen_test.log
BL=/sys/class/leds/led:lcd/brightness
saved=$(cat "$BL" 2>/dev/null)
restore() {
    /etc/init.d/zte_topsw_devui start >/dev/null 2>&1
    sleep 2
    pidof zte_topsw_devui >/dev/null || /etc/init.d/zte_topsw_devui restart >/dev/null 2>&1
    [ -n "$saved" ] && [ "$saved" != 0 ] && echo "$saved" > "$BL"
    echo "restored: devui pid $(pidof zte_topsw_devui)" >> "$LOG"
}
trap "" HUP
trap restore EXIT INT TERM
echo "start $(date) brightness=$saved" > "$LOG"
/etc/init.d/zte_topsw_devui stop >/dev/null 2>&1
for i in 1 2 3 4 5 6 7 8 9 10; do pidof zte_topsw_devui >/dev/null || break; sleep 1; done
pidof zte_topsw_devui >/dev/null && { echo "devui still running, abort" >> "$LOG"; exit 1; }
echo "devui stopped after ${i}s" >> "$LOG"
/data/local/tmp/screen-test "$N" "$ROT" >> "$LOG" 2>&1
echo "test exit $?" >> "$LOG"
