#!/bin/sh
# U60 Pro call-audio probe: keeps the DSP voice graph up through the Qualcomm voice test tool,
# records downlink+uplink to /tmp/call_rec.wav and plays a beep file into the uplink.
# usage: audio_probe.sh start|stop|status
TOOL=zte_route_hal_voice_test
case "$1" in
  start)
    for p in $(pgrep $TOOL); do kill -9 $p; done
    rm -f /tmp/call_rec.wav /tmp/probe.log
    : > /tmp/syslog.full 2>/dev/null
    logger -t MARK "audio probe start"
    # speaker + builtin-mic device pair (2,0x80000004): the only pair whose AUX PCM endpoint calibration is valid
    ( $TOOL -i 11C05000 -d 2,2147483652 -r /tmp/call_rec.wav -t 2 -p /tmp/tone900.wav > /tmp/probe.log 2>&1 & )
    sleep 3
    echo "probe started: $(pgrep $TOOL | head -1)"
    grep -c "Start Success" /tmp/syslog.full 2>/dev/null
    ;;
  stop)
    for p in $(pgrep $TOOL); do kill -INT $p; done; sleep 2
    for p in $(pgrep $TOOL); do kill -9 $p; done
    logger -t MARK "audio probe stop"
    ls -la /tmp/call_rec.wav 2>&1
    ;;
  status)
    pgrep -l $TOOL || echo "not running"
    ls -la /tmp/call_rec.wav 2>&1
    tail -c 300 /tmp/probe.log 2>/dev/null; echo
    ;;
esac
