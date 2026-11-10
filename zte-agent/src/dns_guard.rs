//! Keeps name resolution working when the carrier's DNS servers stop answering.
//!
//! The firmware writes the operator's resolvers into /etc/resolv.conf, which both the router's
//! own programs and dnsmasq (for every device on the hotspot) use. A China Mobile card roaming
//! abroad got 211.136.x resolvers that never answer through the home-routed tunnel: no speed
//! test, no APNs host lookup, no web for hotspot clients. Every minute this asks each listed
//! resolver for a name; if none answers, public resolvers are added after the carrier one
//! (musl and dnsmasq query them in parallel / in turn), and removed again once the carrier's
//! own resolver works.
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

const RESOLV: &str = "/etc/resolv.conf";
const MARK: &str = "# openu60 fallback";
const FALLBACKS: [&str; 2] = ["223.5.5.5", "1.1.1.1"];

/// One A query for www.apple.com; true if any answer comes back.
fn answers(server: &str) -> bool {
    let Ok(addr) = format!("{server}:53").parse::<SocketAddr>() else { return false };
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return false };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));
    let id: u16 = rand::random();
    let mut q = Vec::with_capacity(32);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in ["www", "apple", "com"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    if sock.send_to(&q, addr).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    match sock.recv_from(&mut buf) {
        Ok((n, _)) => n >= 12 && buf[0..2] == id.to_be_bytes(),
        Err(_) => false,
    }
}

fn read() -> (Vec<String>, Vec<String>) {
    let text = std::fs::read_to_string(RESOLV).unwrap_or_default();
    let mut other = Vec::new();
    let mut carrier = Vec::new();
    let mut in_ours = false;
    for line in text.lines() {
        if line.trim() == MARK {
            in_ours = true;
            continue;
        }
        if let Some(ns) = line.trim().strip_prefix("nameserver") {
            let ns = ns.trim().to_string();
            if !in_ours && !FALLBACKS.contains(&ns.as_str()) {
                carrier.push(ns);
            }
        } else if !line.trim().is_empty() && !in_ours {
            other.push(line.to_string());
        }
    }
    (other, carrier)
}

fn write(other: &[String], carrier: &[String], with_fallback: bool) {
    let mut out = String::new();
    for l in other {
        out.push_str(l);
        out.push('\n');
    }
    // musl reads at most three nameservers: one carrier resolver plus the two fallbacks.
    let keep = if with_fallback { 1 } else { carrier.len() };
    for ns in carrier.iter().take(keep) {
        out.push_str(&format!("nameserver {ns}\n"));
    }
    if with_fallback {
        out.push_str(MARK);
        out.push('\n');
        for ns in FALLBACKS {
            out.push_str(&format!("nameserver {ns}\n"));
        }
    }
    let _ = std::fs::write(RESOLV, out);
}

fn check() {
    let (other, carrier) = read();
    let has_fallback = std::fs::read_to_string(RESOLV).map(|t| t.contains(MARK)).unwrap_or(false);
    if carrier.is_empty() {
        return; // no mobile data right now; nothing to repair
    }
    let carrier_ok = carrier.iter().any(|ns| answers(ns));
    if carrier_ok && has_fallback {
        write(&other, &carrier, false);
        eprintln!("[dns] carrier resolver answers again; fallback removed");
    } else if !carrier_ok && !has_fallback && FALLBACKS.iter().any(|ns| answers(ns)) {
        write(&other, &carrier, true);
        eprintln!("[dns] carrier resolver {} not answering; added public fallback", carrier[0]);
    }
}

pub fn start() {
    crate::system::spawn_named("dns-guard", || {
        std::thread::sleep(Duration::from_secs(20));
        loop {
            check();
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}
