//! Relay addresses as people type them, and a quick "is this relay up?" check.
//!
//! The check sends one HELLO and waits for the relay's CHALLENGE, which every relay answers
//! before any registration, so it needs no identity and leaves no state behind (the relay
//! forgets the challenge after a few seconds).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::net::UdpSocket;

use crate::proto::{self, Frame};

/// Port a relay listens on when the address does not say.
pub const DEFAULT_PORT: u16 = 443;

/// Turns what someone typed ("relay.example.com", "udp://1.2.3.4:8443 ", "[::1]:443") into
/// `host:port`, or None when it cannot be an address.
pub fn normalize_relay(input: &str) -> Option<String> {
    // Host names and schemes are case-insensitive.
    let lowered = input.trim().to_ascii_lowercase();
    let mut s = lowered.as_str();
    for prefix in ["udp://", "relay://", "openu60://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
        }
    }
    let s = s.trim_end_matches('/');
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '/' || c == '@') {
        return None;
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        // [v6] or [v6]:port
        let (h, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if after.is_empty() => DEFAULT_PORT,
            None => return None,
        };
        (format!("[{h}]"), port)
    } else if s.matches(':').count() > 1 {
        return None; // bare IPv6 must be bracketed
    } else if let Some((h, p)) = s.split_once(':') {
        (h.to_string(), p.parse::<u16>().ok()?)
    } else {
        (s.to_string(), DEFAULT_PORT)
    };
    let valid_host = !host.is_empty()
        && host.len() <= 253
        && (host.starts_with('[')
            || host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    (valid_host && port != 0).then(|| format!("{host}:{port}"))
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub ok: bool,
    pub relay: String,
    /// Address the name resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    /// Our address as the relay saw it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Sends HELLO (a few times, UDP may drop one) and waits up to `timeout` for the CHALLENGE.
pub async fn probe_relay(relay: &str, timeout: Duration) -> ProbeResult {
    let mut result = ProbeResult { ok: false, relay: relay.to_string(), addr: None, rtt_ms: None, public_addr: None, error: None };
    let Some(target) = normalize_relay(relay) else {
        result.error = Some("not a relay address".into());
        return result;
    };
    result.relay = target.clone();
    let addr: SocketAddr = match tokio::time::timeout(timeout, tokio::net::lookup_host(&target)).await {
        Ok(Ok(mut it)) => match it.find(|a| a.is_ipv4()) {
            Some(a) => a,
            None => {
                result.error = Some("name has no IPv4 address".into());
                return result;
            }
        },
        Ok(Err(e)) => {
            result.error = Some(format!("cannot resolve: {e}"));
            return result;
        }
        Err(_) => {
            result.error = Some("name lookup timed out".into());
            return result;
        }
    };
    result.addr = Some(addr.to_string());
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            result.error = Some(format!("socket: {e}"));
            return result;
        }
    };
    let hello = Frame::Hello { version: proto::VERSION }.encode();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    let mut attempt = 0u32;
    while Instant::now() < deadline {
        let sent_at = Instant::now();
        if let Err(e) = sock.send_to(&hello, addr).await {
            result.error = Some(format!("send: {e}"));
            return result;
        }
        attempt += 1;
        // Wait a little longer each round, but never past the deadline.
        let wait = Duration::from_millis(600 * attempt as u64).min(deadline.saturating_duration_since(Instant::now()));
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) if from == addr => {
                    if let Some(Frame::Challenge { addr: seen, .. }) = Frame::decode(&buf[..n]) {
                        result.ok = true;
                        result.rtt_ms = Some(sent_at.elapsed().as_millis() as u64);
                        result.public_addr = Some(seen.to_string());
                        return result;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    result.error = Some(format!("receive: {e}"));
                    return result;
                }
                Err(_) => break,
            }
        }
    }
    result.error = Some("no answer (check the address, the UDP port and the server's firewall)".into());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize_relay("relay.example.com").as_deref(), Some("relay.example.com:443"));
        assert_eq!(normalize_relay(" udp://Relay.Example.com:8443/ ").as_deref(), Some("relay.example.com:8443"));
        assert_eq!(normalize_relay("UDP://Relay.Example.com:8443").as_deref(), Some("relay.example.com:8443"));
        assert_eq!(normalize_relay("203.0.113.9").as_deref(), Some("203.0.113.9:443"));
        assert_eq!(normalize_relay("[2001:db8::1]:500").as_deref(), Some("[2001:db8::1]:500"));
        assert_eq!(normalize_relay("2001:db8::1"), None);
        assert_eq!(normalize_relay("host:0"), None);
        assert_eq!(normalize_relay("host:99999"), None);
        assert_eq!(normalize_relay("http://x.com/path"), None);
        assert_eq!(normalize_relay(""), None);
        assert_eq!(normalize_relay("bad host"), None);
    }

    #[tokio::test]
    async fn probes_a_live_relay() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let relay = crate::relay::Relay::new(sock);
        tokio::spawn(async move { relay.serve().await });
        let r = probe_relay(&addr.to_string(), Duration::from_secs(3)).await;
        assert!(r.ok, "{r:?}");
        assert!(r.public_addr.is_some());

        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let r = probe_relay(&dead_addr.to_string(), Duration::from_millis(800)).await;
        assert!(!r.ok);
    }
}
