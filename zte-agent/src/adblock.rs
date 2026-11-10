//! Network-wide ad blocking through the router's own dnsmasq.
//!
//! A block list (anti-AD's dnsmasq list by default, refreshed daily) is sanitised and written
//! to /tmp/dnsmasq.d, so every device on the hotspot gets "no such domain" for ad and tracker
//! hosts. dnsmasq logs queries to a tmpfs file that is read and emptied every few seconds for
//! the statistics (queries, blocked, devices, top blocked domains, per hour for 24 h).
//! Pausing removes the list until a deadline; everything is restored after a reboot.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DIR: &str = "/data/openu60/adblock";
const CONF_DNSMASQ: &str = "/tmp/dnsmasq.d/openu60-adblock.conf";
const CONF_LOG: &str = "/tmp/dnsmasq.d/openu60-dnslog.conf";
const LOG_FILE: &str = "/tmp/openu60-dns.log";
const DEFAULT_SOURCES: [&str; 2] = [
    "https://anti-ad.net/anti-ad-for-dnsmasq.conf",
    "https://raw.githubusercontent.com/privacy-protection-tools/anti-AD/master/anti-ad-for-dnsmasq.conf",
];
const REFRESH_SECS: u64 = 24 * 3600;

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    paused_until: u64,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: false, paused_until: 0, allow: Vec::new(), sources: Vec::new() }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct ListMeta {
    domains: usize,
    updated_at: u64,
    source: String,
}

#[derive(Default, Clone)]
struct Hour {
    start: u64,
    queries: u64,
    blocked: u64,
    clients: HashSet<String>,
    top: HashMap<String, u64>,
}

pub struct AdBlock {
    config: Mutex<Config>,
    meta: Mutex<ListMeta>,
    hours: Mutex<Vec<Hour>>,
    last_error: Mutex<Option<String>>,
    updating: Mutex<bool>,
    /// The router's own LAN address: its lookups are not a client's.
    router_ip: String,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty() && d.len() < 254 && d.contains('.') && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

fn restart_dnsmasq() {
    let _ = std::process::Command::new("/etc/init.d/dnsmasq").arg("restart").output();
}

impl AdBlock {
    pub fn new() -> Arc<Self> {
        let config: Config = std::fs::read_to_string(format!("{DIR}/config.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let meta: ListMeta = std::fs::read_to_string(format!("{DIR}/list.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Arc::new(Self {
            config: Mutex::new(config),
            meta: Mutex::new(meta),
            hours: Mutex::new(Vec::new()),
            last_error: Mutex::new(None),
            updating: Mutex::new(false),
            router_ip: std::process::Command::new("uci")
                .args(["-q", "get", "network.lan.ipaddr"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "192.168.0.1".into()),
        })
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(DIR);
        let body = serde_json::to_string(&*self.config.lock().unwrap()).unwrap_or_default();
        let _ = std::fs::write(format!("{DIR}/config.json.tmp"), body);
        let _ = std::fs::rename(format!("{DIR}/config.json.tmp"), format!("{DIR}/config.json"));
    }

    fn active(&self) -> bool {
        let c = self.config.lock().unwrap();
        c.enabled && c.paused_until <= now()
    }

    /// Writes or removes the dnsmasq drop-ins to match the wanted state; restarts dnsmasq only
    /// when something changed.
    fn apply(&self) {
        let want = self.active() && std::path::Path::new(&format!("{DIR}/list.conf")).exists();
        let have = std::path::Path::new(CONF_DNSMASQ).exists();
        let logging = std::path::Path::new(CONF_LOG).exists();
        let enabled = self.config.lock().unwrap().enabled;
        let mut changed = false;
        if want && !have {
            let allow: HashSet<String> = self.config.lock().unwrap().allow.iter().map(|d| d.to_ascii_lowercase()).collect();
            let list = std::fs::read_to_string(format!("{DIR}/list.conf")).unwrap_or_default();
            let mut out = String::with_capacity(list.len());
            out.push_str("# OpenU60 ad blocking (generated)\n");
            for d in list.lines().filter(|d| !allow.contains(*d)) {
                out.push_str("local=/");
                out.push_str(d);
                out.push_str("/\n");
            }
            let _ = std::fs::create_dir_all("/tmp/dnsmasq.d");
            if std::fs::write(CONF_DNSMASQ, out).is_ok() {
                changed = true;
            }
        } else if !want && have {
            let _ = std::fs::remove_file(CONF_DNSMASQ);
            changed = true;
        }
        // Query logging only while the feature is on (also while paused, for the chart).
        if enabled && !logging {
            let _ = std::fs::write(LOG_FILE, "");
            let _ = std::process::Command::new("chown").args(["nobody", LOG_FILE]).output();
            let _ = std::fs::write(CONF_LOG, format!("log-queries\nlog-facility={LOG_FILE}\n"));
            changed = true;
        } else if !enabled && logging {
            let _ = std::fs::remove_file(CONF_LOG);
            let _ = std::fs::remove_file(LOG_FILE);
            changed = true;
        }
        if changed {
            eprintln!("[adblock] {}", if want { "blocking on" } else if enabled { "paused" } else { "off" });
            restart_dnsmasq();
        }
    }

    /// Downloads the block list and keeps only well-formed domain names.
    fn update_list(&self) -> Result<usize, String> {
        let sources = {
            let c = self.config.lock().unwrap();
            if c.sources.is_empty() { DEFAULT_SOURCES.iter().map(|s| s.to_string()).collect() } else { c.sources.clone() }
        };
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().into();
        let mut last = String::from("no source");
        for url in sources {
            let body = match agent.get(&url).call() {
                Ok(mut r) => {
                    let mut s = String::new();
                    if r.body_mut().as_reader().take(32 * 1024 * 1024).read_to_string(&mut s).is_err() {
                        last = format!("{url}: read failed");
                        continue;
                    }
                    s
                }
                Err(e) => {
                    last = format!("{url}: {e}");
                    continue;
                }
            };
            let mut set: Vec<String> = Vec::new();
            let mut seen = HashSet::new();
            for line in body.lines() {
                let l = line.trim();
                // dnsmasq forms: address=/d/ , address=/d/0.0.0.0 , local=/d/ , server=/d/ ; also plain domains.
                let d = if let Some(rest) = l.strip_prefix("address=/").or_else(|| l.strip_prefix("local=/")).or_else(|| l.strip_prefix("server=/")) {
                    rest.split('/').next().unwrap_or("")
                } else if !l.starts_with('#') && !l.contains(' ') && !l.contains('=') {
                    l
                } else {
                    ""
                };
                let d = d.trim_start_matches('.').to_ascii_lowercase();
                if valid_domain(&d) && seen.insert(d.clone()) {
                    set.push(d);
                }
            }
            if set.len() < 1000 {
                last = format!("{url}: only {} domains", set.len());
                continue;
            }
            let _ = std::fs::create_dir_all(DIR);
            let tmp = format!("{DIR}/list.conf.tmp");
            std::fs::write(&tmp, set.join("\n")).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, format!("{DIR}/list.conf")).map_err(|e| e.to_string())?;
            let meta = ListMeta { domains: set.len(), updated_at: now(), source: url.clone() };
            let _ = std::fs::write(format!("{DIR}/list.json"), serde_json::to_string(&meta).unwrap_or_default());
            *self.meta.lock().unwrap() = meta;
            // Force a rewrite of the dnsmasq file with the new list.
            let _ = std::fs::remove_file(CONF_DNSMASQ);
            self.apply();
            eprintln!("[adblock] list updated: {} domains from {url}", set.len());
            return Ok(set.len());
        }
        Err(last)
    }

    fn refresh_in_background(self: &Arc<Self>) {
        {
            let mut u = self.updating.lock().unwrap();
            if *u {
                return;
            }
            *u = true;
        }
        let me = Arc::clone(self);
        crate::system::spawn_named("adblock-update", move || {
            let r = me.update_list();
            *me.last_error.lock().unwrap() = r.err();
            *me.updating.lock().unwrap() = false;
        });
    }

    /// Reads and empties the dnsmasq query log, folding it into hourly buckets.
    fn ingest_log(&self) {
        let Ok(mut f) = std::fs::OpenOptions::new().read(true).write(true).open(LOG_FILE) else { return };
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() || text.is_empty() {
            return;
        }
        let _ = f.set_len(0);
        let _ = f.seek(SeekFrom::Start(0));
        let hour_start = now() / 3600 * 3600;
        let mut hours = self.hours.lock().unwrap();
        if hours.last().map(|h| h.start) != Some(hour_start) {
            hours.push(Hour { start: hour_start, ..Default::default() });
        }
        hours.retain(|h| h.start + 24 * 3600 > hour_start);
        let h = hours.last_mut().unwrap();
        for line in text.lines() {
            // "...dnsmasq[123]: query[A] example.com from 192.168.0.100"
            // "...dnsmasq[123]: config ads.example.com is NXDOMAIN"
            let Some((_, msg)) = line.split_once("]: ") else { continue };
            if let Some(rest) = msg.strip_prefix("query[") {
                // "A] example.com from 192.168.0.100"
                let rest = rest.split_once("] ").map(|(_, r)| r).unwrap_or("");
                let mut parts = rest.split_whitespace();
                let _name = parts.next();
                if let (Some("from"), Some(ip)) = (parts.next(), parts.next()) {
                    if ip != "127.0.0.1" && ip != "::1" && ip != self.router_ip.as_str() {
                        h.queries += 1;
                        if h.clients.len() < 512 {
                            h.clients.insert(ip.to_string());
                        }
                    }
                }
            } else if let Some(rest) = msg.strip_prefix("config ") {
                if let Some(name) = rest.strip_suffix(" is NXDOMAIN").or_else(|| rest.strip_suffix(" is NODATA")) {
                    h.blocked += 1;
                    if h.top.len() < 2000 || h.top.contains_key(name) {
                        *h.top.entry(name.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        crate::system::spawn_named("adblock", move || {
            std::thread::sleep(Duration::from_secs(20));
            me.apply();
            let mut last_check = 0u64;
            loop {
                // Refresh the list daily (and right away when on without one).
                let (enabled, missing, stale) = {
                    let c = me.config.lock().unwrap();
                    let m = me.meta.lock().unwrap();
                    (c.enabled, !std::path::Path::new(&format!("{DIR}/list.conf")).exists(), now().saturating_sub(m.updated_at) > REFRESH_SECS)
                };
                if enabled && (missing || stale) && now().saturating_sub(last_check) > 600 {
                    last_check = now();
                    me.refresh_in_background();
                }
                // A pause that ran out turns blocking back on; dnsmasq restarts by others
                // (firmware, DoH) lose nothing because the files live in /tmp/dnsmasq.d.
                {
                    let mut c = me.config.lock().unwrap();
                    if c.paused_until != 0 && c.paused_until <= now() {
                        c.paused_until = 0;
                        drop(c);
                        me.save();
                    }
                }
                me.apply();
                me.ingest_log();
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    }

    // ---- HTTP API ----

    pub fn status(&self) -> (u16, Value) {
        self.ingest_log();
        let c = self.config.lock().unwrap().clone();
        let m = self.meta.lock().unwrap().clone();
        let hours = self.hours.lock().unwrap().clone();
        let start = now() / 3600 * 3600;
        let mut hourly = Vec::new();
        for i in (0..24).rev() {
            let s = start - i * 3600;
            let h = hours.iter().find(|h| h.start == s);
            hourly.push(json!({"start": s, "queries": h.map(|h| h.queries).unwrap_or(0), "blocked": h.map(|h| h.blocked).unwrap_or(0)}));
        }
        let queries: u64 = hours.iter().map(|h| h.queries).sum();
        let blocked: u64 = hours.iter().map(|h| h.blocked).sum();
        let clients: HashSet<&String> = hours.iter().flat_map(|h| h.clients.iter()).collect();
        let mut top: HashMap<&String, u64> = HashMap::new();
        for h in &hours {
            for (d, n) in &h.top {
                *top.entry(d).or_insert(0) += n;
            }
        }
        let mut top: Vec<(&String, u64)> = top.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        let recent_clients = hours.last().map(|h| h.clients.len()).unwrap_or(0);
        (200, json!({"ok": true, "data": {
            "enabled": c.enabled,
            "active": self.active() && std::path::Path::new(CONF_DNSMASQ).exists(),
            "paused_until": if c.paused_until > now() { c.paused_until } else { 0 },
            "allow": c.allow,
            "list": {"domains": m.domains, "updated_at": m.updated_at, "source": m.source, "updating": *self.updating.lock().unwrap()},
            "stats": {
                "queries_24h": queries,
                "blocked_24h": blocked,
                "block_rate": if queries > 0 { blocked as f64 / queries as f64 } else { 0.0 },
                "devices_24h": clients.len(),
                "devices_now": recent_clients,
                "hourly": hourly,
                "top": top.iter().take(10).map(|(d, n)| json!({"domain": d, "count": n})).collect::<Vec<_>>(),
            },
            "last_error": *self.last_error.lock().unwrap(),
        }}))
    }

    /// PUT /api/adblock/config — {"enabled"?: bool, "allow"?: [domains], "sources"?: [urls]}
    pub fn config_set(self: &Arc<Self>, body: &[u8]) -> (u16, Value) {
        let v: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
        };
        {
            let mut c = self.config.lock().unwrap();
            if let Some(e) = v["enabled"].as_bool() {
                c.enabled = e;
                if !e {
                    c.paused_until = 0;
                }
            }
            if let Some(list) = v["allow"].as_array() {
                c.allow = list.iter().filter_map(|d| d.as_str()).map(|d| d.trim().to_ascii_lowercase()).filter(|d| valid_domain(d)).collect();
                let _ = std::fs::remove_file(CONF_DNSMASQ); // rewrite without the allowed names
            }
            if let Some(list) = v["sources"].as_array() {
                c.sources = list.iter().filter_map(|u| u.as_str()).filter(|u| u.starts_with("https://")).map(str::to_string).collect();
            }
        }
        self.save();
        if self.config.lock().unwrap().enabled && !std::path::Path::new(&format!("{DIR}/list.conf")).exists() {
            self.refresh_in_background();
        }
        self.apply();
        self.status()
    }

    /// POST /api/adblock/pause — {"minutes": n} or {"until": unix seconds}
    pub fn pause(&self, body: &[u8]) -> (u16, Value) {
        let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let until = v["until"].as_u64().or_else(|| v["minutes"].as_u64().map(|m| now() + m * 60)).unwrap_or(0);
        if until <= now() || until > now() + 7 * 86_400 {
            return (400, json!({"ok": false, "error": "pause must end within 7 days"}));
        }
        self.config.lock().unwrap().paused_until = until;
        self.save();
        self.apply();
        self.status()
    }

    pub fn resume(&self) -> (u16, Value) {
        self.config.lock().unwrap().paused_until = 0;
        self.save();
        self.apply();
        self.status()
    }

    pub fn update(self: &Arc<Self>) -> (u16, Value) {
        self.refresh_in_background();
        (200, json!({"ok": true, "data": {"updating": true}}))
    }
}
