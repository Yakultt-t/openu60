//! OpenU60 remote-access tunnel.
//!
//! Phone ⇄ router traffic is QUIC with both ends pinned to each other's Ed25519 key, so the
//! rendezvous/relay server in the middle only ever sees ciphertext. See
//! `docs/design/remote_tunnel_2026-11-06.md` for the wire protocol.

pub mod device;
pub mod forward;
pub mod identity;
pub mod logger;
#[cfg(feature = "mdns")]
pub mod mdns;
pub mod mux;
pub mod phone;
pub mod proto;
pub mod relay;
pub mod tls;
pub mod voice;

pub use identity::Identity;
pub use proto::{DeviceId, PubKey, Role};

/// Service type the router advertises on the LAN (Bonjour / mDNS-SD).
pub const MDNS_SERVICE: &str = "_openu60._tcp.local.";
/// Rendezvous nodes used when none are configured. Empty unless the build sets
/// `OPENU60_DEFAULT_RELAYS` (comma-separated `host:port`), so remote access stays off until
/// it is pointed at a relay you run yourself (see `relay/`).
pub fn default_relays() -> Vec<String> {
    option_env!("OPENU60_DEFAULT_RELAYS")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}
