//! Remote access: the router end of the OpenU60 tunnel (see tunnel/ and
//! docs/design/remote_tunnel_2026-11-06.md). The service registers with the relays, accepts
//! paired phones and hands their streams to this HTTP server and the voice bridge; this module
//! wires it into the agent and exposes `/api/tunnel/*`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use openu60_tunnel::device::DeviceService;
use serde_json::{json, Value};
use tiny_http::Method;

use crate::handlers::AppState;

const DATA_DIR: &str = "/data/openu60/tunnel";

pub fn new_service() -> Option<Arc<DeviceService>> {
    openu60_tunnel::logger::init_stderr(log::LevelFilter::Info);
    let dir = std::env::var("ZTE_TUNNEL_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(DATA_DIR));
    match DeviceService::new(dir) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[tunnel] disabled: {e}");
            None
        }
    }
}

/// Starts relay registration and the Bonjour advertisement. mDNS queries arrive as multicast
/// on every interface, including the Wi-Fi uplink the firewall treats as WAN, so the matching
/// INPUT rule is added here (idempotent; the firewall is rebuilt on every boot anyway).
pub fn start(service: &Arc<DeviceService>) {
    allow_mdns();
    service.set_mdns_name("U60 Pro");
    service.start();
}

fn allow_mdns() {
    let rule = ["INPUT", "-p", "udp", "--dport", "5353", "-d", "224.0.0.251", "-j", "ACCEPT"];
    let exists = Command::new("iptables").arg("-C").args(rule).output().map(|o| o.status.success()).unwrap_or(false);
    if !exists {
        let _ = Command::new("iptables").arg("-I").args(rule).output();
    }
}

/// Dispatches the tunnel routes; `None` for anything else.
pub fn route(method: &Method, path: &str, state: &AppState, body: &[u8]) -> Option<(u16, Value)> {
    let rest = path.strip_prefix("/api/tunnel")?;
    let Some(svc) = state.tunnel.as_ref() else {
        return Some((503, json!({"ok": false, "error": "tunnel service unavailable"})));
    };
    let parsed = || -> Result<Value, (u16, Value)> {
        serde_json::from_slice(body).map_err(|_| (400, json!({"ok": false, "error": "invalid JSON"})))
    };
    Some(match (method, rest) {
        (&Method::Get, "/info") => (200, json!({"ok": true, "data": svc.info()})),
        (&Method::Get, "/status") | (&Method::Get, "") => (200, json!({"ok": true, "data": svc.status()})),
        (&Method::Post, "/pair") => {
            let v = match parsed() {
                Ok(v) => v,
                Err(r) => return Some(r),
            };
            let key = v["public_key"].as_str().unwrap_or("");
            let name = v["name"].as_str().unwrap_or("Phone");
            match svc.pair(key, name) {
                Ok(info) => (200, json!({"ok": true, "data": info})),
                Err(e) => (400, json!({"ok": false, "error": e})),
            }
        }
        (&Method::Post, "/unpair") => {
            let v = match parsed() {
                Ok(v) => v,
                Err(r) => return Some(r),
            };
            match svc.unpair(v["public_key"].as_str().unwrap_or("")) {
                Ok(removed) => (200, json!({"ok": true, "data": {"removed": removed}})),
                Err(e) => (500, json!({"ok": false, "error": e})),
            }
        }
        (&Method::Put, "/config") => {
            let v = match parsed() {
                Ok(v) => v,
                Err(r) => return Some(r),
            };
            match svc.set_config(&v) {
                Ok(info) => (200, json!({"ok": true, "data": info})),
                Err(e) => (400, json!({"ok": false, "error": e})),
            }
        }
        (&Method::Post, "/probe") => {
            let v = match parsed() {
                Ok(v) => v,
                Err(r) => return Some(r),
            };
            let r = svc.probe(v["relay"].as_str().unwrap_or(""));
            (200, json!({"ok": true, "data": r}))
        }
        _ => (404, json!({"ok": false, "error": "not found"})),
    })
}
