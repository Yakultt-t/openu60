//! Phone role: find the router through a relay, punch a hole if possible, open the QUIC
//! connection, and expose it as loopback ports the app already knows how to talk to.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::forward::EchoSink;
use crate::identity::{self, Identity};
use crate::mux::{Mux, MuxEvent, MuxSocket};
use crate::proto::{self, DeviceId, PubKey, Role};
use crate::voice::{VoiceChannel, VoiceStats};
use crate::{forward, tls};

#[derive(Clone, Debug)]
pub struct PhoneParams {
    pub relays: Vec<String>,
    pub device_id: DeviceId,
    pub device_key: PubKey,
    /// Preferred loopback ports; 0 or a busy port falls back to an ephemeral one.
    pub tcp_port: u16,
    pub udp_port: u16,
    /// Skip hole punching and go through the relay (diagnostics).
    pub force_relay: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TunnelInfo {
    pub relay: String,
    pub path: String,
    /// False when the router does not know this phone yet: only login and pairing work.
    pub paired: bool,
    pub peer: String,
    pub public_addr: String,
    pub tcp_port: u16,
    pub udp_port: u16,
}

pub struct PhoneTunnel {
    conn: quinn::Connection,
    endpoint: quinn::Endpoint,
    mux: Arc<Mux>,
    voice: Arc<VoiceChannel>,
    echo: EchoSink,
    info: TunnelInfo,
    tasks: Vec<JoinHandle<()>>,
}

/// Result of [`PhoneTunnel::probe_voice`]: how the audio path behaves right now.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VoiceProbe {
    pub sent: u32,
    pub received: u32,
    pub loss_percent: f64,
    pub rtt_min_ms: f64,
    pub rtt_avg_ms: f64,
    pub rtt_max_ms: f64,
    /// Mean absolute difference between consecutive round trips.
    pub jitter_ms: f64,
}

async fn next_event(
    events: &mut UnboundedReceiver<MuxEvent>,
    wait: Duration,
    mut want: impl FnMut(&MuxEvent) -> bool,
) -> Option<MuxEvent> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match timeout(left, events.recv()).await {
            Ok(Some(ev)) if want(&ev) => return Some(ev),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

impl PhoneTunnel {
    /// Tries each relay in turn; the first one that gets a connection up wins.
    pub async fn connect(identity: Arc<Identity>, params: PhoneParams) -> Result<PhoneTunnel, String> {
        if params.relays.is_empty() {
            return Err("no relay configured".into());
        }
        let mut errors = Vec::new();
        for relay in &params.relays {
            match timeout(Duration::from_secs(25), Self::connect_via(Arc::clone(&identity), &params, relay)).await {
                Ok(Ok(t)) => return Ok(t),
                Ok(Err(e)) => errors.push(format!("{relay}: {e}")),
                Err(_) => errors.push(format!("{relay}: timed out")),
            }
        }
        Err(errors.join("; "))
    }

    async fn connect_via(identity: Arc<Identity>, params: &PhoneParams, relay: &str) -> Result<PhoneTunnel, String> {
        let (mux, mut events) = Mux::bind(Arc::clone(&identity), Role::Phone, relay.to_string())
            .await
            .map_err(|e| format!("bind: {e}"))?;
        mux.start();
        let guard = MuxGuard(Arc::clone(&mux));

        let registered = next_event(&mut events, Duration::from_secs(8), |e| matches!(e, MuxEvent::Registered { .. })).await;
        let Some(MuxEvent::Registered { public_addr }) = registered else {
            return Err("relay did not answer".into());
        };

        // The router re-registers a moment after a relay restart; give it a few seconds.
        let mut link_peer = None;
        for attempt in 0..4 {
            mux.connect(params.device_id).await.map_err(|e| format!("connect: {e}"))?;
            let answer = next_event(&mut events, Duration::from_secs(6), |e| {
                matches!(e, MuxEvent::Link { .. } | MuxEvent::ConnectFail { .. })
            })
            .await;
            match answer {
                Some(MuxEvent::Link { link, peer, .. }) => {
                    link_peer = Some((link, peer));
                    break;
                }
                Some(MuxEvent::ConnectFail { reason }) if reason == proto::FAIL_DEVICE_OFFLINE && attempt < 3 => {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
                Some(MuxEvent::ConnectFail { reason }) => return Err(proto::fail_reason_text(reason).into()),
                _ => return Err("relay did not introduce the router".into()),
            }
        }
        let Some((link, peer)) = link_peer else { return Err("router is offline".into()) };

        // Hole punch: both sides probe for ~1.5 s; a reply from the router's public address
        // means a direct path exists.
        let direct = if params.force_relay {
            None
        } else {
            let deadline = Instant::now() + Duration::from_millis(1500);
            let mut found = None;
            while Instant::now() < deadline && found.is_none() {
                let _ = mux.send_probe(link, peer, false).await;
                if let Some(MuxEvent::ProbeReply { from, .. }) =
                    next_event(&mut events, Duration::from_millis(200), |e| matches!(e, MuxEvent::ProbeReply { link: l, .. } if *l == link)).await
                {
                    found = Some(from);
                }
            }
            found
        };

        let client_cfg = tls::client_config(&identity, params.device_key)?;
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Arc::new(MuxSocket(Arc::clone(&mux))),
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(|e| format!("endpoint: {e}"))?;
        endpoint.set_default_client_config(client_cfg);
        let server_name = format!("{}.openu60", identity::device_id_hex(&params.device_id));

        let mut attempts: Vec<(SocketAddr, &str)> = Vec::new();
        if let Some(d) = direct {
            attempts.push((d, "direct"));
        }
        attempts.push((proto::virtual_addr(link), "relay"));
        let mut last_err = String::new();
        let mut connected = None;
        for (addr, path) in attempts {
            log::info!("quic handshake via {path} to {addr}");
            let attempt = async {
                endpoint.connect(addr, &server_name).map_err(|e| e.to_string())?.await.map_err(|e| e.to_string())
            };
            match timeout(Duration::from_secs(8), attempt).await {
                Ok(Ok(c)) => {
                    connected = Some((c, path, addr));
                    break;
                }
                Ok(Err(e)) => last_err = format!("{path}: {e}"),
                Err(_) => last_err = format!("{path}: handshake timed out"),
            }
        }
        let (conn, path, addr) = connected.ok_or(last_err)?;
        let paired = hello(&conn).await?;
        if !paired {
            log::warn!("router does not know this phone yet: tunnel limited to login and pairing");
        }

        let tcp = bind_tcp(params.tcp_port).await?;
        let udp = bind_udp(params.udp_port).await?;
        let info = TunnelInfo {
            relay: relay.to_string(),
            path: path.to_string(),
            paired,
            peer: addr.to_string(),
            public_addr: public_addr.to_string(),
            tcp_port: tcp.local_addr().map(|a| a.port()).unwrap_or(0),
            udp_port: udp.local_addr().map(|a| a.port()).unwrap_or(0),
        };
        log::info!("tunnel up via {path}: http 127.0.0.1:{} voice 127.0.0.1:{}", info.tcp_port, info.udp_port);

        let voice = Arc::new(VoiceChannel::from_connection(&conn, true)?);
        let echo: EchoSink = Arc::new(std::sync::Mutex::new(None));
        let serve = tokio::spawn(forward::phone_serve(
            conn.clone(),
            Arc::clone(&mux),
            Arc::clone(&voice),
            Arc::clone(&echo),
            tcp,
            udp,
        ));
        let drain = tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                log::debug!("mux event {ev:?}");
            }
        });
        std::mem::forget(guard);
        Ok(PhoneTunnel { conn, endpoint, mux, voice, echo, info, tasks: vec![serve, drain] })
    }

    pub fn info(&self) -> &TunnelInfo {
        &self.info
    }

    pub fn voice_stats(&self) -> VoiceStats {
        self.voice.stats()
    }

    /// Sends `count` echo packets `interval` apart over the voice path and measures what comes
    /// back: the same path a call's audio takes.
    pub async fn probe_voice(&self, count: u32, interval: Duration) -> VoiceProbe {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        *self.echo.lock().unwrap() = Some(tx);
        let peer = self.conn.remote_address();
        let start = Instant::now();
        let sender = {
            let voice = Arc::clone(&self.voice);
            let mux = Arc::clone(&self.mux);
            tokio::spawn(async move {
                for i in 0..count {
                    let t = start.elapsed().as_micros() as u64;
                    let mut payload = Vec::with_capacity(12);
                    payload.extend_from_slice(&i.to_be_bytes());
                    payload.extend_from_slice(&t.to_be_bytes());
                    let _ = mux.send_raw(peer, &voice.seal_echo(false, &payload)).await;
                    tokio::time::sleep(interval).await;
                }
            })
        };
        let deadline = start + interval * count + Duration::from_millis(1500);
        let mut rtts: Vec<f64> = Vec::with_capacity(count as usize);
        let mut seen = std::collections::HashSet::new();
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            match timeout(left, rx.recv()).await {
                Ok(Some(p)) if p.len() >= 12 => {
                    let i = u32::from_be_bytes(p[0..4].try_into().unwrap());
                    let sent_us = u64::from_be_bytes(p[4..12].try_into().unwrap());
                    if seen.insert(i) {
                        rtts.push((start.elapsed().as_micros() as u64).saturating_sub(sent_us) as f64 / 1000.0);
                    }
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        sender.abort();
        *self.echo.lock().unwrap() = None;
        let received = rtts.len() as u32;
        let mut probe = VoiceProbe { sent: count, received, ..Default::default() };
        probe.loss_percent = if count > 0 { (count - received) as f64 * 100.0 / count as f64 } else { 0.0 };
        if !rtts.is_empty() {
            probe.rtt_min_ms = rtts.iter().cloned().fold(f64::MAX, f64::min);
            probe.rtt_max_ms = rtts.iter().cloned().fold(0.0, f64::max);
            probe.rtt_avg_ms = rtts.iter().sum::<f64>() / rtts.len() as f64;
            if rtts.len() > 1 {
                probe.jitter_ms = rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (rtts.len() - 1) as f64;
            }
        }
        probe
    }

    pub fn is_alive(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    pub fn status(&self) -> Value {
        json!({
            "connected": self.is_alive(),
            "rtt_ms": self.conn.rtt().as_millis() as u64,
            "path": self.info.path,
            "paired": self.info.paired,
            "relay": self.info.relay,
            "peer": self.info.peer,
            "public_addr": self.info.public_addr,
            "tcp_port": self.info.tcp_port,
            "udp_port": self.info.udp_port,
            "relay_registered": self.mux.is_registered(),
            "close_reason": self.conn.close_reason().map(|r| r.to_string()),
            "voice": self.voice.stats(),
        })
    }

    /// Resolves once the QUIC connection is gone, for whatever reason.
    pub async fn closed(&self) {
        self.conn.closed().await;
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
        for t in &self.tasks {
            t.abort();
        }
        self.endpoint.close(0u32.into(), b"bye");
        self.mux.close();
    }
}

impl Drop for PhoneTunnel {
    fn drop(&mut self) {
        self.close();
    }
}

struct MuxGuard(Arc<Mux>);

impl Drop for MuxGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// One application round trip: proves the router accepted our certificate (a stranger's
/// handshake "succeeds" on the client side and is torn down a moment later) and tells us
/// whether it knows us (`true`) or only offers login + pairing (`false`).
async fn hello(conn: &quinn::Connection) -> Result<bool, String> {
    let exchange = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&[forward::KIND_PING]).await.map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())?;
        let mut b = [0u8; 1];
        recv.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        match b[0] {
            forward::KIND_PING => Ok(true),
            forward::KIND_PING_LIMITED => Ok(false),
            _ => Err("unexpected hello reply".to_string()),
        }
    };
    match timeout(Duration::from_secs(6), exchange).await {
        Ok(Ok(paired)) => Ok(paired),
        Ok(Err(e)) => Err(match conn.close_reason() {
            Some(reason) => format!("router refused the tunnel: {reason}"),
            None => format!("hello failed: {e}"),
        }),
        Err(_) => Err("router did not answer the hello".into()),
    }
}

async fn bind_tcp(preferred: u16) -> Result<TcpListener, String> {
    if preferred != 0 {
        if let Ok(l) = TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)).await {
            return Ok(l);
        }
    }
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| format!("local tcp: {e}"))
}

async fn bind_udp(preferred: u16) -> Result<UdpSocket, String> {
    if preferred != 0 {
        if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, preferred)).await {
            return Ok(s);
        }
    }
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| format!("local udp: {e}"))
}
