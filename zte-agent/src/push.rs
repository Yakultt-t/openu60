//! APNs pushes straight from the router: an alert for every new SMS and a VoIP push for
//! every incoming call, so the phone app does not have to be open or polling.
//!
//! Credentials come from the same JSON file as the Live Activity provider
//! (`ZTE_AGENT_LIVE_ACTIVITY_CONFIG`, or `ZTE_AGENT_APNS_CONFIG` to use a separate one):
//! `team_id`, `key_id`, `bundle_id`, `key_file` (.p8, mode 0600) and `environment`
//! (`sandbox` for development builds, `production` for TestFlight/App Store builds).
//! Device tokens are registered by the app (PUT /api/push/register) and persisted in
//! /data/zte-agent-push.json so they survive an agent restart.
//!
//! Without a local key (every router but a developer's) pushes go through the OpenU60 push
//! gateway (`ZTE_AGENT_PUSH_GATEWAY`, or the `OPENU60_PUSH_GATEWAY` set at build time; none by default): each
//! request is signed with the router's tunnel identity and the gateway, which holds the APNs
//! key, forwards it to Apple. `ZTE_AGENT_PUSH_VIA=gateway` forces the gateway even with a key.
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::at_cmd::{self, AtPort};
use crate::handlers::AppState;
use crate::sms_forward;
use crate::telephony::parse_clcc;

const TOKENS_FILE: &str = "/data/zte-agent-push.json";
/// How often the modem is asked about calls while a VoIP token is registered.
const CALL_POLL: Duration = Duration::from_millis(1000);
/// The app polls /api/call/status itself while open; a push would only duplicate what it
/// already shows, so skip it when a poll arrived this recently.
const APP_POLL_FRESH: u64 = 4;
/// Provider tokens are valid for an hour; Apple asks not to refresh more often than 20 min.
const JWT_LIFETIME: u64 = 50 * 60;
/// Push gateway baked in at build time (`OPENU60_PUSH_GATEWAY`); none in public builds.
const DEFAULT_GATEWAY: Option<&str> = option_env!("OPENU60_PUSH_GATEWAY");

pub(crate) fn gateway_url() -> Option<String> {
    match std::env::var("ZTE_AGENT_PUSH_GATEWAY") {
        Ok(v) if v == "off" || v.is_empty() => None,
        Ok(v) => Some(v),
        Err(_) => DEFAULT_GATEWAY.filter(|s| !s.is_empty()).map(String::from),
    }
}

#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    team_id: String,
    key_id: String,
    bundle_id: String,
    key_file: String,
    environment: Environment,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Environment {
    Sandbox,
    Production,
}

impl Environment {
    fn host(self) -> &'static str {
        match self {
            Self::Sandbox => "https://api.sandbox.push.apple.com",
            Self::Production => "https://api.push.apple.com",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }
}

struct Provider {
    config: Config,
    key: SigningKey,
}

fn private_file(path: &Path) -> Result<String, &'static str> {
    let meta = fs::symlink_metadata(path).map_err(|_| "private file unavailable")?;
    if !meta.is_file()
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 16384
    {
        return Err("private file must be owned by the agent user, regular, <=16 KiB, mode 0600");
    }
    fs::read_to_string(path).map_err(|_| "cannot read private file")
}

fn load_provider() -> Result<Option<Provider>, &'static str> {
    let path = std::env::var("ZTE_AGENT_APNS_CONFIG")
        .or_else(|_| std::env::var("ZTE_AGENT_LIVE_ACTIVITY_CONFIG"));
    let Ok(path) = path else { return Ok(None) };
    let config: Config = serde_json::from_str(&private_file(Path::new(&path))?)
        .map_err(|_| "invalid APNs config JSON")?;
    if !config.enabled {
        return Ok(None);
    }
    let identifier = |s: &str| s.len() == 10 && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if !identifier(&config.team_id) || !identifier(&config.key_id) {
        return Err("team_id and key_id must be 10 uppercase letters/digits");
    }
    if !config.bundle_id.contains('.') || !Path::new(&config.key_file).is_absolute() {
        return Err("invalid bundle_id or key_file");
    }
    let pem = private_file(Path::new(&config.key_file))?;
    let key = SigningKey::from_pkcs8_pem(&pem).map_err(|_| "key must be an Apple ES256 .p8 key")?;
    Ok(Some(Provider { config, key }))
}

/// Push tokens of one phone/tablet. Every bound device keeps its own, so an incoming call rings
/// all of them (whichever answers carries the audio).
/// What a device wants to be told about. Everything is on until the app says otherwise
/// (except "charged", which most people do not want).
#[derive(Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct NotifyPrefs {
    #[serde(default = "default_true")]
    pub sms: bool,
    #[serde(default = "default_true")]
    pub mms: bool,
    /// Incoming calls through VoIP push / CallKit.
    #[serde(default = "default_true")]
    pub calls: bool,
    #[serde(default = "default_true")]
    pub data: bool,
    /// Only messages from numbers in the phone's contacts (`known`).
    #[serde(default)]
    pub sms_known_only: bool,
    /// ...but verification codes always get through.
    #[serde(default = "default_true")]
    pub codes_always: bool,
    /// Hashed contact numbers (see `number_hash`), sent by the app when `sms_known_only` is on.
    #[serde(default)]
    pub known: Vec<String>,
    /// A plain notification for missed calls (useful with `calls` off).
    #[serde(default = "default_true")]
    pub missed: bool,
    #[serde(default = "default_true")]
    pub offline: bool,
    #[serde(default = "default_true")]
    pub battery: bool,
    #[serde(default = "default_true")]
    pub service: bool,
    #[serde(default = "default_true")]
    pub volte: bool,
    #[serde(default = "default_true")]
    pub uplink: bool,
    #[serde(default = "default_true")]
    pub temperature: bool,
    #[serde(default)]
    pub charged: bool,
    /// UI language of the device ("zh" or "en") for router-written alerts.
    #[serde(default)]
    pub lang: String,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        Self {
            sms: true, mms: true, calls: true, data: true, sms_known_only: false, codes_always: true,
            known: Vec::new(), missed: true, offline: true, battery: true, service: true, volte: true,
            uplink: true, temperature: true, charged: false, lang: String::new(),
        }
    }
}

/// Device conditions the router reports on its own (device_alerts.rs).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DeviceAlert {
    Battery,
    Service,
    Volte,
    Uplink,
    Temperature,
    Charged,
}

/// The key the app hashes a contact number into (and the router a sender): digits only,
/// the last 8 of them, so "+86 138 0013 8000" and "13800138000" match; senders with letters
/// keep their text, upper-cased. First 16 hex chars of SHA-256.
pub fn number_hash(number: &str) -> String {
    let digits: String = number.chars().filter(|c| c.is_ascii_digit()).collect();
    let key = if number.chars().any(|c| c.is_alphabetic()) {
        number.trim().to_uppercase()
    } else {
        digits[digits.len().saturating_sub(8)..].to_string()
    };
    hex::encode(&ring::digest::digest(&ring::digest::SHA256, key.as_bytes()).as_ref()[..8])
}

/// A message that carries a one-time code ("验证码 123456", "Your code is 4821").
pub fn looks_like_code(text: &str) -> bool {
    let lower = text.to_lowercase();
    let keyword = ["验证码", "校验码", "动态码", "动态密码", "确认码", "verification", "code", "otp", "passcode", "pin"]
        .iter()
        .any(|k| lower.contains(k));
    let mut run = 0;
    let mut has_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            run += 1;
        } else {
            if (4..=8).contains(&run) {
                has_digits = true;
            }
            run = 0;
        }
    }
    keyword && (has_digits || (4..=8).contains(&run))
}

/// Which preference a push belongs to.
#[derive(Clone, Copy)]
pub enum Kind {
    Sms,
    Mms,
    Calls,
    Data,
    Missed,
    Device(DeviceAlert),
    /// The test button: every device with an alert token.
    Any,
}

impl NotifyPrefs {
    fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::Sms => self.sms,
            Kind::Mms => self.mms,
            Kind::Calls => self.calls,
            Kind::Data => self.data,
            Kind::Missed => self.missed,
            Kind::Device(a) => match a {
                DeviceAlert::Battery => self.battery,
                DeviceAlert::Service => self.service,
                DeviceAlert::Volte => self.volte,
                DeviceAlert::Uplink => self.uplink,
                DeviceAlert::Temperature => self.temperature,
                DeviceAlert::Charged => self.charged,
            },
            Kind::Any => true,
        }
    }
}

#[derive(Default, Clone, serde::Serialize, Deserialize)]
struct DeviceTokens {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    alert_token: Option<String>,
    #[serde(default)]
    voip_token: Option<String>,
    #[serde(default)]
    updated_at: u64,
    /// APNs environment this device's tokens belong to, learned from the first accepted push:
    /// Xcode builds use the sandbox, TestFlight/App Store builds production.
    #[serde(default)]
    sandbox: Option<bool>,
    #[serde(default)]
    prefs: NotifyPrefs,
}

#[derive(Default, Clone, serde::Serialize, Deserialize)]
struct Tokens {
    #[serde(default)]
    devices: Vec<DeviceTokens>,
    // Pre-multi-device format, migrated on load.
    #[serde(default, skip_serializing)]
    alert_token: Option<String>,
    #[serde(default, skip_serializing)]
    voip_token: Option<String>,
    #[serde(default)]
    updated_at: u64,
}

impl Tokens {
    fn migrate(mut self) -> Self {
        if self.devices.is_empty() && (self.alert_token.is_some() || self.voip_token.is_some()) {
            self.devices.push(DeviceTokens {
                id: "legacy".into(),
                name: "legacy".into(),
                alert_token: self.alert_token.take(),
                voip_token: self.voip_token.take(),
                updated_at: self.updated_at,
                sandbox: None,
                prefs: NotifyPrefs::default(),
            });
        }
        self
    }

    fn any_alert(&self) -> bool {
        self.devices.iter().any(|d| d.alert_token.is_some())
    }

    fn any_voip(&self) -> bool {
        self.devices.iter().any(|d| d.voip_token.is_some())
    }
}

#[derive(Default)]
struct Stats {
    last_sms_push_at: Option<u64>,
    last_call_push_at: Option<u64>,
    last_error: Option<String>,
    sms_pushed: u64,
    calls_pushed: u64,
}

pub struct PushService {
    provider: Option<Provider>,
    config_error: Option<&'static str>,
    tokens: Mutex<Tokens>,
    stats: Mutex<Stats>,
    jwt: Mutex<Option<(String, u64)>>,
    client: Mutex<Option<reqwest::blocking::Client>>,
    /// Router identity used to sign gateway requests; set in `start` from the tunnel service.
    signer: Mutex<Option<Arc<openu60_tunnel::Identity>>>,
    gateway: Option<String>,
    gw_client: Mutex<Option<reqwest::blocking::Client>>,
    at_port: AtPort,
    /// Unix seconds of each device's last call-status poll: a device that is open and polling
    /// sees the call itself and gets no VoIP push.
    polls: Mutex<std::collections::HashMap<String, u64>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn valid_token(t: &str) -> bool {
    (32..=512).contains(&t.len()) && t.len() % 2 == 0 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

enum Outcome {
    Accepted,
    Retry(String),
    Rejected(String),
}

impl PushService {
    pub fn new() -> Self {
        let (provider, config_error) = match load_provider() {
            Ok(p) => (p, None),
            Err(e) => (None, Some(e)),
        };
        let tokens: Tokens = fs::read_to_string(TOKENS_FILE)
            .ok()
            .and_then(|s| serde_json::from_str::<Tokens>(&s).ok())
            .unwrap_or_default()
            .migrate();
        Self {
            provider,
            config_error,
            tokens: Mutex::new(tokens),
            stats: Mutex::new(Stats::default()),
            jwt: Mutex::new(None),
            client: Mutex::new(None),
            signer: Mutex::new(None),
            gateway: gateway_url(),
            gw_client: Mutex::new(None),
            at_port: AtPort::new(),
            polls: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Called by GET /api/call/status with the polling device's id.
    pub fn note_poll(&self, device: &str) {
        if !device.is_empty() {
            self.polls.lock().unwrap().insert(device.to_string(), now());
        }
    }

    fn polling_recently(&self, device: &str) -> bool {
        self.polls
            .lock()
            .unwrap()
            .get(device)
            .map(|t| now().saturating_sub(*t) < APP_POLL_FRESH)
            .unwrap_or(false)
    }

    /// Pushes go to Apple directly with the local key unless told to use the gateway.
    fn direct(&self) -> Option<&Provider> {
        let forced = std::env::var("ZTE_AGENT_PUSH_VIA").map(|v| v == "gateway").unwrap_or(false);
        if forced && self.gateway.is_some() { None } else { self.provider.as_ref() }
    }

    fn via_gateway(&self) -> bool {
        self.direct().is_none() && self.gateway.is_some() && self.signer.lock().unwrap().is_some()
    }

    fn can_push(&self) -> bool {
        self.direct().is_some() || self.via_gateway()
    }

    fn transport_label(&self) -> &'static str {
        if self.direct().is_some() { "direct" } else if self.via_gateway() { "gateway" } else { "none" }
    }

    fn alert_targets(&self, kind: Kind) -> Vec<(String, String)> {
        let t = self.tokens.lock().unwrap();
        t.devices
            .iter()
            .filter(|d| d.prefs.allows(kind))
            .filter_map(|d| d.alert_token.clone().map(|tok| (d.id.clone(), tok)))
            .collect()
    }

    /// Devices that want this SMS: SMS on, and (when they asked for contacts only) a known
    /// sender or, if they allow it, a verification code.
    fn sms_targets(&self, sender: &str, text: &str) -> Vec<(String, String)> {
        let hash = number_hash(sender);
        let code = looks_like_code(text);
        let t = self.tokens.lock().unwrap();
        t.devices
            .iter()
            .filter(|d| d.prefs.sms)
            .filter(|d| !d.prefs.sms_known_only || d.prefs.known.contains(&hash) || (d.prefs.codes_always && code))
            .filter_map(|d| d.alert_token.clone().map(|tok| (d.id.clone(), tok)))
            .collect()
    }

    /// Language and token of every device that wants `kind`, for alerts the router writes.
    fn alert_targets_lang(&self, kind: Kind) -> Vec<(String, String, bool)> {
        let t = self.tokens.lock().unwrap();
        t.devices
            .iter()
            .filter(|d| d.prefs.allows(kind))
            .filter_map(|d| d.alert_token.clone().map(|tok| (d.id.clone(), tok, d.prefs.lang.starts_with("en"))))
            .collect()
    }

    /// A router-written notification (device status, missed call), in each device's language.
    pub fn notify_device(&self, kind: Kind, zh: (&str, &str), en: (&str, &str), collapse: &str) {
        for (device, token, english) in self.alert_targets_lang(kind) {
            let (title, body) = if english { en } else { zh };
            let payload = json!({
                "aps": {"alert": {"title": title, "body": body}, "sound": "default", "thread-id": "openu60-device"},
                "kind": "device",
            });
            if let Outcome::Rejected(e) | Outcome::Retry(e) = self.deliver(&device, &token, "alert", None, 10, &payload, Some(collapse)) {
                eprintln!("[push] {collapse} to {device}: {e}");
            }
        }
    }

    /// Alert tokens (with their APNs environment) of devices that want to hear when the router
    /// goes offline: the gateway sends that one, since an offline router cannot.
    pub fn offline_watchers(&self) -> Vec<Value> {
        let t = self.tokens.lock().unwrap();
        t.devices
            .iter()
            .filter(|d| d.prefs.offline)
            .filter_map(|d| {
                d.alert_token.as_ref().map(|tok| json!({
                    "token": tok,
                    "env": if d.sandbox == Some(true) { "sandbox" } else { "production" },
                    "lang": if d.prefs.lang.starts_with("en") { "en" } else { "zh" },
                }))
            })
            .collect()
    }

    fn voip_targets(&self) -> Vec<(String, String)> {
        let t = self.tokens.lock().unwrap();
        t.devices
            .iter()
            .filter(|d| d.prefs.calls)
            .filter_map(|d| d.voip_token.clone().map(|tok| (d.id.clone(), tok)))
            .collect()
    }

    /// Sends one alert push to every device that registered an alert token and wants `kind`.
    pub fn alert_all(&self, kind: Kind, payload: &Value, collapse: Option<&str>) -> Option<String> {
        self.alert_some(self.alert_targets(kind), payload, collapse)
    }

    fn alert_some(&self, targets: Vec<(String, String)>, payload: &Value, collapse: Option<&str>) -> Option<String> {
        let mut error = None;
        for (device, token) in targets {
            match self.deliver(&device, &token, "alert", None, 10, payload, collapse) {
                Outcome::Accepted => {}
                Outcome::Retry(e) | Outcome::Rejected(e) => error = Some(e),
            }
        }
        error
    }

    // ---- HTTP API ----

    pub fn status(&self) -> (u16, Value) {
        let t = self.tokens.lock().unwrap().clone();
        let s = self.stats.lock().unwrap();
        (200, json!({"ok": true, "data": {
            "enabled": self.can_push(),
            "transport": self.transport_label(),
            "config_error": if self.via_gateway() { None } else { self.config_error },
            "environment": self.provider.as_ref().map(|p| p.config.environment.label()),
            "bundle_id": self.provider.as_ref().map(|p| p.config.bundle_id.clone()),
            "alert_registered": t.any_alert(),
            "voip_registered": t.any_voip(),
            "devices": t.devices.iter().map(|d| json!({
                "id": d.id, "name": d.name,
                "alert": d.alert_token.is_some(), "voip": d.voip_token.is_some(),
                "environment": d.sandbox.map(|s| if s { "sandbox" } else { "production" }),
                "prefs": d.prefs,
                "updated_at": d.updated_at,
            })).collect::<Vec<_>>(),
            "tokens_updated_at": t.updated_at,
            "sms_pushed": s.sms_pushed,
            "calls_pushed": s.calls_pushed,
            "last_sms_push_at": s.last_sms_push_at,
            "last_call_push_at": s.last_call_push_at,
            "last_error": s.last_error,
        }}))
    }

    /// PUT /api/push/register — body: {"alert_token": hex?, "voip_token": hex?}
    pub fn register(&self, body: &[u8]) -> (u16, Value) {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
        };
        // Validate first so a bad body changes nothing.
        let mut updates: Vec<(&str, String)> = Vec::new();
        for field in ["alert_token", "voip_token"] {
            match &parsed[field] {
                Value::String(t) => {
                    if !valid_token(t) {
                        return (400, json!({"ok": false, "error": format!("{field} must be a hex device token")}));
                    }
                    updates.push((field, t.to_lowercase()));
                }
                Value::Null => {}
                _ => return (400, json!({"ok": false, "error": format!("{field} must be a string")})),
            }
        }
        let device_id = parsed["device_id"].as_str().unwrap_or("legacy").chars().take(64).collect::<String>();
        let name = parsed["name"].as_str().unwrap_or("").chars().take(64).collect::<String>();
        let prefs: Option<NotifyPrefs> = parsed.get("prefs").and_then(|p| serde_json::from_value(p.clone()).ok());
        let mut tokens = self.tokens.lock().unwrap();
        // A token moved to a new device id (reinstall): it must not stay on the old entry.
        for (_, value) in &updates {
            for d in tokens.devices.iter_mut().filter(|d| d.id != device_id) {
                if d.alert_token.as_deref() == Some(value.as_str()) { d.alert_token = None; }
                if d.voip_token.as_deref() == Some(value.as_str()) { d.voip_token = None; }
            }
        }
        tokens.devices.retain(|d| d.alert_token.is_some() || d.voip_token.is_some() || d.id == device_id);
        if !tokens.devices.iter().any(|d| d.id == device_id) {
            tokens.devices.push(DeviceTokens { id: device_id.clone(), ..Default::default() });
        }
        let entry = tokens.devices.iter_mut().find(|d| d.id == device_id).unwrap();
        let mut changed = false;
        if let Some(mut p) = prefs {
            p.known.truncate(5000);
            p.known.retain(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()));
            if entry.prefs != p {
                entry.prefs = p;
                changed = true;
            }
        }
        if !name.is_empty() && entry.name != name {
            entry.name = name;
            changed = true;
        }
        for (field, value) in updates {
            let slot = if field == "alert_token" { &mut entry.alert_token } else { &mut entry.voip_token };
            if slot.as_deref() != Some(value.as_str()) {
                *slot = Some(value);
                entry.sandbox = None;
                changed = true;
            }
        }
        let (alert, voip) = (entry.alert_token.is_some(), entry.voip_token.is_some());
        if changed {
            entry.updated_at = now();
            tokens.updated_at = now();
            Self::persist(&tokens);
            eprintln!("[push] tokens updated for {device_id} (alert={alert}, voip={voip}, {} device(s))", tokens.devices.len());
        }
        (200, json!({"ok": true, "data": {"alert_registered": alert, "voip_registered": voip, "enabled": self.can_push()}}))
    }

    /// DELETE /api/push/register[?device=<id>] — forgets one device, or every device.
    pub fn unregister(&self, device: Option<&str>) -> (u16, Value) {
        let mut tokens = self.tokens.lock().unwrap();
        match device {
            Some(id) => {
                tokens.devices.retain(|d| d.id != id);
                tokens.updated_at = now();
                eprintln!("[push] forgot device {id}");
            }
            None => *tokens = Tokens { updated_at: now(), ..Tokens::default() },
        }
        Self::persist(&tokens);
        (200, json!({"ok": true}))
    }

    /// POST /api/push/test — sends a visible alert to the registered phone.
    pub fn test(&self) -> (u16, Value) {
        if self.alert_targets(Kind::Any).is_empty() {
            return (409, json!({"ok": false, "error": "no alert token registered"}));
        }
        let payload = json!({"aps": {"alert": {"title": "OpenU60", "body": "Push notifications are working."}, "sound": "default"}, "kind": "test"});
        match self.alert_all(Kind::Any, &payload, Some("openu60-test")) {
            None => (200, json!({"ok": true, "data": {"status": "sent"}})),
            Some(e) => (502, json!({"ok": false, "error": e})),
        }
    }

    fn persist(tokens: &Tokens) {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(TOKENS_FILE) {
            use std::io::Write;
            let _ = f.write_all(serde_json::to_string(tokens).unwrap_or_default().as_bytes());
        }
    }

    // ---- background work ----

    pub fn start(self: &Arc<Self>, sms_events: mpsc::Receiver<Value>, state: Arc<AppState>) {
        if let Some(tunnel) = state.tunnel.as_ref() {
            *self.signer.lock().unwrap() = Some(Arc::clone(tunnel.identity()));
        }
        if !self.can_push() {
            if let Some(e) = self.config_error {
                eprintln!("[push] disabled: {e}");
            } else {
                eprintln!("[push] disabled: no APNs key and no gateway identity");
            }
            return;
        }
        eprintln!("[push] sending {}", if self.via_gateway() { "through the OpenU60 push gateway" } else { "directly to APNs" });
        let me = Arc::clone(self);
        crate::system::spawn_named("push-sms", move || me.sms_loop(sms_events));
        let me = Arc::clone(self);
        crate::system::spawn_named("push-calls", move || me.call_loop(state));
    }

    fn sms_loop(&self, rx: mpsc::Receiver<Value>) {
        let mut watermark = sms_forward::fetch_max_sms_id().unwrap_or(0);
        eprintln!("[push] sms watermark {watermark}");
        loop {
            // Normally woken by the SMS daemon's event; every 10 s it also checks the store
            // itself, so a missed event (SMS over IMS, a restarted ubus listener) never means
            // a missed notification.
            let from_event = match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(event) => {
                    if event["wms_status"].as_str() != Some("new_sms_received") {
                        continue;
                    }
                    true
                }
                Err(mpsc::RecvTimeoutError::Timeout) => false,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_secs(10));
                    false
                }
            };
            if self.alert_targets(Kind::Sms).is_empty() {
                continue;
            }
            let mut polled_max = None;
            if from_event {
                // Let the modem store the message before reading the list.
                std::thread::sleep(Duration::from_millis(700));
            } else {
                match sms_forward::fetch_max_sms_id() {
                    Ok(max) if max > watermark => polled_max = Some(max),
                    _ => continue,
                }
            }
            let messages = match sms_forward::fetch_new_messages(watermark) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[push] sms fetch failed: {e}");
                    continue;
                }
            };
            for m in messages {
                watermark = watermark.max(m.id);
                let body: String = m.content.chars().take(500).collect();
                let payload = json!({
                    "aps": {
                        "alert": {"title": m.sender, "body": body},
                        "sound": "default",
                        "thread-id": m.sender,
                        "category": "OPENU60_SMS",
                        "mutable-content": 1,
                    },
                    "kind": "sms",
                    "number": m.sender,
                    "sms_id": m.id,
                    "date": m.date,
                });
                let collapse = format!("sms-{}", m.id);
                match self.alert_some(self.sms_targets(&m.sender, &m.content), &payload, Some(&collapse)) {
                    None => {
                        let mut s = self.stats.lock().unwrap();
                        s.sms_pushed += 1;
                        s.last_sms_push_at = Some(now());
                        s.last_error = None;
                    }
                    Some(e) => {
                        eprintln!("[push] sms push failed: {e}");
                        self.stats.lock().unwrap().last_error = Some(e);
                    }
                }
            }
            // Sent messages also raise the id; skip past them so the poll stays quiet.
            if let Some(max) = polled_max {
                watermark = watermark.max(max);
            }
        }
    }

    /// Alert push for a received MMS, mirroring the SMS push payload.
    pub fn notify_mms(&self, number: &str, mms_id: u64, text: &str, images: usize) {
        if self.alert_targets(Kind::Mms).is_empty() {
            return;
        }
        let body: String = if text.is_empty() {
            if images > 0 { format!("[{} image{}]", images, if images == 1 { "" } else { "s" }) } else { "[MMS]".into() }
        } else {
            text.chars().take(500).collect()
        };
        let payload = json!({
            "aps": {
                "alert": {"title": number, "body": body},
                "sound": "default",
                "thread-id": number,
                "category": "OPENU60_SMS",
                "mutable-content": 1,
            },
            "kind": "mms",
            "number": number,
            "mms_id": mms_id,
            "images": images,
        });
        let collapse = format!("mms-{mms_id}");
        match self.alert_all(Kind::Mms, &payload, Some(&collapse)).map_or(Outcome::Accepted, Outcome::Rejected) {
            Outcome::Accepted => {
                let mut s = self.stats.lock().unwrap();
                s.sms_pushed += 1;
                s.last_sms_push_at = Some(now());
                s.last_error = None;
            }
            Outcome::Retry(e) | Outcome::Rejected(e) => {
                eprintln!("[push] mms push failed: {e}");
                self.stats.lock().unwrap().last_error = Some(e);
            }
        }
    }

    /// Polls the modem for ringing calls while a VoIP token is registered and the app is
    /// not already watching, and pushes once per call.
    fn call_loop(&self, state: Arc<AppState>) {
        let mut pushed: Option<(i64, String)> = None;
        loop {
            std::thread::sleep(CALL_POLL);
            if self.voip_targets().is_empty() {
                std::thread::sleep(Duration::from_secs(4));
                continue;
            }
            // When some app is polling, the call monitor already asks the modem every 250 ms:
            // read its answer instead of competing for the AT port.
            let calls: Vec<Value> = match state.call_monitor.recent_calls() {
                Some(c) => c,
                None => {
                    let resp = match at_cmd::send(&self.at_port, "AT+CLCC", 2) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if parse_clcc(&resp).is_empty() && !resp.contains("OK") {
                        continue;
                    }
                    let mut calls = parse_clcc(&resp);
                    crate::ims_guard::fill_missing_numbers(&mut calls);
                    calls
                }
            };
            crate::missed::observe(&calls);
            let calls: Vec<Value> = calls.into_iter().filter(|c| c["mode"].as_i64() == Some(0)).collect();
            if calls.is_empty() {
                pushed = None;
                continue;
            }
            let Some(ringing) = calls.iter().find(|c| matches!(c["stat"].as_str(), Some("incoming") | Some("waiting"))) else {
                continue;
            };
            let id = ringing["id"].as_i64().unwrap_or(-1);
            let number = ringing["number"].as_str().unwrap_or("").to_string();
            if pushed.as_ref() == Some(&(id, number.clone())) {
                continue;
            }
            pushed = Some((id, number.clone()));
            let payload = json!({"kind": "incoming_call", "number": number, "call_id": id, "at": now()});
            // Every device that is not open right now gets the VoIP push; an open one sees the
            // call in its own poll (pushing it too would make it report the call twice).
            for (device, token) in self.voip_targets() {
                if self.polling_recently(&device) {
                    eprintln!("[push] incoming call: {device} is polling, no push");
                    continue;
                }
                let started = Instant::now();
                match self.deliver(&device, &token, "voip", Some(now() + 25), 10, &payload, None) {
                    Outcome::Accepted => {
                        eprintln!("[push] voip push to {device} sent in {} ms", started.elapsed().as_millis());
                        let mut s = self.stats.lock().unwrap();
                        s.calls_pushed += 1;
                        s.last_call_push_at = Some(now());
                        s.last_error = None;
                    }
                    Outcome::Retry(e) | Outcome::Rejected(e) => {
                        eprintln!("[push] voip push to {device} failed: {e}");
                        self.stats.lock().unwrap().last_error = Some(e);
                    }
                }
            }
        }
    }

    /// Sends to one device, trying its learned APNs environment first and the other one on a
    /// token/environment mismatch; a token neither accepts is dropped.
    #[allow(clippy::too_many_arguments)]
    fn deliver(&self, device: &str, token: &str, push_type: &str, expiration: Option<u64>, priority: u8, payload: &Value, collapse_id: Option<&str>) -> Outcome {
        if !self.can_push() {
            return Outcome::Rejected("push provider disabled".into());
        }
        let configured_sandbox = self.direct().map(|p| matches!(p.config.environment, Environment::Sandbox)).unwrap_or(false);
        let learned = {
            let t = self.tokens.lock().unwrap();
            t.devices.iter().find(|d| d.id == device).and_then(|d| d.sandbox)
        };
        let first = learned.unwrap_or(configured_sandbox);
        let order = if learned.is_some() { vec![first] } else { vec![first, !first] };
        let mut last = Outcome::Rejected("not sent".into());
        for sandbox in order {
            let env = if sandbox { Environment::Sandbox } else { Environment::Production };
            match self.send(env, token, push_type, expiration, priority, payload, collapse_id) {
                Outcome::Accepted => {
                    if learned != Some(sandbox) {
                        let mut t = self.tokens.lock().unwrap();
                        if let Some(d) = t.devices.iter_mut().find(|d| d.id == device) {
                            d.sandbox = Some(sandbox);
                        }
                        Self::persist(&t);
                    }
                    return Outcome::Accepted;
                }
                Outcome::Rejected(e) if e.starts_with("TOKEN:") => {
                    last = Outcome::Rejected(e);
                    continue;
                }
                other => return other,
            }
        }
        // Neither environment knows this token: drop it (stale build or revoked).
        let mut t = self.tokens.lock().unwrap();
        if let Some(d) = t.devices.iter_mut().find(|d| d.id == device) {
            if d.alert_token.as_deref() == Some(token) { d.alert_token = None; }
            if d.voip_token.as_deref() == Some(token) { d.voip_token = None; }
            d.sandbox = None;
        }
        t.devices.retain(|d| d.alert_token.is_some() || d.voip_token.is_some());
        Self::persist(&t);
        match last {
            Outcome::Rejected(e) => Outcome::Rejected(format!("{} (token dropped)", e.trim_start_matches("TOKEN:"))),
            o => o,
        }
    }

    // ---- APNs transport ----

    fn jwt(&self, provider: &Provider) -> String {
        let mut slot = self.jwt.lock().unwrap();
        let t = now();
        if let Some((jwt, issued)) = slot.as_ref() {
            if t.saturating_sub(*issued) < JWT_LIFETIME {
                return jwt.clone();
            }
        }
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "ES256", "kid": provider.config.key_id}).to_string());
        let claims = URL_SAFE_NO_PAD.encode(json!({"iss": provider.config.team_id, "iat": t}).to_string());
        let input = format!("{header}.{claims}");
        let signature: Signature = provider.key.sign(input.as_bytes());
        let jwt = format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
        *slot = Some((jwt.clone(), t));
        jwt
    }

    fn client(&self) -> Result<reqwest::blocking::Client, String> {
        let mut slot = self.client.lock().unwrap();
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let c = reqwest::blocking::Client::builder()
            .https_only(true)
            .http2_prior_knowledge()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        *slot = Some(c.clone());
        Ok(c)
    }

    #[allow(clippy::too_many_arguments)]
    fn send(&self, env: Environment, token: &str, push_type: &str, expiration: Option<u64>, priority: u8, payload: &Value, collapse_id: Option<&str>) -> Outcome {
        let Some(provider) = self.direct() else {
            return self.send_gateway(env, token, push_type, expiration, priority, payload, collapse_id);
        };
        let client = match self.client() {
            Ok(c) => c,
            Err(e) => return Outcome::Retry(e),
        };
        let topic = match push_type {
            "voip" => format!("{}.voip", provider.config.bundle_id),
            _ => provider.config.bundle_id.clone(),
        };
        let mut req = client
            .post(format!("{}/3/device/{}", env.host(), token))
            .bearer_auth(self.jwt(provider))
            .header("apns-topic", topic)
            .header("apns-push-type", push_type)
            .header("apns-priority", priority.to_string())
            .header("apns-expiration", expiration.unwrap_or(0).to_string());
        if let Some(c) = collapse_id {
            req = req.header("apns-collapse-id", c);
        }
        let response = match req.json(payload).send() {
            Ok(r) => r,
            Err(e) => return Outcome::Retry(format!("APNs transport: {e}")),
        };
        let status = response.status().as_u16();
        if status == 200 {
            return Outcome::Accepted;
        }
        let mut body = Vec::new();
        let _ = response.take(4096).read_to_end(&mut body);
        let reason = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
        classify(status, &reason)
    }

    /// Signs the request with the router's identity and hands it to the push gateway, which
    /// answers with Apple's status and reason.
    #[allow(clippy::too_many_arguments)]
    fn send_gateway(&self, env: Environment, token: &str, push_type: &str, expiration: Option<u64>, priority: u8, payload: &Value, collapse_id: Option<&str>) -> Outcome {
        let (Some(url), Some(identity)) = (self.gateway.as_ref(), self.signer.lock().unwrap().clone()) else {
            return Outcome::Rejected("push provider disabled".into());
        };
        let client = {
            let mut slot = self.gw_client.lock().unwrap();
            match slot.as_ref() {
                Some(c) => c.clone(),
                None => match reqwest::blocking::Client::builder()
                    .https_only(true)
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(15))
                    .build()
                {
                    Ok(c) => {
                        *slot = Some(c.clone());
                        c
                    }
                    Err(e) => return Outcome::Retry(format!("http client: {e}")),
                },
            }
        };
        let body = json!({
            "env": env.label(), "token": token, "push_type": push_type, "priority": priority,
            "expiration": expiration.unwrap_or(0), "collapse_id": collapse_id, "payload": payload,
        })
        .to_string();
        let ts = now().to_string();
        let digest = hex::encode(ring::digest::digest(&ring::digest::SHA256, body.as_bytes()));
        let sig = identity.sign(format!("openu60-push\n{ts}\n{digest}").as_bytes());
        let response = client
            .post(url)
            .header("content-type", "application/json")
            .header("x-openu60-key", hex::encode(identity.public))
            .header("x-openu60-time", ts)
            .header("x-openu60-sig", hex::encode(sig))
            .body(body)
            .send();
        let response = match response {
            Ok(r) => r,
            Err(e) => return Outcome::Retry(format!("push gateway transport: {e}")),
        };
        let http = response.status().as_u16();
        let reply: Value = response.json().unwrap_or(Value::Null);
        if http != 200 {
            let why = reply["error"].as_str().unwrap_or("error").to_string();
            return if http == 429 || http >= 500 || why == "clock skew" {
                Outcome::Retry(format!("push gateway {http} {why}"))
            } else {
                Outcome::Rejected(format!("push gateway {http} {why}"))
            };
        }
        let status = reply["status"].as_u64().unwrap_or(502) as u16;
        if status == 200 {
            return Outcome::Accepted;
        }
        classify(status, reply["reason"].as_str().unwrap_or(""))
    }
}

/// Maps Apple's status and reason to what `deliver` does next.
fn classify(status: u16, reason: &str) -> Outcome {
    if matches!(reason, "BadDeviceToken" | "DeviceTokenNotForTopic" | "Unregistered" | "BadEnvironmentKeyInToken") {
        // Wrong environment for this token, or revoked: `deliver` tries the other one.
        return Outcome::Rejected(format!("TOKEN:APNs {status} {reason}"));
    }
    if status == 429 || status >= 500 {
        Outcome::Retry(format!("APNs {status} {reason}"))
    } else {
        Outcome::Rejected(format!("APNs {status} {reason}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation() {
        assert!(valid_token(&"ab".repeat(32)));
        assert!(!valid_token("xyz"));
        assert!(!valid_token(&"a".repeat(33)));
    }
}

#[cfg(test)]
mod notify_tests {
    use super::*;

    #[test]
    fn number_hash_matches_formats() {
        let a = number_hash("+86 138 0013 8000");
        assert_eq!(a, number_hash("13800138000"));
        assert_eq!(a, number_hash("0086-138-0013-8000"));
        assert_ne!(a, number_hash("13800138001"));
        assert_eq!(number_hash("【招商银行】"), number_hash(" 【招商银行】 "));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn codes_are_recognised() {
        assert!(looks_like_code("【U60助手】验证码 482913，5 分钟内有效。"));
        assert!(looks_like_code("Your verification code is 4821"));
        assert!(looks_like_code("动态密码:123456"));
        assert!(!looks_like_code("晚上一起吃饭？"));
        assert!(!looks_like_code("验证码已发送，请注意查收"));
        assert!(!looks_like_code("Meet at 1900 tonight"));
    }
}
