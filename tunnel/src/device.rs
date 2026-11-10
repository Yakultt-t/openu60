//! Router role: register with every configured relay, accept tunnels from paired phones, and
//! hand their streams to the local HTTP API and voice bridge.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::identity::{self, Identity};
use crate::mux::{Mux, MuxEvent, MuxSocket};
use crate::proto::{PubKey, Role};
use crate::{forward, tls};

fn default_true() -> bool {
    true
}
fn default_relays() -> Vec<String> {
    crate::default_relays()
}
fn default_http_port() -> u16 {
    9090
}
fn default_voice_port() -> u16 {
    9091
}
fn default_bootstrap_port() -> u16 {
    9092
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_voice_port")]
    pub voice_port: u16,
    /// Loopback port that only serves login + pairing, for phones not paired yet.
    #[serde(default = "default_bootstrap_port")]
    pub bootstrap_port: u16,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self { enabled: true, relays: default_relays(), http_port: 9090, voice_port: 9091, bootstrap_port: 9092 }
    }
}

/// Unpaired phones may open at most this many connections per minute (password guessing is
/// throttled again at the bootstrap port).
const UNPAIRED_PER_MINUTE: usize = 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub key: String,
    pub name: String,
    pub added: u64,
    /// Long-lived bearer token handed to this device when it was bound. It stays valid until
    /// the device is unbound, so the app never has to log in again.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnInfo {
    pub peer: String,
    pub name: String,
    pub paired: bool,
    pub remote: String,
    pub path: String,
    pub relay: String,
    pub since: u64,
}

pub struct DeviceService {
    dir: PathBuf,
    identity: Arc<Identity>,
    config: Mutex<DeviceConfig>,
    peers: Mutex<Vec<Peer>>,
    allowed: tls::AllowList,
    runtime: Runtime,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    relays: Mutex<HashMap<String, Arc<Mux>>>,
    conns: Arc<Mutex<HashMap<u64, ConnInfo>>>,
    unpaired_recent: Arc<Mutex<std::collections::VecDeque<std::time::Instant>>>,
    #[cfg(feature = "mdns")]
    mdns: Mutex<Option<crate::mdns::Advertiser>>,
    mdns_name: Mutex<String>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl DeviceService {
    /// `dir` holds `identity.key`, `config.json` and `peers.json`.
    pub fn new(dir: PathBuf) -> Result<Arc<Self>, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let identity = Arc::new(Identity::load_or_create(&dir.join("identity.key"))?);
        let config: DeviceConfig = std::fs::read(dir.join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let peers: Vec<Peer> = std::fs::read(dir.join("peers.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let allowed: HashSet<PubKey> = peers.iter().filter_map(|p| identity::parse_pubkey(&p.key)).collect();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tunnel")
            .on_thread_start(raise_priority)
            .enable_all()
            .build()
            .map_err(|e| format!("tokio: {e}"))?;
        log::info!("device id {} ({} paired phone(s))", identity::device_id_hex(&identity.device_id()), peers.len());
        Ok(Arc::new(Self {
            dir,
            identity,
            config: Mutex::new(config),
            peers: Mutex::new(peers),
            allowed: Arc::new(RwLock::new(allowed)),
            runtime,
            tasks: Mutex::new(Vec::new()),
            relays: Mutex::new(HashMap::new()),
            conns: Arc::new(Mutex::new(HashMap::new())),
            unpaired_recent: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            #[cfg(feature = "mdns")]
            mdns: Mutex::new(None),
            mdns_name: Mutex::new("U60 Pro".to_string()),
        }))
    }

    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    pub fn device_id_hex(&self) -> String {
        identity::device_id_hex(&self.identity.device_id())
    }

    pub fn config(&self) -> DeviceConfig {
        self.config.lock().unwrap().clone()
    }

    /// Starts relay registration (if enabled) and the LAN advertisement.
    pub fn start(self: &Arc<Self>) {
        self.start_relays();
        self.start_mdns();
    }

    fn start_relays(self: &Arc<Self>) {
        self.stop_relays();
        let cfg = self.config();
        if !cfg.enabled {
            log::info!("relay access disabled");
            return;
        }
        let mut tasks = self.tasks.lock().unwrap();
        for relay in cfg.relays {
            let me = Arc::clone(self);
            tasks.push(self.runtime.spawn(async move { me.run_relay(relay).await }));
        }
    }

    fn stop_relays(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        for (_, mux) in self.relays.lock().unwrap().drain() {
            mux.close();
        }
        self.conns.lock().unwrap().clear();
    }

    pub fn set_mdns_name(&self, name: &str) {
        *self.mdns_name.lock().unwrap() = name.to_string();
    }

    #[cfg(feature = "mdns")]
    fn start_mdns(self: &Arc<Self>) {
        self.refresh_mdns();
        // Addresses change when the router switches between hotspot and Wi-Fi client mode.
        let me = Arc::clone(self);
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                me.refresh_mdns();
            }
        });
    }

    #[cfg(feature = "mdns")]
    fn refresh_mdns(&self) {
        let ips = crate::mdns::lan_ipv4_addresses();
        {
            let current = self.mdns.lock().unwrap();
            if let Some(a) = current.as_ref() {
                if a.ips == ips {
                    return;
                }
            }
        }
        // Drop (unregister) the old record before publishing the new address set.
        *self.mdns.lock().unwrap() = None;
        let id = self.device_id_hex();
        let name = self.mdns_name.lock().unwrap().clone();
        let host = format!("u60-{}.local.", &id[..6]);
        let port = self.config().http_port;
        match crate::mdns::advertise(&name, &host, port, &[("id", &id), ("ver", "1")], &ips) {
            Ok(a) => *self.mdns.lock().unwrap() = Some(a),
            Err(e) => log::warn!("mdns: {e}"),
        }
    }

    #[cfg(not(feature = "mdns"))]
    fn start_mdns(self: &Arc<Self>) {}

    async fn run_relay(self: Arc<Self>, relay: String) {
        let (mux, mut events) = match Mux::bind(Arc::clone(&self.identity), Role::Device, relay.clone()).await {
            Ok(x) => x,
            Err(e) => {
                log::error!("bind for {relay}: {e}");
                return;
            }
        };
        let server_cfg = match tls::server_config(&self.identity) {
            Ok(c) => c,
            Err(e) => {
                log::error!("tls: {e}");
                return;
            }
        };
        let endpoint = match quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server_cfg),
            Arc::new(MuxSocket(Arc::clone(&mux))),
            Arc::new(quinn::TokioRuntime),
        ) {
            Ok(e) => e,
            Err(e) => {
                log::error!("endpoint: {e}");
                return;
            }
        };
        mux.start();
        self.relays.lock().unwrap().insert(relay.clone(), Arc::clone(&mux));
        let cfg = self.config();

        let accept = {
            let endpoint = endpoint.clone();
            let conns = Arc::clone(&self.conns);
            let me = Arc::clone(&self);
            let mux = Arc::clone(&mux);
            let relay = relay.clone();
            async move {
                let mut seq = 0u64;
                while let Some(incoming) = endpoint.accept().await {
                    seq += 1;
                    let id = seq;
                    let conns = Arc::clone(&conns);
                    let peers = me.peers_snapshot();
                    let me = Arc::clone(&me);
                    let mux = Arc::clone(&mux);
                    let relay = relay.clone();
                    tokio::spawn(async move {
                        let conn = match incoming.await {
                            Ok(c) => c,
                            Err(e) => {
                                log::info!("handshake failed: {e}");
                                return;
                            }
                        };
                        let key = peer_key(&conn).map(hex::encode).unwrap_or_default();
                        let paired = peers.iter().any(|p| p.key == key);
                        let name = peers.iter().find(|p| p.key == key).map(|p| p.name.clone()).unwrap_or_default();
                        let remote = conn.remote_address();
                        let path = if crate::proto::link_of(&remote).is_some() { "relay" } else { "direct" };
                        if !paired && !me.admit_unpaired() {
                            log::warn!("too many unpaired phones; dropping {remote}");
                            conn.close(1u32.into(), b"busy");
                            return;
                        }
                        log::info!(
                            "phone {name} ({}) connected via {path} {remote}{}",
                            &key[..key.len().min(12)],
                            if paired { "" } else { " [unpaired: bootstrap only]" }
                        );
                        conns.lock().unwrap().insert(id, ConnInfo {
                            peer: key,
                            name,
                            paired,
                            remote: remote.to_string(),
                            path: path.to_string(),
                            relay: relay.clone(),
                            since: now(),
                        });
                        let ports = forward::DevicePorts { http: cfg.http_port, bootstrap: cfg.bootstrap_port, voice: cfg.voice_port };
                        forward::device_serve(conn, Arc::clone(&mux), ports, paired).await;
                        conns.lock().unwrap().remove(&id);
                        if let Some(link) = crate::proto::link_of(&remote) {
                            mux.close_link(link).await;
                        }
                    });
                }
            }
        };
        let events = {
            let mux = Arc::clone(&mux);
            async move {
                while let Some(ev) = events.recv().await {
                    match ev {
                        MuxEvent::Link { link, peer, .. } => {
                            let mux = Arc::clone(&mux);
                            tokio::spawn(async move {
                                for _ in 0..8 {
                                    let _ = mux.send_probe(link, peer, false).await;
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                            });
                        }
                        MuxEvent::ProbeReply { link, from } => {
                            log::info!("direct path to phone confirmed for {link:#010x} at {from}");
                        }
                        MuxEvent::RelayLost => log::warn!("lost relay session"),
                        _ => {}
                    }
                }
            }
        };
        tokio::join!(accept, events);
    }

    /// Sliding one-minute window on connections from phones we do not know.
    fn admit_unpaired(&self) -> bool {
        let mut recent = self.unpaired_recent.lock().unwrap();
        let now = std::time::Instant::now();
        while recent.front().map(|t| now.duration_since(*t).as_secs() >= 60).unwrap_or(false) {
            recent.pop_front();
        }
        if recent.len() >= UNPAIRED_PER_MINUTE {
            return false;
        }
        recent.push_back(now);
        true
    }

    fn peers_snapshot(&self) -> Arc<Vec<Peer>> {
        Arc::new(self.peers.lock().unwrap().clone())
    }

    fn persist(&self) -> Result<(), String> {
        let cfg = serde_json::to_vec_pretty(&*self.config.lock().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join("config.json"), cfg).map_err(|e| format!("write config: {e}"))?;
        let peers = serde_json::to_vec_pretty(&*self.peers.lock().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join("peers.json"), peers).map_err(|e| format!("write peers: {e}"))?;
        Ok(())
    }

    fn refresh_allowed(&self) {
        let set: HashSet<PubKey> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| identity::parse_pubkey(&p.key))
            .collect();
        *self.allowed.write().unwrap() = set;
    }

    /// What a phone needs to reach this router from anywhere.
    pub fn info(&self) -> Value {
        let cfg = self.config();
        json!({
            "device_id": self.device_id_hex(),
            "public_key": hex::encode(self.identity.public),
            "relays": cfg.relays,
            "enabled": cfg.enabled,
        })
    }

    pub fn status(&self) -> Value {
        let relays: Vec<Value> = self
            .relays
            .lock()
            .unwrap()
            .values()
            .map(|m| serde_json::to_value(m.status()).unwrap_or(Value::Null))
            .collect();
        let conns: Vec<Value> = self
            .conns
            .lock()
            .unwrap()
            .values()
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .collect();
        let mut v = self.info();
        v["relays_status"] = Value::Array(relays);
        v["connections"] = Value::Array(conns);
        let peers: Vec<Value> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|p| json!({"key": p.key, "name": p.name, "added": p.added}))
            .collect();
        v["peers"] = Value::Array(peers);
        v
    }

    /// Adds (or renames) a phone by its hex Ed25519 public key.
    pub fn pair(self: &Arc<Self>, key_hex: &str, name: &str) -> Result<Value, String> {
        let key = identity::parse_pubkey(key_hex).ok_or("public key must be 64 hex characters")?;
        let key_hex = hex::encode(key);
        let token = {
            let mut peers = self.peers.lock().unwrap();
            // Re-binding the same device keeps its token, so a second bind never logs it out.
            let existing = peers.iter().find(|p| p.key == key_hex).and_then(|p| p.token.clone());
            let token = existing.unwrap_or_else(|| hex::encode(rand::random::<[u8; 24]>()));
            peers.retain(|p| p.key != key_hex);
            peers.push(Peer { key: key_hex.clone(), name: name.trim().to_string(), added: now(), token: Some(token.clone()) });
            token
        };
        self.refresh_allowed();
        self.persist()?;
        log::info!("paired phone {name} ({})", &key_hex[..12]);
        let mut info = self.info();
        info["device_token"] = Value::String(token);
        Ok(info)
    }

    /// True for the permanent token of a bound device.
    pub fn validate_token(&self, token: &str) -> bool {
        !token.is_empty() && self.peers.lock().unwrap().iter().any(|p| p.token.as_deref() == Some(token))
    }

    pub fn unpair(self: &Arc<Self>, key_hex: &str) -> Result<bool, String> {
        let removed = {
            let mut peers = self.peers.lock().unwrap();
            let before = peers.len();
            peers.retain(|p| !p.key.eq_ignore_ascii_case(key_hex.trim()));
            before != peers.len()
        };
        self.refresh_allowed();
        self.persist()?;
        Ok(removed)
    }

    /// Applies `enabled` and/or `relays` and restarts the relay sessions.
    pub fn set_config(self: &Arc<Self>, patch: &Value) -> Result<Value, String> {
        {
            let mut cfg = self.config.lock().unwrap();
            if let Some(e) = patch.get("enabled").and_then(Value::as_bool) {
                cfg.enabled = e;
            }
            if let Some(list) = patch.get("relays").and_then(Value::as_array) {
                let relays: Vec<String> = list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if relays.is_empty() {
                    return Err("at least one relay is required".into());
                }
                cfg.relays = relays;
            }
        }
        self.persist()?;
        self.start_relays();
        Ok(self.info())
    }
}

/// Voice packets cross these threads; give them a slightly better nice level than the HTTP
/// workers (not real-time: see voice_bridge::raise_audio_priority).
fn raise_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain libc call on the current thread.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -5);
    }
}

fn peer_key(conn: &quinn::Connection) -> Option<PubKey> {
    let identity = conn.peer_identity()?;
    let certs = identity.downcast::<Vec<rustls_pki_types::CertificateDer<'static>>>().ok()?;
    tls::public_key_of_cert(certs.first()?)
}
