//! Watch link: lets an Apple Watch reach the router over cellular, without the iPhone.
//!
//! watchOS lets ordinary apps use only HTTP(S), not the UDP tunnel the phone uses, so the
//! watch talks to the router through the HTTPS push gateway (pushgw) instead:
//!
//!   watch --HTTPS--> gateway /v1/link/send/<device id>   (queued for the router)
//!   router --HTTPS--> gateway /v1/link/poll               (long poll, signed with the router key)
//!   router --HTTPS--> gateway /v1/link/reply              (signed)
//!
//! Every request and reply is sealed end to end with ChaCha20-Poly1305 under a 32-byte key
//! that only the router and the watch hold (the phone fetches it with `POST /api/watch/link`
//! and hands it to the watch over WatchConnectivity), so the gateway forwards blobs it cannot
//! read. The router answers only a short list of read-and-SMS calls (`ALLOWED`).
//!
//! Blob: 1 byte version (1) || 12 byte nonce || ciphertext+tag. AAD: "openu60-link/1/q|r/<id>".
//! Plaintext request: {"id": str, "ts": unix, "m": "GET"|"POST", "p": "/api/...", "b": json}.
//! Plaintext reply:   {"id": str, "s": http status, "b": json}.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use serde_json::{json, Value};
use tiny_http::Method;

use crate::handlers::AppState;

const FILE: &str = "/data/openu60/watch_link.json";
const VERSION: u8 = 1;
const MAX_SKEW: u64 = 300;
const POLL_TIMEOUT: Duration = Duration::from_secs(40);

/// What the watch may ask for. Reading status and messages, sending and marking SMS; no
/// calls, settings or anything that changes the router.
const ALLOWED: &[(&str, &str)] = &[
    ("GET", "/api/watch/status"),
    ("GET", "/api/network/signal"),
    ("GET", "/api/battery"),
    ("GET", "/api/device"),
    ("POST", "/api/sms/list"),
    ("POST", "/api/sms/send"),
    ("POST", "/api/sms/read"),
    ("GET", "/api/call/missed"),
];

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Gateway base URL, from the push gateway setting (`https://host/v1/push` -> `https://host`).
fn gateway_base() -> Option<String> {
    let url = crate::push::gateway_url()?;
    let base = url.trim_end_matches('/').trim_end_matches("/v1/push").to_string();
    base.starts_with("https://").then_some(base)
}

fn load_key() -> Option<[u8; 32]> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(FILE).ok()?).ok()?;
    B64.decode(v["key"].as_str()?).ok()?.try_into().ok()
}

fn save_key(key: Option<&[u8; 32]>) -> Result<(), String> {
    match key {
        Some(k) => {
            let body = json!({"key": B64.encode(k), "created": now()}).to_string();
            std::fs::write(FILE, body).map_err(|e| format!("write {FILE}: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(FILE, std::fs::Permissions::from_mode(0o600));
            }
            Ok(())
        }
        None => {
            let _ = std::fs::remove_file(FILE);
            Ok(())
        }
    }
}

fn aad(direction: char, device_id: &str) -> Vec<u8> {
    format!("openu60-link/{VERSION}/{direction}/{device_id}").into_bytes()
}

pub fn seal(key: &[u8; 32], direction: char, device_id: &str, plain: &[u8]) -> Vec<u8> {
    let k = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("32-byte key"));
    let nonce_bytes: [u8; 12] = rand::random();
    let mut buf = plain.to_vec();
    k.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce_bytes), Aad::from(aad(direction, device_id)), &mut buf)
        .expect("seal");
    let mut out = Vec::with_capacity(13 + buf.len());
    out.push(VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&buf);
    out
}

pub fn open(key: &[u8; 32], direction: char, device_id: &str, blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 13 + 16 || blob[0] != VERSION {
        return None;
    }
    let k = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).ok()?);
    let nonce = Nonce::try_assume_unique_for_key(&blob[1..13]).ok()?;
    let mut buf = blob[13..].to_vec();
    let plain = k.open_in_place(nonce, Aad::from(aad(direction, device_id)), &mut buf).ok()?;
    Some(plain.to_vec())
}

/// Request ids seen in the last few minutes, so a captured blob cannot be replayed.
struct Seen {
    order: VecDeque<(u64, String)>,
    ids: HashMap<String, u64>,
}

impl Seen {
    fn fresh(&mut self, id: &str, ts: u64) -> bool {
        let t = now();
        while self.order.front().map(|(at, _)| t.saturating_sub(*at) > 2 * MAX_SKEW).unwrap_or(false) {
            if let Some((_, old)) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        if t.abs_diff(ts) > MAX_SKEW || id.is_empty() || id.len() > 64 || self.ids.contains_key(id) {
            return false;
        }
        self.ids.insert(id.to_string(), t);
        self.order.push_back((t, id.to_string()));
        true
    }
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

/// Runs one decrypted request against the local API, if it is on the list.
fn execute(state: &AppState, req: &Value) -> Value {
    let id = req["id"].as_str().unwrap_or("").to_string();
    let method = req["m"].as_str().unwrap_or("");
    let path = req["p"].as_str().unwrap_or("");
    if !ALLOWED.iter().any(|(m, p)| *m == method && *p == path) {
        return json!({"id": id, "s": 403, "b": {"ok": false, "error": "not allowed from the watch"}});
    }
    let m = if method == "POST" { Method::Post } else { Method::Get };
    let body = if req["b"].is_null() { Vec::new() } else { req["b"].to_string().into_bytes() };
    let (status, out) = crate::server::route(&m, path, state, &body);
    json!({"id": id, "s": status, "b": out})
}

/// Status for the watch's home screen in one call: signal, battery and unread SMS.
pub fn status(state: &AppState) -> (u16, Value) {
    let (_, signal) = crate::handlers::network_signal(state);
    let (_, battery) = crate::handlers::battery(state);
    let list_body = json!({"page": 0, "data_per_page": 100, "mem_store": 2, "tags": 10, "order_by": "order by id desc"}).to_string();
    let (_, sms) = crate::sms::sms_list(state, list_body.as_bytes());
    let unread = sms["data"]["messages"]
        .as_array()
        .map(|list| list.iter().filter(|m| m["tag"].as_str() == Some("1")).count())
        .unwrap_or(0);
    (200, json!({"ok": true, "data": {
        "signal": signal.get("data").cloned().unwrap_or(signal.clone()),
        "battery": battery.get("data").cloned().unwrap_or(battery.clone()),
        "unread": unread,
        "time": now(),
    }}))
}

/// `/api/watch/*` for the phone: create or fetch the link key, revoke it, report state.
pub fn route(method: &Method, path: &str, state: &AppState) -> Option<(u16, Value)> {
    let rest = path.strip_prefix("/api/watch")?;
    Some(match (method, rest) {
        (&Method::Get, "/status") => status(state),
        (&Method::Post, "/link") => {
            let Some(base) = gateway_base() else {
                return Some((503, json!({"ok": false, "error": "this router has no gateway set (ZTE_AGENT_PUSH_GATEWAY)"})));
            };
            let Some(tunnel) = state.tunnel.as_ref() else {
                return Some((503, json!({"ok": false, "error": "tunnel identity unavailable"})));
            };
            let key = match load_key() {
                Some(k) => k,
                None => {
                    let k: [u8; 32] = rand::random();
                    if let Err(e) = save_key(Some(&k)) {
                        return Some((500, json!({"ok": false, "error": e})));
                    }
                    eprintln!("[watch-link] new watch key created");
                    k
                }
            };
            (200, json!({"ok": true, "data": {
                "gateway": base,
                "device_id": tunnel.device_id_hex(),
                "key": B64.encode(key),
            }}))
        }
        (&Method::Delete, "/link") | (&Method::Post, "/link/revoke") => match save_key(None) {
            Ok(()) => {
                eprintln!("[watch-link] watch key revoked");
                (200, json!({"ok": true}))
            }
            Err(e) => (500, json!({"ok": false, "error": e})),
        },
        (&Method::Get, "/link") => (200, json!({"ok": true, "data": {
            "enabled": load_key().is_some(),
            "gateway": gateway_base(),
            "last_poll_ok": LAST_OK.lock().map(|v| *v).unwrap_or(0),
        }})),
        _ => (404, json!({"ok": false, "error": "not found"})),
    })
}

static LAST_OK: Mutex<u64> = Mutex::new(0);

/// Long-polls the gateway while a watch key exists.
pub fn start(state: Arc<AppState>) {
    let Some(base) = gateway_base() else {
        eprintln!("[watch-link] no gateway configured; cellular watch access is off");
        return;
    };
    crate::system::spawn_named("watch-link", move || {
        let client = match reqwest::blocking::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(8))
            .timeout(POLL_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[watch-link] http client: {e}");
                return;
            }
        };
        let mut backoff = 5u64;
        loop {
            let (Some(key), Some(tunnel)) = (load_key(), state.tunnel.clone()) else {
                std::thread::sleep(Duration::from_secs(20));
                continue;
            };
            let identity = Arc::clone(tunnel.identity());
            let device_id = tunnel.device_id_hex();
            let reply = signed_post(&client, &identity, &format!("{base}/v1/link/poll"), "{}");
            let requests = match reply {
                Ok(v) => {
                    backoff = 5;
                    if let Ok(mut t) = LAST_OK.lock() {
                        *t = now();
                    }
                    v["requests"].as_array().cloned().unwrap_or_default()
                }
                Err(e) => {
                    eprintln!("[watch-link] poll: {e}; retry in {backoff} s");
                    std::thread::sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
            for item in requests {
                let (Some(id), Some(blob)) = (item["id"].as_str(), item["blob"].as_str().and_then(|b| B64.decode(b).ok())) else {
                    continue;
                };
                let id = id.to_string();
                let state = Arc::clone(&state);
                let client = client.clone();
                let identity = Arc::clone(&identity);
                let device_id = device_id.clone();
                let base = base.clone();
                crate::system::spawn_named("watch-link-req", move || {
                    let Some(plain) = open(&key, 'q', &device_id, &blob) else {
                        eprintln!("[watch-link] request {id}: cannot open (wrong key?)");
                        return;
                    };
                    let Ok(req) = serde_json::from_slice::<Value>(&plain) else { return };
                    let fresh = {
                        let mut seen = SEEN.lock().unwrap();
                        seen.get_or_insert_with(|| Seen { order: VecDeque::new(), ids: HashMap::new() })
                            .fresh(req["id"].as_str().unwrap_or(""), req["ts"].as_u64().unwrap_or(0))
                    };
                    let answer = if fresh {
                        execute(&state, &req)
                    } else {
                        json!({"id": req["id"], "s": 401, "b": {"ok": false, "error": "stale or replayed request (check the watch clock)"}})
                    };
                    let sealed = seal(&key, 'r', &device_id, answer.to_string().as_bytes());
                    let body = json!({"id": id, "blob": B64.encode(sealed)}).to_string();
                    if let Err(e) = signed_post(&client, &identity, &format!("{base}/v1/link/reply"), &body) {
                        eprintln!("[watch-link] reply {id}: {e}");
                    }
                });
            }
        }
    });
}

pub(crate) fn signed_post(
    client: &reqwest::blocking::Client,
    identity: &openu60_tunnel::identity::Identity,
    url: &str,
    body: &str,
) -> Result<Value, String> {
    let ts = now().to_string();
    let digest = hex::encode(ring::digest::digest(&ring::digest::SHA256, body.as_bytes()));
    let sig = identity.sign(format!("openu60-link\n{ts}\n{digest}").as_bytes());
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-openu60-key", hex::encode(identity.public))
        .header("x-openu60-time", ts)
        .header("x-openu60-sig", hex::encode(sig))
        .body(body.to_string())
        .send()
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().unwrap_or(Value::Null);
    if status != 200 {
        return Err(format!("HTTP {status} {}", v["error"].as_str().unwrap_or("")));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open() {
        let key: [u8; 32] = rand::random();
        let blob = seal(&key, 'q', "abcd", b"{\"x\":1}");
        assert_eq!(open(&key, 'q', "abcd", &blob).as_deref(), Some(&b"{\"x\":1}"[..]));
        // Wrong direction, device or key: refused.
        assert!(open(&key, 'r', "abcd", &blob).is_none());
        assert!(open(&key, 'q', "abce", &blob).is_none());
        assert!(open(&rand::random(), 'q', "abcd", &blob).is_none());
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&key, 'q', "abcd", &tampered).is_none());
    }

    #[test]
    fn replays_are_refused() {
        let mut seen = Seen { order: VecDeque::new(), ids: HashMap::new() };
        let t = now();
        assert!(seen.fresh("a", t));
        assert!(!seen.fresh("a", t));
        assert!(!seen.fresh("b", t - 10 * MAX_SKEW));
        assert!(seen.fresh("c", t + 10));
    }
}
