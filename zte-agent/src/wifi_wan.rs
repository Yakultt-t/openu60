//! Wi-Fi uplink ("station"/repeater mode): join a home Wi-Fi so the router can stay at
//! home with cellular data off while SMS/voice keep working.
//!
//! The STD (international) firmware ships zwrt_wlan's sta_* methods but removed the helper
//! script they shell out to, so this module drives its own station stack through
//! /data/local/tmp/wifi_uplink.sh (wpa_supplicant + udhcpc + low-metric default route + NAT).

use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::handlers::AppState;

const SCRIPT: &str = "/data/local/tmp/wifi_uplink.sh";

/// Connect runs in the background (association + DHCP can take ~50 s, longer than any
/// client timeout); this records what the last attempt did so /status can report it.
#[derive(Default, Clone)]
struct ConnectState {
    in_progress: bool,
    target_ssid: String,
    last_error: Option<String>,
}

static CONNECT: OnceLock<Mutex<ConnectState>> = OnceLock::new();

fn connect_state() -> &'static Mutex<ConnectState> {
    CONNECT.get_or_init(|| Mutex::new(ConnectState::default()))
}

fn run(args: &[&str], psk: Option<&str>) -> Result<Value, String> {
    let mut cmd = Command::new("sh");
    cmd.arg(SCRIPT).args(args);
    cmd.env_remove("WIFI_PSK");
    if let Some(psk) = psk {
        cmd.env("WIFI_PSK", psk);
    }
    let output = cmd.output().map_err(|e| format!("uplink script exec: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().rev().find(|l| l.trim_start().starts_with('{') || l.trim_start().starts_with('['));
    match line {
        Some(l) => serde_json::from_str(l.trim()).map_err(|e| format!("uplink script JSON: {e}: {}", l.trim())),
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("uplink script produced no result (exit {:?}): {}", output.status.code(), stderr.trim()))
        }
    }
}

fn err(status: u16, e: String) -> (u16, Value) {
    (status, json!({"ok": false, "error": e}))
}

/// GET /api/wifi/wan/status
pub fn status(_state: &AppState) -> (u16, Value) {
    match run(&["status"], None) {
        Ok(v) => {
            let ipv4: Vec<String> = v["ip"].as_str().filter(|s| !s.is_empty()).map(|s| vec![s.to_string()]).unwrap_or_default();
            let cs = connect_state().lock().map(|c| c.clone()).unwrap_or_default();
            (200, json!({"ok": true, "data": {
                "connecting": cs.in_progress,
                "target_ssid": cs.target_ssid,
                "last_error": cs.last_error,
                "up": v["up"].as_bool().unwrap_or(false),
                "connected": v["connected"].as_bool().unwrap_or(false),
                "available": true,
                "pending": v["state"].as_str().map(|s| s != "COMPLETED" && s != "DISCONNECTED" && s != "INACTIVE").unwrap_or(false),
                "device": "wlan4",
                "ipv4": ipv4,
                "gateway": v["gateway"],
                "dns": [],
                "uptime_secs": 0,
                "mode": v["mode"],
                "hotspot_2g": v["hotspot_2g"],
                "hotspot_5g": v["hotspot_5g"],
                "station": {
                    "sta_ssid": v["ssid"],
                    "bssid": v["bssid"],
                    "freq": v["freq"],
                    "signal": v["signal"],
                    "state": v["state"],
                },
            }}))
        }
        Err(e) => err(503, e),
    }
}

/// POST /api/wifi/wan/scan — synchronous on this firmware; results are returned by GET too.
pub fn scan_start(_state: &AppState) -> (u16, Value) {
    match run(&["scan"], None) {
        Ok(list) => (200, json!({"ok": true, "data": {"networks": list}})),
        Err(e) => err(503, e),
    }
}

/// GET /api/wifi/wan/scan
pub fn scan_results(state: &AppState) -> (u16, Value) {
    scan_start(state)
}

/// POST /api/wifi/wan/connect — body {"ssid": "...", "psk": "...", "bssid": "optional"}
pub fn connect(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return err(400, "invalid JSON".into()),
    };
    let ssid = parsed["ssid"].as_str().unwrap_or("").trim().to_string();
    if ssid.is_empty() {
        return err(400, "missing 'ssid'".into());
    }
    let psk = parsed["psk"].as_str().or_else(|| parsed["password"].as_str()).unwrap_or("").to_string();
    let bssid = parsed["bssid"].as_str().unwrap_or("").to_string();
    eprintln!("[wifi-wan] connect ssid={ssid:?}");
    {
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        if cs.in_progress {
            return err(409, format!("already connecting to {}", cs.target_ssid));
        }
        cs.in_progress = true;
        cs.target_ssid = ssid.clone();
        cs.last_error = None;
    }
    let spawned = std::thread::Builder::new().name("wifi-uplink-connect".into()).spawn(move || {
        let mut args = vec!["connect", ssid.as_str()];
        if !bssid.is_empty() {
            args.push(bssid.as_str());
        }
        let result = run(&args, if psk.is_empty() { None } else { Some(&psk) });
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        cs.in_progress = false;
        cs.last_error = match result {
            Ok(v) if v["ok"].as_bool().unwrap_or(false) => None,
            Ok(v) => Some(v["error"].as_str().unwrap_or("connect failed").to_string()),
            Err(e) => Some(e),
        };
        if let Some(e) = &cs.last_error {
            eprintln!("[wifi-wan] connect failed: {e}");
        } else {
            eprintln!("[wifi-wan] connected");
        }
    });
    if let Err(e) = spawned {
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        cs.in_progress = false;
        return err(503, format!("cannot spawn connect thread: {e}"));
    }
    (202, json!({"ok": true, "data": {"status": "connecting"}}))
}

/// POST /api/wifi/wan/disconnect
pub fn disconnect(_state: &AppState) -> (u16, Value) {
    if let Ok(mut cs) = connect_state().lock() {
        cs.last_error = None;
        cs.target_ssid.clear();
    }
    match run(&["disconnect"], None) {
        Ok(v) => (200, json!({"ok": true, "data": v})),
        Err(e) => err(503, e),
    }
}

/// PUT /api/wifi/wan/mode — body {"mode": "hotspot"|"client"|"repeater", "ssid", "psk", "bssid"}.
/// hotspot: uplink off, both hotspot radios on. client: join the Wi-Fi, then (20 s later)
/// turn the hotspot off. repeater: join the Wi-Fi and keep the hotspot on the other band.
pub fn mode_set(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return err(400, "invalid JSON".into()),
    };
    let mode = parsed["mode"].as_str().unwrap_or("").to_string();
    if !["hotspot", "client", "repeater"].contains(&mode.as_str()) {
        return err(400, "mode must be hotspot, client or repeater".into());
    }
    let ssid = parsed["ssid"].as_str().unwrap_or("").trim().to_string();
    let psk = parsed["psk"].as_str().or_else(|| parsed["password"].as_str()).unwrap_or("").to_string();
    let bssid = parsed["bssid"].as_str().unwrap_or("").to_string();
    if mode != "hotspot" && ssid.is_empty() {
        return err(400, "missing 'ssid'".into());
    }
    if mode == "hotspot" {
        return match run(&["mode", "hotspot"], None) {
            Ok(v) => (200, json!({"ok": true, "data": v})),
            Err(e) => err(503, e),
        };
    }
    {
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        if cs.in_progress {
            return err(409, format!("already connecting to {}", cs.target_ssid));
        }
        cs.in_progress = true;
        cs.target_ssid = ssid.clone();
        cs.last_error = None;
    }
    eprintln!("[wifi-wan] mode {mode} ssid={ssid:?}");
    let spawned = std::thread::Builder::new().name("wifi-uplink-mode".into()).spawn(move || {
        let mut args = vec!["mode", mode.as_str(), ssid.as_str()];
        if !bssid.is_empty() {
            args.push(bssid.as_str());
        }
        let result = run(&args, if psk.is_empty() { None } else { Some(&psk) });
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        cs.in_progress = false;
        cs.last_error = match result {
            Ok(v) if v["ok"].as_bool().unwrap_or(false) => None,
            Ok(v) => Some(v["error"].as_str().unwrap_or("mode change failed").to_string()),
            Err(e) => Some(e),
        };
    });
    if let Err(e) = spawned {
        let mut cs = connect_state().lock().unwrap_or_else(|e| e.into_inner());
        cs.in_progress = false;
        return err(503, format!("cannot spawn mode thread: {e}"));
    }
    (202, json!({"ok": true, "data": {"status": "connecting"}}))
}

/// Brings the Wi-Fi uplink back after the firmware reloads its Wi-Fi stack (toggling Wi-Fi 7,
/// changing hotspot settings, the vendor web page...). A reload destroys the station
/// interface; without this the router silently drops off the home network while the phone
/// still looks for it there.
/// True while another copy of the uplink script is joining (rc.local's boot-time restore,
/// or a connect started from the app): the guard must not start a second one beside it.
fn script_busy() -> bool {
    !busy_pids(false).is_empty()
}

/// Pids of uplink scripts that are joining; with `with_scans`, also of `iw ... scan` runs
/// (what a stuck join is usually blocked in).
fn busy_pids(with_scans: bool) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let c = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<&[u8]> = c.split(|b| *b == 0).collect();
            let joining = args.iter().any(|a| a.ends_with(b"wifi_uplink.sh"))
                && args.iter().any(|a| matches!(*a, b"restore" | b"connect" | b"mode"));
            let scan = with_scans
                && args.first().is_some_and(|a| a.ends_with(b"iw"))
                && args.iter().any(|a| *a == b"scan");
            (joining || scan).then_some(pid)
        })
        .collect()
}

/// How long a join may run before the guard calls it stuck. A normal one, including a
/// firmware Wi-Fi reload, finishes in under three minutes.
const STUCK_AFTER_CHECKS: u32 = 10; // x 30 s = 5 min

pub fn start_guard() {
    crate::system::spawn_named("wifi-uplink-guard", || {
        // rc.local restores the uplink 45 s after boot and that takes up to a minute.
        std::thread::sleep(std::time::Duration::from_secs(150));
        let mut misses = 0u32;
        let mut busy_checks = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let busy = connect_state().lock().map(|c| c.in_progress).unwrap_or(true) || script_busy();
            if busy {
                misses = 0;
                busy_checks += 1;
                // A join that never ends (2026-11-10: a scan that never returned after a
                // reboot kept the router offline for 73 min) would block this guard forever.
                // Kill it; the next checks then see the uplink down and restore it afresh.
                if busy_checks >= STUCK_AFTER_CHECKS {
                    busy_checks = 0;
                    let pids = busy_pids(true);
                    eprintln!("[wifi-wan] uplink join still running after 5 min; killing {pids:?}");
                    for pid in pids {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
                continue;
            }
            busy_checks = 0;
            let Ok(v) = run(&["status"], None) else { continue };
            let mode = v["mode"].as_str().unwrap_or("hotspot");
            let up = v["up"].as_bool().unwrap_or(false);
            let state = v["state"].as_str().unwrap_or("");
            let lost = mode != "hotspot" && !up && matches!(state, "INTERFACE_DISABLED" | "DISCONNECTED" | "INACTIVE" | "");
            misses = if lost { misses + 1 } else { 0 };
            // Two checks in a row (about a minute): a reload in progress gets time to finish.
            if misses >= 2 {
                misses = 0;
                eprintln!("[wifi-wan] uplink lost (state {state:?}, mode {mode}); restoring");
                if let Ok(mut cs) = connect_state().lock() {
                    cs.in_progress = true;
                    cs.last_error = None;
                }
                // On its own thread, so this loop keeps watching and can kill it if it hangs.
                let spawned = crate::system::try_spawn_named("wifi-uplink-restore", || {
                    let result = run(&["restore"], None);
                    if let Ok(mut cs) = connect_state().lock() {
                        cs.in_progress = false;
                    }
                    match run(&["status"], None) {
                        Ok(s) if s["up"].as_bool().unwrap_or(false) => eprintln!("[wifi-wan] uplink restored"),
                        _ => eprintln!("[wifi-wan] restore did not bring the uplink up: {:?}", result.err()),
                    }
                });
                if spawned.is_err() {
                    if let Ok(mut cs) = connect_state().lock() {
                        cs.in_progress = false;
                    }
                }
            }
        }
    });
}
