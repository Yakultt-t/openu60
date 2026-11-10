//! One UDP socket per relay that carries three things at once: the relay control session
//! (register, keepalive, link setup), hole-punch probes, and the QUIC packets themselves. QUIC
//! sees it through [`MuxSocket`], an abstract socket that turns a peer "address" in the
//! 198.18.0.0/15 range into a DATA frame bounced off the relay, and passes every other address
//! straight through to the wire.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use crate::identity::Identity;
use crate::proto::{self, DeviceId, Frame, Role, Token};
use crate::voice;

pub const PING_INTERVAL: Duration = Duration::from_secs(10);
/// No PONG (or relayed data) for this long means the relay session is gone.
pub const RELAY_TIMEOUT: Duration = Duration::from_secs(45);
const INBOX_CAP: usize = 4096;

#[derive(Debug, Clone)]
pub enum MuxEvent {
    Registered { public_addr: SocketAddr },
    RelayLost,
    Link { link: u32, peer: SocketAddr, peer_role: Role },
    ConnectFail { reason: u8 },
    ProbeReply { link: u32, from: SocketAddr },
    ProbeRequest { link: u32, from: SocketAddr },
    LinkClosed { link: u32 },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MuxStatus {
    pub relay: String,
    pub relay_addr: Option<String>,
    pub registered: bool,
    pub public_addr: Option<String>,
    pub local_port: u16,
    pub links: usize,
    pub last_pong_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LinkInfo {
    pub peer: SocketAddr,
    pub peer_role: Role,
    /// Peer address a probe reply came back from: the direct path works.
    pub direct: Option<SocketAddr>,
}

#[derive(Default)]
struct Inbox {
    queue: VecDeque<(SocketAddr, Bytes)>,
    waker: Option<Waker>,
}

struct Session {
    relay_addr: Option<SocketAddr>,
    token: Option<Token>,
    public_addr: Option<SocketAddr>,
    last_pong: Option<Instant>,
    links: HashMap<u32, LinkInfo>,
}

pub struct Mux {
    sock: UdpSocket,
    identity: Arc<Identity>,
    role: Role,
    relay_host: String,
    session: Mutex<Session>,
    inbox: Mutex<Inbox>,
    events: mpsc::UnboundedSender<MuxEvent>,
    stop: watch::Sender<bool>,
    /// Voice packets bypass quinn: delivered to whoever registered for the peer address.
    voice: Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Bytes>>>,
}

impl fmt::Debug for Mux {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mux").field("relay", &self.relay_host).field("role", &self.role).finish()
    }
}

impl Mux {
    /// Binds a fresh IPv4 socket. Call [`Mux::start`] to begin talking to the relay.
    pub async fn bind(
        identity: Arc<Identity>,
        role: Role,
        relay_host: String,
    ) -> io::Result<(Arc<Mux>, mpsc::UnboundedReceiver<MuxEvent>)> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        let (events, rx) = mpsc::unbounded_channel();
        let (stop, _) = watch::channel(false);
        let mux = Arc::new(Mux {
            sock,
            identity,
            role,
            relay_host,
            session: Mutex::new(Session {
                relay_addr: None,
                token: None,
                public_addr: None,
                last_pong: None,
                links: HashMap::new(),
            }),
            inbox: Mutex::new(Inbox::default()),
            events,
            stop,
            voice: Mutex::new(HashMap::new()),
        });
        Ok((mux, rx))
    }

    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.recv_loop().await });
        let me = Arc::clone(self);
        tokio::spawn(async move { me.session_loop().await });
    }

    pub fn close(&self) {
        let _ = self.stop.send(true);
    }

    pub fn relay_host(&self) -> &str {
        &self.relay_host
    }

    pub fn local_port(&self) -> u16 {
        self.sock.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn is_registered(&self) -> bool {
        self.session.lock().unwrap().token.is_some()
    }

    pub fn public_addr(&self) -> Option<SocketAddr> {
        self.session.lock().unwrap().public_addr
    }

    pub fn link(&self, link: u32) -> Option<LinkInfo> {
        self.session.lock().unwrap().links.get(&link).cloned()
    }

    pub fn set_direct(&self, link: u32, addr: SocketAddr) {
        if let Some(l) = self.session.lock().unwrap().links.get_mut(&link) {
            l.direct = Some(addr);
        }
    }

    pub fn status(&self) -> MuxStatus {
        let s = self.session.lock().unwrap();
        MuxStatus {
            relay: self.relay_host.clone(),
            relay_addr: s.relay_addr.map(|a| a.to_string()),
            registered: s.token.is_some(),
            public_addr: s.public_addr.map(|a| a.to_string()),
            local_port: self.local_port(),
            links: s.links.len(),
            last_pong_secs: s.last_pong.map(|t| t.elapsed().as_secs()),
        }
    }

    /// Asks the relay to introduce us to `device`. The answer arrives as a `Link` or
    /// `ConnectFail` event.
    pub async fn connect(&self, device: DeviceId) -> io::Result<()> {
        let (token, relay) = self.registered_session()?;
        self.send_frame(&Frame::Connect { token, device }, relay).await
    }

    pub async fn send_probe(&self, link: u32, to: SocketAddr, reply: bool) -> io::Result<()> {
        self.send_frame(&Frame::Probe { link, reply }, to).await
    }

    pub async fn close_link(&self, link: u32) {
        if let Ok((token, relay)) = self.registered_session() {
            let _ = self.send_frame(&Frame::Close { token, link }, relay).await;
        }
        self.session.lock().unwrap().links.remove(&link);
    }

    /// Voice packets from `peer` (a direct or virtual address) will arrive on the receiver.
    pub fn register_voice(&self, peer: SocketAddr) -> mpsc::UnboundedReceiver<Bytes> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.voice.lock().unwrap().insert(peer, tx);
        rx
    }

    pub fn unregister_voice(&self, peer: SocketAddr) {
        self.voice.lock().unwrap().remove(&peer);
    }

    /// Sends a non-QUIC packet to a peer, through the relay when the address is virtual.
    pub async fn send_raw(&self, dest: SocketAddr, data: &[u8]) -> io::Result<()> {
        if let Some(link) = proto::link_of(&dest) {
            let (token, relay) = self.registered_session()?;
            self.sock.send_to(&proto::encode_data(&token, link, data), relay).await.map(|_| ())
        } else {
            self.sock.send_to(data, dest).await.map(|_| ())
        }
    }

    fn dispatch_voice(&self, from: SocketAddr, data: Bytes) {
        let voice = self.voice.lock().unwrap();
        if let Some(tx) = voice.get(&from) {
            let _ = tx.send(data);
        }
    }

    fn registered_session(&self) -> io::Result<(Token, SocketAddr)> {
        let s = self.session.lock().unwrap();
        match (s.token, s.relay_addr) {
            (Some(t), Some(a)) => Ok((t, a)),
            _ => Err(io::Error::new(io::ErrorKind::NotConnected, "not registered with relay")),
        }
    }

    async fn send_frame(&self, frame: &Frame, to: SocketAddr) -> io::Result<()> {
        self.sock.send_to(&frame.encode(), to).await.map(|_| ())
    }

    fn emit(&self, ev: MuxEvent) {
        let _ = self.events.send(ev);
    }

    fn push_inbox(&self, from: SocketAddr, data: Bytes) {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.queue.len() >= INBOX_CAP {
            inbox.queue.pop_front();
        }
        inbox.queue.push_back((from, data));
        if let Some(w) = inbox.waker.take() {
            w.wake();
        }
    }

    async fn recv_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                r = self.sock.recv_from(&mut buf) => match r {
                    Ok((n, src)) => self.handle(&buf[..n], src).await,
                    Err(e) => {
                        // ICMP unreachable surfaces here on some platforms; keep going.
                        log::debug!("recv_from: {e}");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }

    async fn handle(&self, buf: &[u8], src: SocketAddr) {
        if proto::is_data(buf) {
            let Some((token, link, off)) = proto::data_header(buf) else { return };
            let ok = {
                let s = self.session.lock().unwrap();
                s.relay_addr == Some(src) && s.token == Some(token) && s.links.contains_key(&link)
            };
            if ok {
                // Relayed traffic proves the relay session is alive, even if PONGs got lost.
                self.session.lock().unwrap().last_pong = Some(Instant::now());
                let payload = &buf[off..];
                if voice::is_voice(payload) {
                    self.dispatch_voice(proto::virtual_addr(link), Bytes::copy_from_slice(payload));
                } else {
                    self.push_inbox(proto::virtual_addr(link), Bytes::copy_from_slice(payload));
                }
            }
            return;
        }
        if voice::is_voice(buf) {
            self.dispatch_voice(src, Bytes::copy_from_slice(buf));
            return;
        }
        if proto::is_control(buf) {
            if let Some(frame) = Frame::decode(buf) {
                self.handle_control(frame, src).await;
            }
            return;
        }
        self.push_inbox(src, Bytes::copy_from_slice(buf));
    }

    async fn handle_control(&self, frame: Frame, src: SocketAddr) {
        let from_relay = self.session.lock().unwrap().relay_addr == Some(src);
        match frame {
            Frame::Challenge { nonce, addr } if from_relay => {
                self.session.lock().unwrap().public_addr = Some(addr);
                let sig = self.identity.sign(&proto::register_message(self.role, &nonce));
                let reg = Frame::Register { role: self.role, pubkey: self.identity.public, nonce, sig };
                if let Err(e) = self.send_frame(&reg, src).await {
                    log::warn!("register send: {e}");
                }
            }
            Frame::Registered { token, addr } if from_relay => {
                {
                    let mut s = self.session.lock().unwrap();
                    s.token = Some(token);
                    s.public_addr = Some(addr);
                    s.last_pong = Some(Instant::now());
                }
                log::info!("registered with {} as {:?}, public address {addr}", self.relay_host, self.role);
                self.emit(MuxEvent::Registered { public_addr: addr });
            }
            Frame::Pong { token, addr } if from_relay => {
                let mut s = self.session.lock().unwrap();
                if s.token == Some(token) {
                    s.last_pong = Some(Instant::now());
                    if s.public_addr != Some(addr) {
                        log::info!("public address changed to {addr}");
                        s.public_addr = Some(addr);
                    }
                }
            }
            Frame::Link { token, link, peer, peer_role } if from_relay => {
                let accepted = {
                    let mut s = self.session.lock().unwrap();
                    if s.token == Some(token) {
                        s.links.insert(link, LinkInfo { peer, peer_role, direct: None });
                        true
                    } else {
                        false
                    }
                };
                if accepted {
                    log::info!("link {link:#010x} with {peer_role:?} at {peer}");
                    self.emit(MuxEvent::Link { link, peer, peer_role });
                }
            }
            Frame::ConnectFail { token, reason } if from_relay => {
                if self.session.lock().unwrap().token == Some(token) {
                    log::warn!("connect refused: {}", proto::fail_reason_text(reason));
                    self.emit(MuxEvent::ConnectFail { reason });
                }
            }
            Frame::Close { token, link } if from_relay => {
                let removed = {
                    let mut s = self.session.lock().unwrap();
                    s.token == Some(token) && s.links.remove(&link).is_some()
                };
                if removed {
                    self.emit(MuxEvent::LinkClosed { link });
                }
            }
            Frame::Probe { link, reply } => {
                let known = self.session.lock().unwrap().links.contains_key(&link);
                if !known {
                    return;
                }
                if reply {
                    self.set_direct(link, src);
                    self.emit(MuxEvent::ProbeReply { link, from: src });
                } else {
                    let _ = self.send_probe(link, src, true).await;
                    self.emit(MuxEvent::ProbeRequest { link, from: src });
                }
            }
            _ => {}
        }
    }

    /// Resolves the relay, registers, keeps the session alive, and starts over when it dies.
    async fn session_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut backoff = Duration::from_secs(2);
        loop {
            if *stop.borrow() {
                return;
            }
            let relay_addr = match tokio::net::lookup_host(&self.relay_host).await {
                Ok(mut it) => it.find(|a| a.is_ipv4()),
                Err(e) => {
                    log::warn!("resolve {}: {e}", self.relay_host);
                    None
                }
            };
            let Some(relay_addr) = relay_addr else {
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            };
            {
                let mut s = self.session.lock().unwrap();
                s.relay_addr = Some(relay_addr);
                s.token = None;
            }
            let mut registered = false;
            for _ in 0..8 {
                if let Err(e) = self.send_frame(&Frame::Hello { version: proto::VERSION }, relay_addr).await {
                    log::warn!("hello to {relay_addr}: {e}");
                }
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                if self.is_registered() {
                    registered = true;
                    break;
                }
            }
            if !registered {
                log::warn!("relay {} not answering, retry in {:?}", self.relay_host, backoff);
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
            backoff = Duration::from_secs(2);
            loop {
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(PING_INTERVAL) => {}
                }
                let (token, stale) = {
                    let s = self.session.lock().unwrap();
                    let stale = s.last_pong.map(|t| t.elapsed() > RELAY_TIMEOUT).unwrap_or(true);
                    (s.token, stale)
                };
                let Some(token) = token else { break };
                if stale {
                    break;
                }
                if let Err(e) = self.send_frame(&Frame::Ping { token }, relay_addr).await {
                    log::warn!("ping: {e}");
                }
            }
            {
                let mut s = self.session.lock().unwrap();
                s.token = None;
                s.links.clear();
            }
            log::warn!("relay session with {} lost, re-registering", self.relay_host);
            self.emit(MuxEvent::RelayLost);
        }
    }
}

/// quinn's view of a [`Mux`].
pub struct MuxSocket(pub Arc<Mux>);

impl fmt::Debug for MuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MuxSocket({:?})", self.0)
    }
}

struct Poller(Arc<Mux>);

impl fmt::Debug for Poller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MuxPoller")
    }
}

impl UdpPoller for Poller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.0.sock.poll_send_ready(cx)
    }
}

impl AsyncUdpSocket for MuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Poller(Arc::clone(&self.0)))
    }

    fn try_send(&self, t: &Transmit) -> io::Result<()> {
        let mux = &self.0;
        let result = if let Some(link) = proto::link_of(&t.destination) {
            let (token, relay) = {
                let s = mux.session.lock().unwrap();
                match (s.token, s.relay_addr) {
                    (Some(tok), Some(r)) if s.links.contains_key(&link) => (tok, r),
                    // Relay session gone: drop the packet, QUIC retransmits or times out.
                    _ => return Ok(()),
                }
            };
            mux.sock.try_send_to(&proto::encode_data(&token, link, t.contents), relay)
        } else {
            mux.sock.try_send_to(t.contents, t.destination)
        };
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
            Err(e) => {
                log::debug!("send to {}: {e}", t.destination);
                Ok(())
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut inbox = self.0.inbox.lock().unwrap();
        match inbox.queue.pop_front() {
            Some((addr, data)) => {
                let dst = &mut bufs[0];
                let n = data.len().min(dst.len());
                dst[..n].copy_from_slice(&data[..n]);
                let m = RecvMeta { addr, len: n, stride: n, ..RecvMeta::default() };
                meta[0] = m;
                Poll::Ready(Ok(1))
            }
            None => {
                inbox.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.sock.local_addr()
    }

    fn may_fragment(&self) -> bool {
        true
    }
}
