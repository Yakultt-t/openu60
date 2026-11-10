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
//! Configuration (environment):
//!   PUSHGW_LISTEN      address, default 0.0.0.0:443
//!   PUSHGW_CERT/KEY    TLS certificate chain and key (PEM)
//!   PUSHGW_APNS_KEY    Apple .p8 key file
//!   PUSHGW_KEY_ID, PUSHGW_TEAM_ID, PUSHGW_BUNDLE_ID

use std::collections::{HashMap, HashSet};
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
    apns: Apns,
    limits: Mutex<HashMap<[u8; 32], Limits>>,
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

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Full<Bytes>> {
        if req.method() == Method::GET && req.uri().path() == "/healthz" {
            return reply(StatusCode::OK, json!({"ok": true}));
        }
        if req.method() != Method::POST || req.uri().path() != "/v1/push" {
            return reply(StatusCode::NOT_FOUND, json!({"error": "not found"}));
        }
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
        let (status, reason) = self.apns.send(&push).await;
        if status != 200 {
            log(format!("{router} {} {}: APNs {status} {}", push.push_type, push.env, reason.as_deref().unwrap_or("")));
        }
        reply(StatusCode::OK, json!({"status": status, "reason": reason}))
    }
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
    let pem = std::fs::read_to_string(env("PUSHGW_APNS_KEY")).expect("read PUSHGW_APNS_KEY");
    let apns = Apns {
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
    };
    let gw = Arc::new(Gateway { apns, limits: Mutex::new(HashMap::new()) });
    let acceptor = tls_acceptor();
    let listen = std::env::var("PUSHGW_LISTEN").unwrap_or_else(|_| "0.0.0.0:443".into());
    let listener = TcpListener::bind(&listen).await.expect("bind");
    log(format!("listening on {listen}"));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let acceptor = acceptor.clone();
        let gw = gw.clone();
        tokio::spawn(async move {
            let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
            let service = hyper::service::service_fn(move |req| {
                let gw = gw.clone();
                async move { Ok::<_, std::convert::Infallible>(gw.handle(req, peer).await) }
            });
            let _ = tokio::time::timeout(
                Duration::from_secs(120),
                hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), service),
            )
            .await;
        });
    }
}
