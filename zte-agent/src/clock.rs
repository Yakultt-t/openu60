//! Time zone and network time for what the router shows (touchscreen, app, web panel).
//!
//! The system clock stays on UTC: the ZTE daemons assume it, and the firmware already sets it
//! from the cellular network (NITZ). What users see wrong is the *zone*: roaming networks often
//! send none or the wrong one. So the agent keeps its own setting, stored in
//! /data/openu60/time.json:
//!
//! - automatic zone: the offset the router's own network reports (NITZ), else the zone the
//!   phone app last reported, else UTC;
//! - manual zone: an IANA name from the firmware's LuCI table (DST rules included) or a fixed
//!   "UTC+08:00" offset;
//! - network time: SNTP every 6 h on top of NITZ (for when there is no cellular service), or
//!   the time typed in by hand when it is off.
//!
//! GET/PUT /api/system/time, GET /api/system/timezones.

use std::collections::HashMap;
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ubus;

const FILE: &str = "/data/openu60/time.json";
const TZDATA: &str = "/usr/lib/lua/luci/sys/zoneinfo/tzdata.lua";
const SERVERS: [&str; 4] = ["ntp.aliyun.com", "time.apple.com", "pool.ntp.org", "time.cloudflare.com"];
const SYNC_EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Settings {
    /// "auto" or "manual".
    #[serde(default = "auto")]
    pub tz_mode: String,
    /// Manual zone: IANA name ("Asia/Shanghai") or a fixed offset ("UTC+08:00").
    #[serde(default)]
    pub zone: String,
    /// The zone the phone app last reported, and its offset then (fallback for names the
    /// firmware table does not know).
    #[serde(default)]
    pub phone_zone: String,
    #[serde(default)]
    pub phone_offset_minutes: i32,
    /// SNTP on/off.
    #[serde(default = "yes")]
    pub time_auto: bool,
    #[serde(default)]
    pub last_sync: u64,
    #[serde(default)]
    pub last_sync_server: String,
    #[serde(default)]
    pub last_sync_error: String,
}

fn auto() -> String {
    "auto".into()
}
fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

static SETTINGS: LazyLock<Mutex<Settings>> = LazyLock::new(|| {
    Mutex::new(std::fs::read(FILE).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
});

/// IANA name (spaces as underscores) → POSIX TZ rule, from the firmware's LuCI table.
static ZONES: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let text = std::fs::read_to_string(TZDATA).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split('\'');
            parts.next()?;
            let name = parts.next()?;
            parts.next()?;
            let rule = parts.next()?;
            (!name.is_empty() && !rule.is_empty()).then(|| (name.replace(' ', "_"), rule.to_string()))
        })
        .collect()
});

/// rule → (unix minute, offset): `date` evaluates DST rules; asked at most once a minute.
static OFFSET_CACHE: LazyLock<Mutex<HashMap<String, (u64, i32)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn save(s: &Settings) {
    if let Ok(text) = serde_json::to_vec_pretty(s) {
        let tmp = format!("{FILE}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, FILE);
        }
    }
}

/// "UTC+08:00" / "UTC-3:30" / "UTC" → minutes.
pub fn parse_fixed(zone: &str) -> Option<i32> {
    let rest = zone.strip_prefix("UTC").or_else(|| zone.strip_prefix("GMT"))?;
    if rest.is_empty() {
        return Some(0);
    }
    let (sign, rest) = match rest.as_bytes()[0] {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

pub fn format_fixed(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}

fn rule_offset(rule: &str) -> Option<i32> {
    let minute = now() / 60;
    if let Some((at, off)) = OFFSET_CACHE.lock().unwrap().get(rule) {
        if *at == minute {
            return Some(*off);
        }
    }
    let out = std::process::Command::new("date").env("TZ", rule).arg("+%z").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.len() != 5 {
        return None;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let off = sign * (s[1..3].parse::<i32>().ok()? * 60 + s[3..5].parse::<i32>().ok()?);
    OFFSET_CACHE.lock().unwrap().insert(rule.to_string(), (minute, off));
    Some(off)
}

/// Offset now for an IANA name or fixed offset; None if unknown.
pub fn zone_offset(zone: &str) -> Option<i32> {
    if let Some(m) = parse_fixed(zone) {
        return Some(m);
    }
    let rule = ZONES.iter().find(|(n, _)| n == zone)?.1.clone();
    rule_offset(&rule)
}

fn known_zone(zone: &str) -> bool {
    parse_fixed(zone).is_some() || ZONES.iter().any(|(n, _)| n == zone)
}

/// The offset the cellular network reported (NITZ), if the firmware has one.
fn network_offset() -> Option<i32> {
    let text = ubus::uci_show("zte_nwinfo.sys_info").ok()?;
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("zte_nwinfo.sys_info.{k}=")))
            .map(|v| v.trim_matches('\'').to_string())
    };
    if get("nitz_sync_flag").as_deref() != Some("1") {
        return None;
    }
    let hours: f64 = get("nitz_timezone")?.parse().ok()?;
    (hours.abs() <= 14.0).then(|| (hours * 60.0).round() as i32)
}

pub struct Effective {
    pub offset_minutes: i32,
    /// "manual", "network", "phone" or "utc".
    pub source: &'static str,
    pub zone: String,
}

pub fn effective() -> Effective {
    let s = SETTINGS.lock().unwrap().clone();
    if s.tz_mode == "manual" {
        if let Some(off) = zone_offset(&s.zone) {
            return Effective { offset_minutes: off, source: "manual", zone: s.zone };
        }
    }
    if let Some(off) = network_offset() {
        return Effective { offset_minutes: off, source: "network", zone: format_fixed(off) };
    }
    if !s.phone_zone.is_empty() {
        let off = zone_offset(&s.phone_zone).unwrap_or(s.phone_offset_minutes);
        return Effective { offset_minutes: off, source: "phone", zone: s.phone_zone };
    }
    Effective { offset_minutes: 0, source: "utc", zone: "UTC".into() }
}

fn civil(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn status() -> Value {
    let e = effective();
    let s = SETTINGS.lock().unwrap().clone();
    let utc = now();
    json!({
        "utc": utc,
        "local": civil(utc as i64 + e.offset_minutes as i64 * 60),
        "offset_minutes": e.offset_minutes,
        "offset_label": format_fixed(e.offset_minutes),
        "source": e.source,
        "zone": e.zone,
        "tz_mode": s.tz_mode,
        "manual_zone": s.zone,
        "phone_zone": s.phone_zone,
        "network_offset_minutes": network_offset(),
        "time_auto": s.time_auto,
        "last_sync": s.last_sync,
        "last_sync_server": s.last_sync_server,
        "last_sync_error": s.last_sync_error,
    })
}

pub fn get() -> (u16, Value) {
    (200, json!({"ok": true, "data": status()}))
}

pub fn zones() -> (u16, Value) {
    let list: Vec<Value> = ZONES
        .iter()
        .map(|(n, _)| json!({"name": n, "offset_minutes": zone_offset(n)}))
        .collect();
    (200, json!({"ok": true, "data": {"zones": list}}))
}

fn bad(msg: &str) -> (u16, Value) {
    (400, json!({"ok": false, "error": msg}))
}

pub fn set(body: &[u8]) -> (u16, Value) {
    let Ok(b) = serde_json::from_slice::<Value>(body) else { return bad("invalid JSON") };
    let mut sync_now = b["sync_now"].as_bool() == Some(true);
    {
        let mut s = SETTINGS.lock().unwrap();
        let before = s.clone();
        if let Some(mode) = b["tz_mode"].as_str() {
            if mode != "auto" && mode != "manual" {
                return bad("tz_mode must be auto or manual");
            }
            s.tz_mode = mode.to_string();
        }
        if let Some(zone) = b["zone"].as_str() {
            let zone = zone.trim().replace(' ', "_");
            if !known_zone(&zone) {
                return bad("unknown time zone");
            }
            s.zone = zone;
        }
        if s.tz_mode == "manual" && s.zone.is_empty() {
            return bad("choose a zone for manual mode");
        }
        if let Some(zone) = b["phone_zone"].as_str() {
            s.phone_zone = zone.trim().replace(' ', "_");
            if let Some(off) = b["phone_offset_minutes"].as_i64() {
                s.phone_offset_minutes = off.clamp(-14 * 60, 14 * 60) as i32;
            }
        }
        if let Some(on) = b["time_auto"].as_bool() {
            if on && !s.time_auto {
                sync_now = true;
            }
            s.time_auto = on;
        }
        if let Some(t) = b["set_utc"].as_u64() {
            // A typed-in time only makes sense without network time.
            if t < 1_700_000_000 {
                return bad("time out of range");
            }
            if let Err(e) = set_clock(t as f64) {
                return (500, json!({"ok": false, "error": e}));
            }
            s.time_auto = false;
            s.last_sync_error.clear();
        }
        if *s != before {
            save(&s);
        }
    }
    if sync_now {
        sync();
    }
    get()
}

fn set_clock(unix: f64) -> Result<(), String> {
    let ts = libc::timespec { tv_sec: unix.trunc() as _, tv_nsec: (unix.fract() * 1e9) as libc::c_long };
    // SAFETY: a plain clock_settime with a valid timespec.
    let r = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if r == 0 { Ok(()) } else { Err(format!("clock_settime: {}", std::io::Error::last_os_error())) }
}

/// One SNTP query: the clock offset (server − local) in seconds.
fn sntp(server: &str) -> Result<f64, String> {
    let addr = (server, 123).to_socket_addrs().map_err(|e| e.to_string())?.find(|a| a.is_ipv4()).ok_or("no IPv4 address")?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let mut pkt = [0u8; 48];
    pkt[0] = 0x1B; // LI 0, version 3, client
    let t0 = SystemTime::now();
    sock.send_to(&pkt, addr).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 48];
    let (n, _) = sock.recv_from(&mut buf).map_err(|e| e.to_string())?;
    let t3 = SystemTime::now();
    if n < 48 || buf[1] == 0 || buf[0] & 0x07 != 4 {
        return Err("bad reply".into());
    }
    let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]) as f64;
    let frac = u32::from_be_bytes([buf[44], buf[45], buf[46], buf[47]]) as f64 / 4_294_967_296.0;
    if secs == 0.0 {
        return Err("empty timestamp".into());
    }
    let server_time = secs - 2_208_988_800.0 + frac;
    let rtt = t3.duration_since(t0).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let local = t3.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    Ok(server_time + rtt / 2.0 - local)
}

/// Asks the servers in turn and steps the clock when it is off by more than a second.
pub fn sync() {
    let mut err = String::new();
    for server in SERVERS {
        match sntp(server) {
            Ok(off) => {
                if off.abs() > 1.0 {
                    let target = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) + off;
                    if let Err(e) = set_clock(target) {
                        err = e;
                        continue;
                    }
                    eprintln!("[clock] stepped by {off:.1} s from {server}");
                }
                let mut s = SETTINGS.lock().unwrap();
                s.last_sync = now();
                s.last_sync_server = server.to_string();
                s.last_sync_error.clear();
                save(&s);
                return;
            }
            Err(e) => err = format!("{server}: {e}"),
        }
    }
    eprintln!("[clock] network time failed: {err}");
    let mut s = SETTINGS.lock().unwrap();
    s.last_sync_error = err;
    save(&s);
}

pub fn start() {
    crate::system::spawn_named("clock-sync", || {
        // Let the uplink come up after boot first.
        std::thread::sleep(Duration::from_secs(90));
        loop {
            if SETTINGS.lock().unwrap().time_auto {
                sync();
            }
            std::thread::sleep(SYNC_EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_offsets_round_trip() {
        assert_eq!(parse_fixed("UTC+08:00"), Some(480));
        assert_eq!(parse_fixed("UTC-03:30"), Some(-210));
        assert_eq!(parse_fixed("UTC"), Some(0));
        assert_eq!(parse_fixed("UTC+5"), Some(300));
        assert_eq!(parse_fixed("UTC+15:00"), None);
        assert_eq!(parse_fixed("Asia/Shanghai"), None);
        assert_eq!(format_fixed(-210), "UTC-03:30");
        assert_eq!(format_fixed(345), "UTC+05:45");
    }

    #[test]
    fn civil_formats_dates() {
        assert_eq!(civil(0), "1970-01-01 00:00:00");
        assert_eq!(civil(1_794_394_959), "2026-11-11 11:02:39");
    }
}
