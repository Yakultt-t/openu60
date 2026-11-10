//! Relay wire format. Every relay/probe frame starts with [`MAGIC`] (0x2A): a QUIC packet's first
//! byte always has the fixed bit 0x40 set, so the two can share one UDP socket without ambiguity.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

pub const MAGIC: u8 = 0x2A;
pub const VERSION: u8 = 1;
/// Domain-separation prefix for the registration signature.
pub const SIGN_CONTEXT: &[u8] = b"OU60-relay-register/v1";

pub type Token = [u8; 16];
pub type Nonce = [u8; 16];
pub type PubKey = [u8; 32];
pub type DeviceId = [u8; 8];

pub const T_HELLO: u8 = 0x01;
pub const T_CHALLENGE: u8 = 0x02;
pub const T_REGISTER: u8 = 0x03;
pub const T_REGISTERED: u8 = 0x04;
pub const T_PING: u8 = 0x05;
pub const T_PONG: u8 = 0x06;
pub const T_CONNECT: u8 = 0x07;
pub const T_CONNECT_FAIL: u8 = 0x08;
pub const T_LINK: u8 = 0x09;
pub const T_PROBE: u8 = 0x0A;
pub const T_DATA: u8 = 0x0B;
pub const T_CLOSE: u8 = 0x0C;

/// Byte offset of the token inside a DATA frame (magic, type, then token).
pub const DATA_TOKEN_OFFSET: usize = 2;
/// Total DATA header: magic + type + token + link id.
pub const DATA_HEADER_LEN: usize = 2 + 16 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Device = 1,
    Phone = 2,
}

impl Role {
    pub fn from_u8(v: u8) -> Option<Role> {
        match v {
            1 => Some(Role::Device),
            2 => Some(Role::Phone),
            _ => None,
        }
    }
}

pub const FAIL_DEVICE_OFFLINE: u8 = 1;
pub const FAIL_NOT_REGISTERED: u8 = 2;
pub const FAIL_BUSY: u8 = 3;
pub const FAIL_BAD_ROLE: u8 = 4;

pub fn fail_reason_text(code: u8) -> &'static str {
    match code {
        FAIL_DEVICE_OFFLINE => "device offline",
        FAIL_NOT_REGISTERED => "not registered",
        FAIL_BUSY => "relay busy",
        FAIL_BAD_ROLE => "bad role",
        _ => "unknown",
    }
}

/// Control frames (DATA is handled separately so the payload is never copied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Hello { version: u8 },
    Challenge { nonce: Nonce, addr: SocketAddr },
    Register { role: Role, pubkey: PubKey, nonce: Nonce, sig: [u8; 64] },
    Registered { token: Token, addr: SocketAddr },
    Ping { token: Token },
    Pong { token: Token, addr: SocketAddr },
    Connect { token: Token, device: DeviceId },
    ConnectFail { token: Token, reason: u8 },
    Link { token: Token, link: u32, peer: SocketAddr, peer_role: Role },
    Probe { link: u32, reply: bool },
    Close { token: Token, link: u32 },
}

fn put_addr(out: &mut Vec<u8>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

fn take_addr(buf: &[u8], pos: &mut usize) -> Option<SocketAddr> {
    let fam = *buf.get(*pos)?;
    *pos += 1;
    let ip = match fam {
        4 => {
            let b: [u8; 4] = buf.get(*pos..*pos + 4)?.try_into().ok()?;
            *pos += 4;
            IpAddr::V4(Ipv4Addr::from(b))
        }
        6 => {
            let b: [u8; 16] = buf.get(*pos..*pos + 16)?.try_into().ok()?;
            *pos += 16;
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(buf.get(*pos..*pos + 2)?.try_into().ok()?);
    *pos += 2;
    Some(SocketAddr::new(ip, port))
}

fn take<const N: usize>(buf: &[u8], pos: &mut usize) -> Option<[u8; N]> {
    let b: [u8; N] = buf.get(*pos..*pos + N)?.try_into().ok()?;
    *pos += N;
    Some(b)
}

fn take_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    take::<4>(buf, pos).map(u32::from_be_bytes)
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.push(MAGIC);
        match self {
            Frame::Hello { version } => {
                out.push(T_HELLO);
                out.push(*version);
            }
            Frame::Challenge { nonce, addr } => {
                out.push(T_CHALLENGE);
                out.extend_from_slice(nonce);
                put_addr(&mut out, addr);
            }
            Frame::Register { role, pubkey, nonce, sig } => {
                out.push(T_REGISTER);
                out.push(*role as u8);
                out.extend_from_slice(pubkey);
                out.extend_from_slice(nonce);
                out.extend_from_slice(sig);
            }
            Frame::Registered { token, addr } => {
                out.push(T_REGISTERED);
                out.extend_from_slice(token);
                put_addr(&mut out, addr);
            }
            Frame::Ping { token } => {
                out.push(T_PING);
                out.extend_from_slice(token);
            }
            Frame::Pong { token, addr } => {
                out.push(T_PONG);
                out.extend_from_slice(token);
                put_addr(&mut out, addr);
            }
            Frame::Connect { token, device } => {
                out.push(T_CONNECT);
                out.extend_from_slice(token);
                out.extend_from_slice(device);
            }
            Frame::ConnectFail { token, reason } => {
                out.push(T_CONNECT_FAIL);
                out.extend_from_slice(token);
                out.push(*reason);
            }
            Frame::Link { token, link, peer, peer_role } => {
                out.push(T_LINK);
                out.extend_from_slice(token);
                out.extend_from_slice(&link.to_be_bytes());
                put_addr(&mut out, peer);
                out.push(*peer_role as u8);
            }
            Frame::Probe { link, reply } => {
                out.push(T_PROBE);
                out.extend_from_slice(&link.to_be_bytes());
                out.push(u8::from(*reply));
            }
            Frame::Close { token, link } => {
                out.push(T_CLOSE);
                out.extend_from_slice(token);
                out.extend_from_slice(&link.to_be_bytes());
            }
        }
        out
    }

    /// Parses a control frame. DATA frames return `None`; use [`data_header`] for those.
    pub fn decode(buf: &[u8]) -> Option<Frame> {
        if buf.len() < 2 || buf[0] != MAGIC {
            return None;
        }
        let mut pos = 2;
        let frame = match buf[1] {
            T_HELLO => Frame::Hello { version: *buf.get(pos)? },
            T_CHALLENGE => {
                let nonce = take::<16>(buf, &mut pos)?;
                let addr = take_addr(buf, &mut pos)?;
                Frame::Challenge { nonce, addr }
            }
            T_REGISTER => {
                let role = Role::from_u8(*buf.get(pos)?)?;
                pos += 1;
                let pubkey = take::<32>(buf, &mut pos)?;
                let nonce = take::<16>(buf, &mut pos)?;
                let sig = take::<64>(buf, &mut pos)?;
                Frame::Register { role, pubkey, nonce, sig }
            }
            T_REGISTERED => {
                let token = take::<16>(buf, &mut pos)?;
                let addr = take_addr(buf, &mut pos)?;
                Frame::Registered { token, addr }
            }
            T_PING => Frame::Ping { token: take::<16>(buf, &mut pos)? },
            T_PONG => {
                let token = take::<16>(buf, &mut pos)?;
                let addr = take_addr(buf, &mut pos)?;
                Frame::Pong { token, addr }
            }
            T_CONNECT => {
                let token = take::<16>(buf, &mut pos)?;
                let device = take::<8>(buf, &mut pos)?;
                Frame::Connect { token, device }
            }
            T_CONNECT_FAIL => {
                let token = take::<16>(buf, &mut pos)?;
                let reason = *buf.get(pos)?;
                Frame::ConnectFail { token, reason }
            }
            T_LINK => {
                let token = take::<16>(buf, &mut pos)?;
                let link = take_u32(buf, &mut pos)?;
                let peer = take_addr(buf, &mut pos)?;
                let peer_role = Role::from_u8(*buf.get(pos)?)?;
                Frame::Link { token, link, peer, peer_role }
            }
            T_PROBE => {
                let link = take_u32(buf, &mut pos)?;
                let reply = *buf.get(pos)? != 0;
                Frame::Probe { link, reply }
            }
            T_CLOSE => {
                let token = take::<16>(buf, &mut pos)?;
                let link = take_u32(buf, &mut pos)?;
                Frame::Close { token, link }
            }
            _ => return None,
        };
        Some(frame)
    }
}

/// True when the datagram is one of ours rather than a QUIC packet.
pub fn is_control(buf: &[u8]) -> bool {
    !buf.is_empty() && buf[0] == MAGIC
}

pub fn is_data(buf: &[u8]) -> bool {
    buf.len() >= DATA_HEADER_LEN && buf[0] == MAGIC && buf[1] == T_DATA
}

/// Splits a DATA frame into (token, link id, payload offset).
pub fn data_header(buf: &[u8]) -> Option<(Token, u32, usize)> {
    if !is_data(buf) {
        return None;
    }
    let mut pos = DATA_TOKEN_OFFSET;
    let token = take::<16>(buf, &mut pos)?;
    let link = take_u32(buf, &mut pos)?;
    Some((token, link, pos))
}

pub fn encode_data(token: &Token, link: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DATA_HEADER_LEN + payload.len());
    out.push(MAGIC);
    out.push(T_DATA);
    out.extend_from_slice(token);
    out.extend_from_slice(&link.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Message a client signs to prove it owns the key it registers with.
pub fn register_message(role: Role, nonce: &Nonce) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGN_CONTEXT.len() + 1 + 16);
    m.extend_from_slice(SIGN_CONTEXT);
    m.push(role as u8);
    m.extend_from_slice(nonce);
    m
}

/// Address quinn uses for a peer reached through the relay: 198.18.0.0/15 is reserved for
/// benchmarking and never routed, so it can carry the 32-bit link id (16 bits in the host part,
/// 16 in the port). Link ids never have a zero low half, so the port is always valid.
pub fn virtual_addr(link: u32) -> SocketAddr {
    let hi = (link >> 16) as u16;
    SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(198, 18, (hi >> 8) as u8, hi as u8),
        (link & 0xFFFF) as u16,
    ))
}

pub fn link_of(addr: &SocketAddr) -> Option<u32> {
    match addr {
        SocketAddr::V4(a) => {
            let o = a.ip().octets();
            if o[0] == 198 && o[1] == 18 {
                Some(((o[2] as u32) << 24) | ((o[3] as u32) << 16) | a.port() as u32)
            } else {
                None
            }
        }
        SocketAddr::V6(_) => None,
    }
}

/// Keeps the unused-import lint quiet on platforms without v6 literals in tests.
#[allow(dead_code)]
fn _v6(_: SocketAddrV6) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let addr: SocketAddr = "203.0.113.9:4433".parse().unwrap();
        let frames = vec![
            Frame::Hello { version: 1 },
            Frame::Challenge { nonce: [7; 16], addr },
            Frame::Register { role: Role::Device, pubkey: [1; 32], nonce: [2; 16], sig: [3; 64] },
            Frame::Registered { token: [9; 16], addr },
            Frame::Ping { token: [9; 16] },
            Frame::Pong { token: [9; 16], addr: "[2001:db8::1]:443".parse().unwrap() },
            Frame::Connect { token: [9; 16], device: [5; 8] },
            Frame::ConnectFail { token: [9; 16], reason: FAIL_DEVICE_OFFLINE },
            Frame::Link { token: [9; 16], link: 0x1234_5678, peer: addr, peer_role: Role::Phone },
            Frame::Probe { link: 42, reply: true },
            Frame::Close { token: [9; 16], link: 42 },
        ];
        for f in frames {
            assert_eq!(Frame::decode(&f.encode()), Some(f));
        }
    }

    #[test]
    fn data_frames() {
        let d = encode_data(&[4; 16], 0xABCD_0001, b"hello");
        assert!(is_data(&d));
        let (token, link, off) = data_header(&d).unwrap();
        assert_eq!(token, [4; 16]);
        assert_eq!(link, 0xABCD_0001);
        assert_eq!(&d[off..], b"hello");
        assert_eq!(Frame::decode(&d), None);
    }

    #[test]
    fn virtual_addrs() {
        for link in [0x0000_0001u32, 0xFFFF_FFFF, 0x1234_5678, 0x0001_8000] {
            assert_eq!(link_of(&virtual_addr(link)), Some(link));
        }
        assert_eq!(link_of(&"1.2.3.4:5".parse().unwrap()), None);
    }
}
