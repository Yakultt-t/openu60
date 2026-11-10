//! The relay itself: introduces phones to routers and forwards their encrypted packets when a
//! direct path cannot be punched. It never holds a key that could decrypt anything. Lives in
//! the library so tests can run one in-process; the `openu60-relay` binary is a thin wrapper.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::identity::{device_id_of, device_id_hex, verify};
use crate::proto::{self, DeviceId, Frame, Role, Token};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// A session is dropped when no PING arrives for this long.
const SESSION_TTL: Duration = Duration::from_secs(90);
/// A HELLO challenge is only valid this long.
const CHALLENGE_TTL: Duration = Duration::from_secs(20);
const MAX_SESSIONS: usize = 20_000;
const MAX_SESSIONS_PER_IP: usize = 64;
const MAX_LINKS_PER_SESSION: usize = 16;

struct Session {
    addr: SocketAddr,
    role: Role,
    device: DeviceId,
    last_seen: Instant,
    links: Vec<u32>,
    relayed_bytes: u64,
}

struct Link {
    a: Token,
    b: Token,
}

#[derive(Default)]
struct State {
    challenges: HashMap<SocketAddr, (proto::Nonce, Instant)>,
    sessions: HashMap<Token, Session>,
    /// Devices by id: only Role::Device sessions are listed here.
    devices: HashMap<DeviceId, Token>,
    links: HashMap<u32, Link>,
}

impl State {
    fn sessions_from(&self, ip: std::net::IpAddr) -> usize {
        self.sessions.values().filter(|s| s.addr.ip() == ip).count()
    }

    fn new_link_id(&self) -> u32 {
        loop {
            let id: u32 = rand::random();
            if id & 0xFFFF != 0 && !self.links.contains_key(&id) {
                return id;
            }
        }
    }

    fn drop_session(&mut self, token: &Token) -> Vec<(u32, Token)> {
        let mut notify = Vec::new();
        if let Some(s) = self.sessions.remove(token) {
            if s.role == Role::Device && self.devices.get(&s.device) == Some(token) {
                self.devices.remove(&s.device);
            }
            for link in s.links {
                if let Some(l) = self.links.remove(&link) {
                    let other = if l.a == *token { l.b } else { l.a };
                    notify.push((link, other));
                    if let Some(o) = self.sessions.get_mut(&other) {
                        o.links.retain(|x| *x != link);
                    }
                }
            }
        }
        notify
    }
}

pub struct Relay {
    sock: UdpSocket,
    state: Mutex<State>,
}

impl Relay {
    async fn send(&self, frame: &Frame, to: SocketAddr) {
        if let Err(e) = self.sock.send_to(&frame.encode(), to).await {
            log::debug!("send to {to}: {e}");
        }
    }

    async fn handle(&self, buf: &mut [u8], src: SocketAddr) {
        if proto::is_data(buf) {
            self.forward(buf, src).await;
            return;
        }
        let Some(frame) = Frame::decode(buf) else { return };
        match frame {
            Frame::Hello { version } => {
                if version != proto::VERSION {
                    log::debug!("{src}: unsupported version {version}");
                    return;
                }
                let nonce: proto::Nonce = rand::random();
                {
                    let mut st = self.state.lock().await;
                    if st.challenges.len() > MAX_SESSIONS {
                        st.challenges.retain(|_, (_, t)| t.elapsed() < CHALLENGE_TTL);
                    }
                    st.challenges.insert(src, (nonce, Instant::now()));
                }
                self.send(&Frame::Challenge { nonce, addr: src }, src).await;
            }
            Frame::Register { role, pubkey, nonce, sig } => {
                let reply = {
                    let mut st = self.state.lock().await;
                    let Some((expected, issued)) = st.challenges.remove(&src) else {
                        log::debug!("{src}: register without challenge");
                        return;
                    };
                    if expected != nonce || issued.elapsed() > CHALLENGE_TTL {
                        log::debug!("{src}: stale challenge");
                        return;
                    }
                    if !verify(&pubkey, &proto::register_message(role, &nonce), &sig) {
                        log::warn!("{src}: bad registration signature");
                        return;
                    }
                    if st.sessions.len() >= MAX_SESSIONS || st.sessions_from(src.ip()) >= MAX_SESSIONS_PER_IP {
                        log::warn!("{src}: session limit reached");
                        return;
                    }
                    // One session per address: a client that re-registers replaces itself.
                    let stale: Vec<Token> = st.sessions.iter().filter(|(_, s)| s.addr == src).map(|(t, _)| *t).collect();
                    for t in stale {
                        st.drop_session(&t);
                    }
                    let token: Token = rand::random();
                    let device = device_id_of(&pubkey);
                    if role == Role::Device {
                        if let Some(old) = st.devices.insert(device, token) {
                            st.drop_session(&old);
                        }
                    }
                    st.sessions.insert(token, Session {
                        addr: src,
                        role,
                        device,
                        last_seen: Instant::now(),
                        links: Vec::new(),
                        relayed_bytes: 0,
                    });
                    log::info!("{src}: registered {role:?} {} ({} sessions)", device_id_hex(&device), st.sessions.len());
                    Frame::Registered { token, addr: src }
                };
                self.send(&reply, src).await;
            }
            Frame::Ping { token } => {
                let ok = {
                    let mut st = self.state.lock().await;
                    match st.sessions.get_mut(&token) {
                        Some(s) => {
                            s.last_seen = Instant::now();
                            if s.addr != src {
                                log::info!("{}: address moved {} -> {src}", device_id_hex(&s.device), s.addr);
                                s.addr = src;
                            }
                            true
                        }
                        None => false,
                    }
                };
                if ok {
                    self.send(&Frame::Pong { token, addr: src }, src).await;
                }
            }
            Frame::Connect { token, device } => {
                let outcome = {
                    let mut st = self.state.lock().await;
                    let Some(me) = st.sessions.get(&token) else {
                        return;
                    };
                    if me.addr != src {
                        return;
                    }
                    if me.role != Role::Phone {
                        Err(proto::FAIL_BAD_ROLE)
                    } else if me.links.len() >= MAX_LINKS_PER_SESSION {
                        Err(proto::FAIL_BUSY)
                    } else {
                        match st.devices.get(&device).copied() {
                            Some(dev_token) if st.sessions.contains_key(&dev_token) => {
                                let link = st.new_link_id();
                                let dev_addr = st.sessions[&dev_token].addr;
                                st.links.insert(link, Link { a: token, b: dev_token });
                                st.sessions.get_mut(&token).unwrap().links.push(link);
                                st.sessions.get_mut(&dev_token).unwrap().links.push(link);
                                log::info!("link {link:#010x}: phone {src} <-> device {} {dev_addr}", device_id_hex(&device));
                                Ok((link, dev_token, dev_addr))
                            }
                            _ => Err(proto::FAIL_DEVICE_OFFLINE),
                        }
                    }
                };
                match outcome {
                    Ok((link, dev_token, dev_addr)) => {
                        self.send(&Frame::Link { token, link, peer: dev_addr, peer_role: Role::Device }, src).await;
                        self.send(&Frame::Link { token: dev_token, link, peer: src, peer_role: Role::Phone }, dev_addr).await;
                    }
                    Err(reason) => self.send(&Frame::ConnectFail { token, reason }, src).await,
                }
            }
            Frame::Close { token, link } => {
                let notify = {
                    let mut st = self.state.lock().await;
                    let Some(me) = st.sessions.get(&token) else { return };
                    if me.addr != src {
                        return;
                    }
                    match st.links.remove(&link) {
                        Some(l) if l.a == token || l.b == token => {
                            let other = if l.a == token { l.b } else { l.a };
                            for t in [token, other] {
                                if let Some(s) = st.sessions.get_mut(&t) {
                                    s.links.retain(|x| *x != link);
                                }
                            }
                            st.sessions.get(&other).map(|s| (other, s.addr))
                        }
                        Some(l) => {
                            st.links.insert(link, l);
                            None
                        }
                        None => None,
                    }
                };
                if let Some((other, addr)) = notify {
                    self.send(&Frame::Close { token: other, link }, addr).await;
                }
            }
            _ => {}
        }
    }

    /// DATA: swap the sender's token for the receiver's and pass the frame on untouched.
    async fn forward(&self, buf: &mut [u8], src: SocketAddr) {
        let Some((token, link, _)) = proto::data_header(buf) else { return };
        let dest = {
            let mut st = self.state.lock().await;
            let Some(l) = st.links.get(&link) else { return };
            let other = if l.a == token {
                l.b
            } else if l.b == token {
                l.a
            } else {
                return;
            };
            let Some(me) = st.sessions.get_mut(&token) else { return };
            if me.addr != src {
                return;
            }
            // Traffic is proof of life too: a lost PING must not expire a session that is
            // carrying a call.
            me.last_seen = Instant::now();
            me.relayed_bytes += buf.len() as u64;
            let Some(peer) = st.sessions.get(&other) else { return };
            (other, peer.addr)
        };
        buf[proto::DATA_TOKEN_OFFSET..proto::DATA_TOKEN_OFFSET + 16].copy_from_slice(&dest.0);
        if let Err(e) = self.sock.send_to(buf, dest.1).await {
            log::debug!("forward to {}: {e}", dest.1);
        }
    }

    async fn sweep(&self) {
        let mut notify = Vec::new();
        let (sessions, links) = {
            let mut st = self.state.lock().await;
            st.challenges.retain(|_, (_, t)| t.elapsed() < CHALLENGE_TTL);
            let dead: Vec<Token> = st
                .sessions
                .iter()
                .filter(|(_, s)| s.last_seen.elapsed() > SESSION_TTL)
                .map(|(t, _)| *t)
                .collect();
            for t in dead {
                if let Some(s) = st.sessions.get(&t) {
                    log::info!("{}: session expired ({} relayed bytes)", device_id_hex(&s.device), s.relayed_bytes);
                }
                for (link, other) in st.drop_session(&t) {
                    if let Some(o) = st.sessions.get(&other) {
                        notify.push((link, other, o.addr));
                    }
                }
            }
            (st.sessions.len(), st.links.len())
        };
        for (link, token, addr) in notify {
            self.send(&Frame::Close { token, link }, addr).await;
        }
        log::debug!("{sessions} sessions, {links} links");
    }
}

impl Relay {
    pub fn new(sock: UdpSocket) -> Arc<Self> {
        Arc::new(Relay { sock, state: Mutex::new(State::default()) })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    /// Serves until the future is dropped.
    pub async fn serve(self: Arc<Self>) {
        let _sweeper = {
            let relay = Arc::clone(&self);
            AbortOnDrop(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    relay.sweep().await;
                }
            }))
        };
        let mut buf = vec![0u8; 2048];
        loop {
            match self.sock.recv_from(&mut buf).await {
                Ok((n, src)) => self.handle(&mut buf[..n], src).await,
                Err(e) => {
                    log::debug!("recv: {e}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
