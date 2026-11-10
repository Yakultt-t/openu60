//! OpenU60 push gateway.
//!
//! Routers never hold the APNs key. A router sends `POST /v1/push` with a JSON body and three
//! headers: its Ed25519 public key, a Unix timestamp and a signature over
//! `"openu60-push\n<timestamp>\n<sha256(body) hex>"`. The gateway checks the signature, the
//! clock skew and per-router limits, then forwards the push to Apple with its own key and
//! answers `{"status": <APNs status>, "reason": <APNs reason or null>}`.
//!
//! Limits keep one router (or someone who generated a key) from abusing the APNs key: a
//! bounded request rate and a bounded number of distinct device tokens per router per day.
//!
//! Watch link (`/v1/link/*`): a mailbox that lets an Apple Watch reach its router over
//! cellular. The watch posts a sealed blob to `/v1/link/send/<router id>`; the router, which
//! long-polls `/v1/link/poll` (signed like pushes, with "openu60-link" as the label), picks it
//! up and answers through `/v1/link/reply`. Blobs are encrypted end to end between watch and
//! router (see zte-agent/src/watch_link.rs); the gateway only sees sizes and the router id.
//!
//! Configuration (environment):
//!   PUSHGW_LISTEN      address, default 0.0.0.0:443
//!   PUSHGW_CERT/KEY    TLS certificate chain and key (PEM)
//!   PUSHGW_APNS_KEY    Apple .p8 key file (optional: without it only the watch link runs)
//!   PUSHGW_KEY_ID, PUSHGW_TEAM_ID, PUSHGW_BUNDLE_ID

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use ring::signature::{UnparsedPublicKey, ED25519};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify};
use tokio_rustls::TlsAcceptor;

const MAX_BODY: usize = 8 * 1024;
const MAX_SKEW: u64 = 300;
/// Requests per router: a burst of 60, refilled at one per second.
const BURST: f64 = 60.0;
const REFILL_PER_SEC: f64 = 1.0;
/// Distinct device tokens one router may push to per day (a household has a few phones).
const TOKENS_PER_DAY: usize = 30;
const JWT_LIFETIME: u64 = 40 * 60;

struct Apns {
    key: SigningKey,
    key_id: String,
    team_id: String,
    bundle_id: String,
    jwt: Mutex<Option<(String, u64)>>,
    http: reqwest::Client,
}

struct Limits {
    bucket: f64,
    refilled: Instant,
    day: u64,
    tokens: HashSet<String>,
}

struct Gateway {
    apns: Option<Apns>,
    limits: Mutex<HashMap<[u8; 32], Limits>>,
    links: Mutex<HashMap<String, Arc<Mailbox>>>,
    watch_limits: Mutex<HashMap<std::net::IpAddr, (f64, Instant)>>,
    heartbeats: Mutex<HashMap<String, Heartbeat>>,
}

/// Offline alerts: a router posts `/v1/heartbeat` (signed, "openu60-link") once a minute with
/// the devices that want to know when it goes quiet. After `OFFLINE_AFTER` without one the
/// gateway tells them; when heartbeats resume it says the router is back.
const OFFLINE_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_WATCHERS: usize = 10;

/// `PUSHGW_OFFLINE_SECS` shortens the wait for testing.
fn offline_after() -> Duration {
    std::env::var("PUSHGW_OFFLINE_SECS").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(OFFLINE_AFTER)
}

#[derive(Clone, Deserialize)]
struct Watcher {
    token: String,
    #[serde(default)]
    env: String,
    #[serde(default)]
    lang: String,
}

struct Heartbeat {
    last: Instant,
    watchers: Vec<Watcher>,
    alerted: bool,
}

/// Watch link limits.
const LINK_MAX_REQUEST: usize = 16 * 1024;
const LINK_MAX_REPLY: usize = 512 * 1024;
const LINK_MAX_PENDING: usize = 32;
/// How long a router poll is held open, and how long a watch waits for its answer.
const LINK_POLL_HOLD: Duration = Duration::from_secs(25);
const LINK_ANSWER_WAIT: Duration = Duration::from_secs(25);
/// A router that has not polled for this long is reported offline straight away.
const LINK_ONLINE_WINDOW: Duration = Duration::from_secs(75);
/// Watch requests per client address: a burst of 30, refilled at two per second.
const WATCH_BURST: f64 = 30.0;
const WATCH_REFILL: f64 = 2.0;

#[derive(Default)]
struct MailboxState {
    queue: VecDeque<(String, String)>,
    waiting: HashMap<String, oneshot::Sender<String>>,
    last_poll: Option<Instant>,
}

#[derive(Default)]
struct Mailbox {
    state: Mutex<MailboxState>,
    notify: Notify,
}

#[derive(Deserialize)]
struct PushRequest {
    env: String,
    token: String,
    push_type: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    expiration: Option<u64>,
    #[serde(default)]
    collapse_id: Option<String>,
    payload: Value,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[pushgw] {}", msg.as_ref());
}

impl Apns {
    fn jwt(&self) -> String {
        let mut slot = self.jwt.lock().unwrap();
        let t = now();
        if let Some((jwt, issued)) = slot.as_ref() {
            if t.saturating_sub(*issued) < JWT_LIFETIME {
                return jwt.clone();
            }
        }
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "ES256", "kid": self.key_id}).to_string());
        let claims = URL_SAFE_NO_PAD.encode(json!({"iss": self.team_id, "iat": t}).to_string());
        let input = format!("{header}.{claims}");
        let sig: Signature = self.key.sign(input.as_bytes());
        let jwt = format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        *slot = Some((jwt.clone(), t));
        jwt
    }

    async fn send(&self, req: &PushRequest) -> (u16, Option<String>) {
        let host = if req.env == "sandbox" { "api.sandbox.push.apple.com" } else { "api.push.apple.com" };
        let topic = match req.push_type.as_str() {
            "voip" => format!("{}.voip", self.bundle_id),
            "liveactivity" => format!("{}.push-type.liveactivity", self.bundle_id),
            _ => self.bundle_id.clone(),
        };
        let mut r = self
            .http
            .post(format!("https://{host}/3/device/{}", req.token))
            .bearer_auth(self.jwt())
            .header("apns-topic", topic)
            .header("apns-push-type", req.push_type.as_str())
            .header("apns-priority", req.priority.unwrap_or(10).to_string())
            .header("apns-expiration", req.expiration.unwrap_or(0).to_string());
        if let Some(c) = &req.collapse_id {
            r = r.header("apns-collapse-id", c.as_str());
        }
        match r.json(&req.payload).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 200 {
                    return (200, None);
                }
                let reason = resp
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v["reason"].as_str().map(str::to_string));
                (status, reason)
            }
            Err(e) => (502, Some(format!("transport: {e}"))),
        }
    }
}

impl Gateway {
    /// Token bucket plus distinct-token budget; `Err` carries the reason.
    fn admit(&self, key: &[u8; 32], token: &str) -> Result<(), &'static str> {
        let mut all = self.limits.lock().unwrap();
        if all.len() > 100_000 {
            all.retain(|_, l| l.refilled.elapsed() < Duration::from_secs(86_400));
        }
        let day = now() / 86_400;
        let l = all.entry(*key).or_insert_with(|| Limits { bucket: BURST, refilled: Instant::now(), day, tokens: HashSet::new() });
        let elapsed = l.refilled.elapsed().as_secs_f64();
        l.bucket = (l.bucket + elapsed * REFILL_PER_SEC).min(BURST);
        l.refilled = Instant::now();
        if l.day != day {
            l.day = day;
            l.tokens.clear();
        }
        if l.bucket < 1.0 {
            return Err("rate limited");
        }
        if !l.tokens.contains(token) {
            if l.tokens.len() >= TOKENS_PER_DAY {
                return Err("too many device tokens today");
            }
            l.tokens.insert(token.to_string());
        }
        l.bucket -= 1.0;
        Ok(())
    }

    fn mailbox(&self, device: &str) -> Arc<Mailbox> {
        let mut all = self.links.lock().unwrap();
        if all.len() > 50_000 {
            all.retain(|_, m| m.state.lock().unwrap().last_poll.map(|t| t.elapsed() < Duration::from_secs(3600)).unwrap_or(false));
        }
        all.entry(device.to_string()).or_default().clone()
    }

    fn admit_watch(&self, ip: std::net::IpAddr) -> bool {
        let mut all = self.watch_limits.lock().unwrap();
        if all.len() > 100_000 {
            all.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(600));
        }
        let (bucket, at) = all.entry(ip).or_insert((WATCH_BURST, Instant::now()));
        *bucket = (*bucket + at.elapsed().as_secs_f64() * WATCH_REFILL).min(WATCH_BURST);
        *at = Instant::now();
        if *bucket < 1.0 {
            return false;
        }
        *bucket -= 1.0;
        true
    }

    /// Watch side: queue a sealed request for a router and wait for its sealed answer.
    async fn link_send(&self, req: Request<Incoming>, peer: SocketAddr, device: &str) -> Response<Full<Bytes>> {
        if device.len() != 16 || !device.bytes().all(|b| b.is_ascii_hexdigit()) {
            return reply(StatusCode::NOT_FOUND, json!({"error": "unknown router"}));
        }
        if !self.admit_watch(peer.ip()) {
            return reply(StatusCode::TOO_MANY_REQUESTS, json!({"error": "rate limited"}));
        }
        let body = match Limited::new(req.into_body(), LINK_MAX_REQUEST).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "body too large"})),
        };
        let blob = match serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["blob"].as_str().map(str::to_string)) {
            Some(b) if !b.is_empty() => b,
            _ => return reply(StatusCode::BAD_REQUEST, json!({"error": "bad request"})),
        };
        let device = device.to_ascii_lowercase();
        let mailbox = self.mailbox(&device);
        let id = hex::encode(rand_bytes::<12>());
        let (tx, rx) = oneshot::channel();
        {
            let mut st = mailbox.state.lock().unwrap();
            let online = st.last_poll.map(|t| t.elapsed() < LINK_ONLINE_WINDOW).unwrap_or(false);
            if !online {
                return reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "router offline"}));
            }
            if st.queue.len() >= LINK_MAX_PENDING {
                return reply(StatusCode::TOO_MANY_REQUESTS, json!({"error": "router busy"}));
            }
            st.queue.push_back((id.clone(), blob));
            st.waiting.insert(id.clone(), tx);
        }
        mailbox.notify.notify_waiters();
        match tokio::time::timeout(LINK_ANSWER_WAIT, rx).await {
            Ok(Ok(answer)) => reply(StatusCode::OK, json!({"blob": answer})),
            _ => {
                let mut st = mailbox.state.lock().unwrap();
                st.waiting.remove(&id);
                st.queue.retain(|(q, _)| *q != id);
                reply(StatusCode::GATEWAY_TIMEOUT, json!({"error": "router did not answer"}))
            }
        }
    }

    /// Router side: `/v1/link/poll` (hold until a request arrives) and `/v1/link/reply`.
    async fn link_router(&self, req: Request<Incoming>, peer: SocketAddr, path: &str) -> Response<Full<Bytes>> {
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let (Some(key_hex), Some(ts), Some(sig_hex)) = (header("x-openu60-key"), header("x-openu60-time"), header("x-openu60-sig")) else {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "missing signature"}));
        };
        let body = match Limited::new(req.into_body(), LINK_MAX_REPLY + 1024).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "body too large"})),
        };
        let key: [u8; 32] = match hex::decode(&key_hex).ok().and_then(|k| k.try_into().ok()) {
            Some(k) => k,
            None => return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad key"})),
        };
        let Ok(ts_num) = ts.parse::<u64>() else {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad time"}));
        };
        if now().abs_diff(ts_num) > MAX_SKEW {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "clock skew", "server_time": now()}));
        }
        let digest = hex::encode(ring::digest::digest(&ring::digest::SHA256, &body));
        let sig = hex::decode(&sig_hex).unwrap_or_default();
        if UnparsedPublicKey::new(&ED25519, key).verify(format!("openu60-link\n{ts}\n{digest}").as_bytes(), &sig).is_err() {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad signature"}));
        }
        // Same id the tunnel uses: the first 8 bytes of SHA-256 of the router's key.
        let device = hex::encode(&ring::digest::digest(&ring::digest::SHA256, &key).as_ref()[..8]);
        let mailbox = self.mailbox(&device);

        if path == "/v1/heartbeat" {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let watchers: Vec<Watcher> = serde_json::from_value::<Vec<Watcher>>(v["watchers"].clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|w| (32..=512).contains(&w.token.len()) && w.token.bytes().all(|b| b.is_ascii_hexdigit()))
                .take(MAX_WATCHERS)
                .collect();
            let back = {
                let mut all = self.heartbeats.lock().unwrap();
                let entry = all.entry(device.clone()).or_insert(Heartbeat { last: Instant::now(), watchers: Vec::new(), alerted: false });
                entry.last = Instant::now();
                entry.watchers = watchers;
                let was = entry.alerted;
                entry.alerted = false;
                if was { entry.watchers.clone() } else { Vec::new() }
            };
            if !back.is_empty() {
                log(format!("router {device} is back online"));
                self.notify_watchers(&back, ("U60 已恢复在线", "路由器重新连上网络了。"), ("U60 is back online", "The router is reachable again.")).await;
            }
            return reply(StatusCode::OK, json!({"ok": true}));
        }

        if path == "/v1/link/reply" {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (Some(id), Some(blob)) = (v["id"].as_str(), v["blob"].as_str()) else {
                return reply(StatusCode::BAD_REQUEST, json!({"error": "bad reply"}));
            };
            if blob.len() > LINK_MAX_REPLY {
                return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "reply too large"}));
            }
            let waiter = mailbox.state.lock().unwrap().waiting.remove(id);
            return match waiter {
                Some(tx) => {
                    let _ = tx.send(blob.to_string());
                    reply(StatusCode::OK, json!({"ok": true}))
                }
                None => reply(StatusCode::OK, json!({"ok": false, "error": "the watch stopped waiting"})),
            };
        }

        // Poll: hand over what is queued, or wait for something to arrive.
        let deadline = Instant::now() + LINK_POLL_HOLD;
        loop {
            let notified = mailbox.notify.notified();
            {
                let mut st = mailbox.state.lock().unwrap();
                st.last_poll = Some(Instant::now());
                if !st.queue.is_empty() {
                    let requests: Vec<Value> = st.queue.drain(..).map(|(id, blob)| json!({"id": id, "blob": blob})).collect();
                    log(format!("link {device} from {peer}: {} request(s)", requests.len()));
                    return reply(StatusCode::OK, json!({"requests": requests}));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                mailbox.state.lock().unwrap().last_poll = Some(Instant::now());
                return reply(StatusCode::OK, json!({"requests": []}));
            }
        }
    }

    async fn notify_watchers(&self, watchers: &[Watcher], zh: (&str, &str), en: (&str, &str)) {
        let Some(apns) = self.apns.as_ref() else { return };
        for w in watchers {
            let (title, body) = if w.lang == "en" { en } else { zh };
            let push = PushRequest {
                env: if w.env == "sandbox" { "sandbox".into() } else { "production".into() },
                token: w.token.clone(),
                push_type: "alert".into(),
                priority: Some(10),
                expiration: Some(now() + 3600),
                collapse_id: Some("openu60-offline".into()),
                payload: json!({"aps": {"alert": {"title": title, "body": body}, "sound": "default", "thread-id": "openu60-device"}, "kind": "device"}),
            };
            let (status, reason) = apns.send(&push).await;
            if status != 200 {
                log(format!("offline alert: APNs {status} {}", reason.unwrap_or_default()));
            }
        }
    }

    /// Runs forever: tells watchers about routers whose heartbeats stopped.
    async fn watch_heartbeats(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(if offline_after() < Duration::from_secs(60) { 2 } else { 30 })).await;
            let due: Vec<(String, Vec<Watcher>)> = {
                let mut all = self.heartbeats.lock().unwrap();
                all.retain(|_, h| h.last.elapsed() < Duration::from_secs(7 * 86_400));
                all.iter_mut()
                    .filter(|(_, h)| !h.alerted && h.last.elapsed() >= offline_after() && !h.watchers.is_empty())
                    .map(|(id, h)| {
                        h.alerted = true;
                        (id.clone(), h.watchers.clone())
                    })
                    .collect()
            };
            for (id, watchers) in due {
                log(format!("router {id} offline; telling {} device(s)", watchers.len()));
                self.notify_watchers(&watchers, ("U60 已离线", "5 分钟没有收到路由器的消息，可能没电、关机或断网了，这段时间收不到短信和电话。"),
                    ("U60 is offline", "No word from the router for 5 minutes: it may be out of battery, off or without internet. SMS and calls cannot reach you.")).await;
            }
        }
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Full<Bytes>> {
        if req.method() == Method::GET && req.uri().path() == "/healthz" {
            return reply(StatusCode::OK, json!({"ok": true, "push": self.apns.is_some(), "link": true}));
        }
        if req.method() == Method::POST {
            let path = req.uri().path().to_string();
            if let Some(device) = path.strip_prefix("/v1/link/send/") {
                let device = device.to_string();
                return self.link_send(req, peer, &device).await;
            }
            if path == "/v1/link/poll" || path == "/v1/link/reply" || path == "/v1/heartbeat" {
                return self.link_router(req, peer, &path).await;
            }
        }
        if req.method() != Method::POST || req.uri().path() != "/v1/push" {
            return reply(StatusCode::NOT_FOUND, json!({"error": "not found"}));
        }
        let Some(apns) = self.apns.as_ref() else {
            return reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "push is not configured on this gateway"}));
        };
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let (Some(key_hex), Some(ts), Some(sig_hex)) = (header("x-openu60-key"), header("x-openu60-time"), header("x-openu60-sig")) else {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "missing signature"}));
        };
        let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "body too large"})),
        };
        let key: [u8; 32] = match hex::decode(&key_hex).ok().and_then(|k| k.try_into().ok()) {
            Some(k) => k,
            None => return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad key"})),
        };
        let Ok(ts_num) = ts.parse::<u64>() else {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad time"}));
        };
        if now().abs_diff(ts_num) > MAX_SKEW {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "clock skew", "server_time": now()}));
        }
        let digest = hex::encode(ring::digest::digest(&ring::digest::SHA256, &body));
        let message = format!("openu60-push\n{ts}\n{digest}");
        let sig = hex::decode(&sig_hex).unwrap_or_default();
        if UnparsedPublicKey::new(&ED25519, key).verify(message.as_bytes(), &sig).is_err() {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "bad signature"}));
        }
        let push: PushRequest = match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(_) => return reply(StatusCode::BAD_REQUEST, json!({"error": "bad request"})),
        };
        let token_ok = (32..=512).contains(&push.token.len()) && push.token.bytes().all(|b| b.is_ascii_hexdigit());
        if !token_ok || !matches!(push.push_type.as_str(), "alert" | "voip" | "liveactivity" | "background")
            || !matches!(push.env.as_str(), "production" | "sandbox")
        {
            return reply(StatusCode::BAD_REQUEST, json!({"error": "bad push"}));
        }
        let router = hex::encode(&ring::digest::digest(&ring::digest::SHA256, &key).as_ref()[..8]);
        if let Err(why) = self.admit(&key, &push.token) {
            log(format!("{router} from {peer}: refused ({why})"));
            return reply(StatusCode::TOO_MANY_REQUESTS, json!({"error": why}));
        }
        let (status, reason) = apns.send(&push).await;
        if status != 200 {
            log(format!("{router} {} {}: APNs {status} {}", push.push_type, push.env, reason.as_deref().unwrap_or("")));
        }
        reply(StatusCode::OK, json!({"status": status, "reason": reason}))
    }
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut out).expect("random");
    out
}

fn reply(code: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn tls_acceptor() -> TlsAcceptor {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(env("PUSHGW_CERT"))
        .expect("read PUSHGW_CERT")
        .collect::<Result<_, _>>()
        .expect("parse certificate");
    let key = PrivateKeyDer::from_pem_file(env("PUSHGW_KEY")).expect("read PUSHGW_KEY");
    let mut config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("tls versions")
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .expect("certificate and key");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsAcceptor::from(Arc::new(config))
}

#[tokio::main]
async fn main() {
    let apns = match std::env::var("PUSHGW_APNS_KEY").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            let pem = std::fs::read_to_string(&path).expect("read PUSHGW_APNS_KEY");
            Some(Apns {
                key: SigningKey::from_pkcs8_pem(&pem).expect("APNs key must be an ES256 .p8"),
                key_id: env("PUSHGW_KEY_ID"),
                team_id: env("PUSHGW_TEAM_ID"),
                bundle_id: env("PUSHGW_BUNDLE_ID"),
                jwt: Mutex::new(None),
                http: reqwest::Client::builder()
                    .http2_prior_knowledge()
                    .https_only(true)
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(10))
                    .build()
                    .expect("http client"),
            })
        }
        None => {
            log("PUSHGW_APNS_KEY not set: push is off, only the watch link runs");
            None
        }
    };
    let gw = Arc::new(Gateway {
        apns,
        limits: Mutex::new(HashMap::new()),
        links: Mutex::new(HashMap::new()),
        watch_limits: Mutex::new(HashMap::new()),
        heartbeats: Mutex::new(HashMap::new()),
    });
    tokio::spawn(Arc::clone(&gw).watch_heartbeats());
    // Local testing only: plain HTTP when explicitly asked and no certificate is given.
    let insecure = std::env::var("PUSHGW_CERT").is_err() && std::env::var("PUSHGW_INSECURE_HTTP").as_deref() == Ok("1");
    let acceptor = if insecure { None } else { Some(tls_acceptor()) };
    let listen = std::env::var("PUSHGW_LISTEN").unwrap_or_else(|_| "0.0.0.0:443".into());
    let listener = TcpListener::bind(&listen).await.expect("bind");
    log(format!("listening on {listen}{}", if insecure { " (plain HTTP, local testing only)" } else { "" }));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let acceptor = acceptor.clone();
        let gw = gw.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let gw = gw.clone();
                async move { Ok::<_, std::convert::Infallible>(gw.handle(req, peer).await) }
            });
            let http = hyper::server::conn::http1::Builder::new();
            match acceptor {
                Some(acceptor) => {
                    let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
                    let _ = tokio::time::timeout(Duration::from_secs(120), http.serve_connection(TokioIo::new(tls), service)).await;
                }
                None => {
                    let _ = tokio::time::timeout(Duration::from_secs(120), http.serve_connection(TokioIo::new(tcp), service)).await;
                }
            }
        });
    }
}
