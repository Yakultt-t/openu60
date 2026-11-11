//! Fills `Status` from zte-agent over loopback (local token in /tmp). Live throughput and the
//! clock run on their own one-second ticker, so a slow agent call never freezes the speed.
//! On mobile data the speed comes from the modem (`zwrt_data get_wwandst`), which counts the
//! traffic the IPA hardware forwards past the kernel; /proc/net/dev would miss it and lag a
//! speed test. On a Wi-Fi uplink it is that interface's kernel counters.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::model::Status;

const BASE: &str = "http://127.0.0.1:9090";
const TOKEN_FILE: &str = "/tmp/openu60-local.token";
const HISTORY: usize = 60;

/// Set by actions: poll everything on the next tick instead of waiting.
pub static REFRESH: AtomicBool = AtomicBool::new(false);
/// Read the messages on the next tick (a new message arrived, or the page asked).
pub static SMS_REFRESH: AtomicBool = AtomicBool::new(false);
/// Screen on: only then is the modem asked for its rate every second.
pub static AWAKE: AtomicBool = AtomicBool::new(true);
/// Minutes from UTC to show the clock in; NO_TZ until known.
static TZ_MIN: AtomicI32 = AtomicI32::new(NO_TZ);
const NO_TZ: i32 = i32::MIN;
/// Interface of the Wi-Fi uplink when it carries the traffic; empty on mobile data.
static WAN_DEV: Mutex<String> = Mutex::new(String::new());

pub struct Api {
    agent: ureq::Agent,
    token: String,
}

impl Api {
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(45))
    }

    fn with_timeout(t: Duration) -> Self {
        let config = ureq::Agent::config_builder().timeout_global(Some(t)).http_status_as_error(false).build();
        Self { agent: config.into(), token: String::new() }
    }

    fn token(&mut self) -> Option<String> {
        if self.token.is_empty() {
            self.token = std::fs::read_to_string(TOKEN_FILE).ok()?.trim().to_string();
        }
        Some(format!("Bearer {}", self.token))
    }

    /// POST/PUT with a JSON body; `Some(data)` when the agent answered ok.
    pub fn send(&mut self, method: &str, path: &str, body: Value) -> Option<Value> {
        let auth = self.token()?;
        let url = format!("{BASE}{path}");
        let req = match method {
            "PUT" => self.agent.put(&url),
            _ => self.agent.post(&url),
        };
        let mut resp = req.header("Authorization", &auth).send_json(&body).ok()?;
        if resp.status().as_u16() == 401 {
            self.token.clear();
            return None;
        }
        let v: Value = resp.body_mut().read_json().ok()?;
        (v["ok"].as_bool() == Some(true)).then(|| v["data"].clone())
    }

    pub fn get(&mut self, path: &str) -> Option<Value> {
        let auth = self.token()?;
        let mut resp = self.agent.get(&format!("{BASE}{path}")).header("Authorization", &auth).call().ok()?;
        if resp.status().as_u16() == 401 {
            // The agent restarted with a new token.
            self.token.clear();
            return None;
        }
        let v: Value = resp.body_mut().read_json().ok()?;
        if v["ok"].as_bool() == Some(true) { Some(v["data"].clone()) } else { None }
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().trim_end_matches('.').parse().ok()))
}

/// Interface counters from /proc/net/dev: (rx_bytes, tx_bytes).
fn counters(pred: impl Fn(&str) -> bool) -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if !pred(name.trim()) {
            continue;
        }
        let cols: Vec<u64> = rest.split_whitespace().filter_map(|c| c.parse().ok()).collect();
        if cols.len() >= 9 {
            rx += cols[0];
            tx += cols[8];
        }
    }
    (rx, tx)
}

/// Local time at an offset from UTC (the system clock runs UTC).
fn local_time(offset_min: i32) -> (u32, u32, u32, u32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let t = now + offset_min as i64 * 60;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let weekday = ((days + 4).rem_euclid(7)) as u32; // 1970-01-01 was a Thursday
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((secs / 3600) as u32, ((secs % 3600) / 60) as u32, month, day, weekday)
}

/// SMS body: UCS-2 hex (UTF-16BE, 4 hex digits per unit), as the firmware stores it.
fn decode_ucs2(hex: &str) -> String {
    let units: Vec<u16> = hex
        .as_bytes()
        .chunks(4)
        .filter_map(|q| std::str::from_utf8(q).ok().filter(|q| q.len() == 4).and_then(|q| u16::from_str_radix(q, 16).ok()))
        .collect();
    String::from_utf16_lossy(&units)
}

/// "26,11,11,18,29,00,+32" (yy,MM,dd,HH,mm,ss,tz) -> "11-11 18:29".
fn sms_time(date: &str) -> String {
    let p: Vec<&str> = date.split(',').map(str::trim).collect();
    if p.len() >= 5 { format!("{}-{} {}:{}", p[1], p[2], p[3], p[4]) } else { date.to_string() }
}

fn parse_sms(v: &Value) -> Vec<crate::model::Sms> {
    let mut out: Vec<crate::model::Sms> = v["messages"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| {
            let id = m["id"].as_i64().or_else(|| m["id"].as_str().and_then(|s| s.parse().ok()))?;
            let tag = m["tag"].as_str().map(str::to_string).or_else(|| m["tag"].as_i64().map(|t| t.to_string())).unwrap_or_default();
            if tag == "4" {
                return None; // draft
            }
            let raw = m["content"].as_str().unwrap_or("");
            let text = decode_ucs2(raw);
            Some(crate::model::Sms {
                id,
                number: m["number"].as_str().unwrap_or("").to_string(),
                text: if text.trim().is_empty() { raw.to_string() } else { text },
                time: sms_time(m["date"].as_str().unwrap_or("")),
                incoming: tag == "0" || tag == "1",
                unread: tag == "1",
            })
        }).collect())
        .unwrap_or_default();
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// The modem's own rate for the data bearer, in bits per second (down, up).
fn modem_rate() -> Option<(f64, f64)> {
    let out = std::process::Command::new("ubus")
        .args(["call", "zwrt_data", "get_wwandst", r#"{"cid":1,"type":1}"#])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((num(&v["real_rx_speed"])? * 8.0, num(&v["real_tx_speed"])? * 8.0))
}

/// One-second ticker: throughput and the clock, independent of the agent polls.
fn ticker(status: Arc<Mutex<Status>>) {
    let mut last: Option<(Instant, String, u64, u64)> = None;
    let mut next = Instant::now();
    loop {
        let dev = WAN_DEV.lock().unwrap().clone();
        let now = Instant::now();
        let (rx, tx) = if dev.is_empty() { counters(|n| n.starts_with("rmnet_data")) } else { counters(|n| n == dev) };
        let mut rate = None;
        if let Some((t0, dev0, rx0, tx0)) = &last {
            if *dev0 == dev {
                let dt = now.duration_since(*t0).as_secs_f64().max(0.2);
                rate = Some((rx.saturating_sub(*rx0) as f64 * 8.0 / dt, tx.saturating_sub(*tx0) as f64 * 8.0 / dt));
            }
        }
        last = Some((now, dev.clone(), rx, tx));
        if dev.is_empty() && AWAKE.load(Ordering::Relaxed) {
            rate = modem_rate().or(rate);
        }
        let (down, up) = rate.unwrap_or((0.0, 0.0));

        let tz = TZ_MIN.load(Ordering::Relaxed);
        let tz = if tz == NO_TZ { 0 } else { tz };
        let (h, m, mo, d, wd) = local_time(tz);
        {
            let mut s = status.lock().unwrap();
            s.hour = h;
            s.minute = m;
            s.month = mo;
            s.day = d;
            s.weekday = wd;
            s.tz_hours = tz as f64 / 60.0;
            s.down_bps = down;
            s.up_bps = up;
            s.down_hist.push(down as f32);
            s.up_hist.push(up as f32);
            if s.down_hist.len() > HISTORY {
                s.down_hist.remove(0);
                s.up_hist.remove(0);
            }
        }
        next += Duration::from_secs(1);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind (suspend, slow fork): start a fresh cadence
        }
    }
}

pub fn spawn(status: Arc<Mutex<Status>>) {
    let st = status.clone();
    std::thread::Builder::new()
        .name("screen-ticker".into())
        .spawn(move || ticker(st))
        .expect("ticker thread");
    std::thread::Builder::new()
        .name("screen-data".into())
        .spawn(move || run(status))
        .expect("data thread");
}

fn run(status: Arc<Mutex<Status>>) {
    let mut api = Api::with_timeout(Duration::from_secs(4));
    // The agent's time setting; until it answers (or on an agent without it), NITZ.
    let mut agent_clock = false;
    let mut tick: u64 = 0;
    loop {
        let started = Instant::now();

        // Everything else every 5 s (signal) or 15 s (slow-moving values); an action asks for
        // an immediate full refresh.
        if REFRESH.swap(false, Ordering::SeqCst) {
            tick = 0;
        }
        let mut update: Vec<Box<dyn FnOnce(&mut Status)>> = Vec::new();
        if tick % 5 == 0 {
            if let Some(m) = api.get("/api/call/missed") {
                let list: Vec<(String, u64)> = m["calls"].as_array().cloned().unwrap_or_default().iter()
                    .map(|c| (c["number"].as_str().unwrap_or("").to_string(), c["at"].as_u64().unwrap_or(0))).collect();
                update.push(Box::new(move |s: &mut Status| s.missed = list));
            }
        }
        // The line: ringing, dialling or talking. Polled every second, without a device id so
        // it never counts as an app being open (which would hold back the VoIP push).
        if let Some(cs) = api.get("/api/call/status") {
            // Only voice calls (mode 0): the modem also lists its data bearers (mode 1) as
            // "active", which are not calls.
            let calls: Vec<Value> = cs["calls"].as_array().cloned().unwrap_or_default()
                .into_iter().filter(|c| c["mode"].as_i64() == Some(0)).collect();
            let pick = calls
                .iter()
                .find(|c| c["stat"] == "incoming" || c["stat"] == "waiting")
                .or_else(|| calls.iter().find(|c| c["stat"] == "active"))
                .or_else(|| calls.iter().find(|c| c["stat"] == "dialing" || c["stat"] == "alerting"))
                .cloned();
            update.push(Box::new(move |s: &mut Status| {
                s.call = pick.map(|c| {
                    let state = match c["stat"].as_str().unwrap_or("") {
                        "incoming" | "waiting" => "来电",
                        "active" | "held" => "通话中",
                        _ => "呼出",
                    };
                    let number = c["number"].as_str().unwrap_or("").to_string();
                    let since = match &s.call {
                        Some(old) if old.state == state && old.number == number => old.since,
                        _ => Instant::now(),
                    };
                    crate::model::Call { number, state, since }
                });
            }));
        }
        if tick % 5 == 0 {
            if let Some(sig) = api.get("/api/network/signal") {
                if let Some(h) = num(&sig["nitz_timezone"]) {
                    if !agent_clock {
                        TZ_MIN.store((h * 60.0).round() as i32, Ordering::Relaxed);
                    }
                }
                update.push(Box::new(move |s: &mut Status| {
                    s.carrier = sig["network_provider"].as_str().unwrap_or("").to_string();
                    let nt = sig["network_type"].as_str().unwrap_or("");
                    s.net_type = match nt {
                        "LTE" | "LTE-A" | "LTE_CA" => "4G".into(),
                        "NR" | "SA" | "NSA" | "ENDC" | "NR5G" => "5G".into(),
                        "" | "NO_SERVICE" | "NoService" => "无服务".into(),
                        other if other.contains("NR") => "5G".into(),
                        other => other.to_string(),
                    };
                    let nr = num(&sig["nr5g_rsrp"]).unwrap_or(0.0);
                    if s.net_type == "5G" && nr != 0.0 {
                        s.rsrp = nr as f32;
                        s.sinr = num(&sig["nr5g_snr"]).unwrap_or(0.0) as f32;
                    } else {
                        s.rsrp = num(&sig["lte_rsrp"]).unwrap_or(0.0) as f32;
                        s.sinr = num(&sig["lte_snr"]).unwrap_or(0.0) as f32;
                    }
                    s.bars = num(&sig["signalbar"]).unwrap_or(0.0).clamp(0.0, 4.0) as u8;
                    s.roaming = sig["rmcc"].as_u64().map(|mcc| mcc != 460).unwrap_or(false);
                    // lteca: "pci,band,scell,earfcn,bw;..." — first entry is the primary cell.
                    let ca: Vec<(String, f32)> = sig["lteca"]
                        .as_str()
                        .unwrap_or("")
                        .split(';')
                        .filter_map(|c| {
                            let f: Vec<&str> = c.split(',').collect();
                            (f.len() >= 5).then(|| (format!("B{}", f[1]), f[4].parse::<f32>().unwrap_or(0.0)))
                        })
                        .collect();
                    if !ca.is_empty() {
                        s.band = ca.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join("+");
                        s.bandwidth_mhz = ca.iter().map(|c| c.1).sum();
                    }
                }));
            }
        }
        if tick % 15 == 0 {
            if let Some(b) = api.get("/api/battery") {
                update.push(Box::new(move |s: &mut Status| {
                    s.battery = num(&b["capacity"]).unwrap_or(0.0) as u8;
                    s.battery_temp_c = num(&b["temperature"]).unwrap_or(0.0) as f32 / 10.0;
                    s.battery_watts = (num(&b["current_ua"]).unwrap_or(0.0).abs() * num(&b["voltage_uv"]).unwrap_or(0.0) / 1e12) as f32;
                    s.charging = b["status"].as_str().map(|v| v == "Charging" || v == "Full").unwrap_or(false);
                    // Rough runtime from the present draw; the U60 Pro pack is about 38 Wh.
                    let ua = num(&b["current_ua"]).unwrap_or(0.0).abs();
                    let uv = num(&b["voltage_uv"]).unwrap_or(3.85e6);
                    let watts = ua * uv / 1e12;
                    s.battery_hours = (!s.charging && watts > 0.3).then(|| (38.0 * s.battery as f64 / 100.0 / watts) as f32);
                }));
            }
            if let Some(m) = api.get("/api/memory") {
                update.push(Box::new(move |s: &mut Status| s.memory_pct = num(&m["usage_pct"]).unwrap_or(0.0) as f32));
            }
            if let Some(c) = api.get("/api/cpu") {
                update.push(Box::new(move |s: &mut Status| s.cpu_pct = num(&c["overall"]).unwrap_or(0.0) as f32));
            }
            if let Some(t) = api.get("/api/device/thermal") {
                update.push(Box::new(move |s: &mut Status| s.temp_c = num(&t["cpuss_temp"]).unwrap_or(0.0) as f32));
            }
            if let Some(p) = api.get("/api/data-plan") {
                update.push(Box::new(move |s: &mut Status| {
                    s.month_bytes = num(&p["usage"]["cycle_bytes"]).unwrap_or(0.0);
                    s.today_bytes = num(&p["usage"]["today_bytes"]).unwrap_or(0.0);
                    let quota = num(&p["usage"]["quota_bytes"]).unwrap_or(0.0);
                    s.plan_bytes = (p["plan"]["enabled"].as_bool() == Some(true) && quota > 0.0).then_some(quota);
                }));
            }
            if let Some(cc) = api.get("/api/device/charge-control") {
                update.push(Box::new(move |s: &mut Status| {
                    s.charge_limit_enabled = cc["charge_limit_enabled"].as_bool().unwrap_or(false);
                    s.charge_limit = num(&cc["charge_limit"]).unwrap_or(100.0) as u32;
                }));
            }
            if let Some(ch) = api.get("/api/device/charger") {
                update.push(Box::new(move |s: &mut Status| {
                    s.charger_connected = num(&ch["charger_connect"]).unwrap_or(0.0) > 0.0;
                    s.powerbank = num(&ch["otg_powerbank_state"]).unwrap_or(0.0) > 0.0;
                }));
            }
            let last_relay = crate::actions::last_relay().map(|(ssid, _)| ssid).unwrap_or_default();
            update.push(Box::new(move |s: &mut Status| s.relay_last_ssid = last_relay));
            if let Some(w) = api.get("/api/wifi/status") {
                let clients = num(&w["clients_total"]).unwrap_or(0.0) as u32;
                let (ssid, key, enc) = (
                    w["ssid_2g"].as_str().unwrap_or("").to_string(),
                    w["key_2g"].as_str().unwrap_or("").to_string(),
                    w["encryption_2g"].as_str().unwrap_or("").to_string(),
                );
                update.push(Box::new(move |s: &mut Status| {
                    s.hotspot_ssid = ssid;
                    s.hotspot_key = key;
                    s.hotspot_enc = enc;
                }));
                let c5 = num(&w["clients_5g"]).unwrap_or(0.0) as u32;
                let c2 = num(&w["clients_2g"]).unwrap_or(0.0) as u32;
                update.push(Box::new(move |s: &mut Status| {
                    s.clients = clients;
                    s.clients_band = if c5 > 0 && c2 > 0 { "2.4G 和 5G".into() } else if c5 > 0 { "5G".into() } else { "2.4G".into() };
                }));
            }
            if let Some(w) = api.get("/api/wifi/wan/status") {
                let saved = w["mode"].as_str().unwrap_or("hotspot").to_string();
                let connected = w["connected"].as_bool() == Some(true) && saved != "hotspot";
                let hotspot_on = w["hotspot_2g"].as_str() == Some("on") || w["hotspot_5g"].as_str() == Some("on");
                // Show what the router really does: on Wi-Fi with its hotspot still up it is a
                // repeater, whatever mode was saved.
                let mode = match (saved.as_str(), connected, hotspot_on) {
                    ("client", true, true) => "repeater".to_string(),
                    ("repeater", true, false) => "client".to_string(),
                    _ => saved,
                };
                *WAN_DEV.lock().unwrap() = if connected { w["device"].as_str().unwrap_or("").to_string() } else { String::new() };
                let sig = w["station"]["signal"].as_i64().unwrap_or(0) as i32;
                let freq = w["station"]["freq"].as_u64().unwrap_or(0) as u32;
                let sta = w["station"]["sta_ssid"].as_str().unwrap_or("").to_string();
                let connecting = w["connecting"].as_bool() == Some(true);
                update.push(Box::new(move |s: &mut Status| {
                    s.uplink_mode = mode;
                    s.wifi_uplink = connected;
                    s.wifi_uplink_signal = sig;
                    s.relay_freq = freq;
                    s.relay_ssid = sta;
                    s.relay_connecting = connecting;
                }));
            }
            if let Some(a) = api.get("/api/adblock/status") {
                update.push(Box::new(move |s: &mut Status| {
                    let st = &a["stats"];
                    let hourly = st["hourly"].as_array().cloned().unwrap_or_default();
                    s.adblock = crate::model::AdBlock {
                        loaded: true,
                        enabled: a["enabled"].as_bool().unwrap_or(false),
                        active: a["active"].as_bool().unwrap_or(false),
                        paused_until: a["paused_until"].as_u64().unwrap_or(0),
                        queries: st["queries_24h"].as_u64().unwrap_or(0),
                        blocked: st["blocked_24h"].as_u64().unwrap_or(0),
                        rate: st["block_rate"].as_f64().unwrap_or(0.0) as f32,
                        devices: st["devices_now"].as_u64().unwrap_or(0).max(st["devices_24h"].as_u64().unwrap_or(0).min(1)) as u32,
                        domains: a["list"]["domains"].as_u64().unwrap_or(0),
                        updating: a["list"]["updating"].as_bool().unwrap_or(false),
                        hourly_queries: hourly.iter().map(|h| h["queries"].as_f64().unwrap_or(0.0) as f32).collect(),
                        hourly_blocked: hourly.iter().map(|h| h["blocked"].as_f64().unwrap_or(0.0) as f32).collect(),
                        top: st["top"].as_array().cloned().unwrap_or_default().iter()
                            .filter_map(|t| Some((t["domain"].as_str()?.to_string(), t["count"].as_u64().unwrap_or(0)))).collect(),
                    };
                }));
            }
            if let Some(p) = api.get("/api/push/status") {
                update.push(Box::new(move |s: &mut Status| {
                    s.push_enabled = p["enabled"].as_bool().unwrap_or(false);
                    s.push_devices = p["devices"].as_array().map(|d| d.len() as u32).unwrap_or(0);
                }));
            }
            if let Some(c) = api.get("/api/sms/capacity") {
                // Read the messages themselves twice a minute while the screen is on.
                if AWAKE.load(Ordering::Relaxed) && tick % 30 == 0 {
                    SMS_REFRESH.store(true, Ordering::Relaxed);
                }
                update.push(Box::new(move |s: &mut Status| {
                    s.unread_sms = (num(&c["sms_dev_unread_num"]).unwrap_or(0.0) + num(&c["sms_sim_unread_num"]).unwrap_or(0.0)) as u32;
                }));
            }
            if let Some(t) = api.get("/api/tunnel/status") {
                update.push(Box::new(move |s: &mut Status| {
                    s.paired_phones = t["peers"].as_array().map(|a| a.len() as u32).unwrap_or(0);
                    let conns = t["connections"].as_array().cloned().unwrap_or_default();
                    s.remote_mode = if conns.is_empty() {
                        String::new()
                    } else if conns.iter().any(|c| c["path"].as_str() == Some("direct")) {
                        "远程·直连".into()
                    } else {
                        "远程·转发".into()
                    };
                }));
            }
        }

        if SMS_REFRESH.swap(false, Ordering::Relaxed) {
            let body = serde_json::json!({"page": 0, "data_per_page": 300, "mem_store": 2, "tags": 10, "order_by": "order by id desc"});
            if let Some(v) = api.send("POST", "/api/sms/list", body) {
                let list = parse_sms(&v);
                update.push(Box::new(move |s: &mut Status| {
                    s.sms = list;
                    s.sms_loaded = true;
                }));
            }
        }

        // Time zone setting: once a minute, and right after a change from this screen.
        if tick % 60 == 0 {
            if let Some(t) = api.get("/api/system/time") {
                if let Some(off) = t["offset_minutes"].as_i64() {
                    agent_clock = true;
                    TZ_MIN.store(off as i32, Ordering::Relaxed);
                }
                update.push(Box::new(move |s: &mut Status| {
                    s.clock = crate::model::ClockInfo {
                        loaded: true,
                        offset_min: t["offset_minutes"].as_i64().unwrap_or(0) as i32,
                        source: t["source"].as_str().unwrap_or("").to_string(),
                        zone: t["zone"].as_str().unwrap_or("").to_string(),
                        manual: t["tz_mode"].as_str() == Some("manual"),
                        time_auto: t["time_auto"].as_bool().unwrap_or(true),
                        last_sync: t["last_sync"].as_u64().unwrap_or(0),
                        sync_error: t["last_sync_error"].as_str().unwrap_or("").to_string(),
                    };
                }));
            }
        }

        {
            let mut s = status.lock().unwrap();
            let any = !update.is_empty();
            for f in update {
                f(&mut s);
            }
            if any {
                s.live = true;
            }
        }
        tick += 1;
        std::thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_router_sms() {
        let v = serde_json::json!({"messages": [
            {"id": 83, "number": "10086", "content": "4F60597D002000F0009F00980080", "date": "26,11,11,18,29,00,+32", "tag": "1"},
            {"id": 90, "number": "+61412345678", "content": "00480069D83DDE00", "date": "26,11,12,09,05,00,+44", "tag": "2"},
            {"id": 91, "number": "x", "content": "", "date": "", "tag": "4"}
        ]});
        let list = parse_sms(&v);
        assert_eq!(list.len(), 2, "drafts are left out");
        assert_eq!(list[0].id, 90, "newest first");
        assert_eq!(list[0].text, "Hi😀");
        assert!(!list[0].incoming && !list[0].unread);
        assert_eq!(list[1].time, "11-11 18:29");
        assert!(list[1].incoming && list[1].unread);
        assert!(list[1].text.starts_with("你好"));
    }
}
