//! Online update of the router software (agent, touchscreen program, device scripts).
//!
//! A release publishes `manifest.json` and `manifest.json.sig` (Ed25519 over the exact bytes)
//! next to the files. The agent only trusts a manifest signed with the key below, only writes
//! the five files it knows, and checks every file's SHA-256 before anything is replaced.
//!
//! Applying: download into /data/openu60/update/staging, keep the current files in
//! .../previous, move the new ones in, then exit so the supervisor (zte-agent-run.sh) starts
//! the new agent. A detached watchdog restores the previous files if the new agent has not
//! confirmed itself after two minutes (it confirms after 60 s of uptime), so a bad release
//! cannot leave the router without an agent.
//!
//! Builds say which release line they follow at compile time: OPENU60_CHANNEL ("public" or
//! "relay"; anything else, e.g. a developer's own build, never updates itself) and
//! OPENU60_BUILD (a number that only grows).
//!
//! GET /api/update/status, POST /api/update/check, POST /api/update/apply.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MANIFEST_URL: &str = match option_env!("OPENU60_UPDATE_URL") {
    Some(u) => u,
    None => "https://github.com/Yakultt-t/openu60/releases/download/router-update/manifest.json",
};
/// Ed25519 public key of the release signing key (the private half never leaves the
/// maintainer's machine).
const PUBLIC_KEY_HEX: &str = "25e01f9701de4c7031122fa4981555a5c8fc184b477bafeb7a836f31cc76004f";

pub const CHANNEL: &str = match option_env!("OPENU60_CHANNEL") {
    Some(c) => c,
    None => "dev",
};

/// Release version ("0.2.0"); OPENU60_VERSION is set by the release script.
pub fn version() -> &'static str {
    option_env!("OPENU60_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn build() -> u32 {
    option_env!("OPENU60_BUILD").and_then(|b| b.parse().ok()).unwrap_or(0)
}

const DIR: &str = "/data/openu60/update";

/// Files a release may replace, and where they live.
fn target(name: &str) -> Option<&'static str> {
    Some(match name {
        "zte-agent" => "/data/zte-agent",
        "openu60-screen" => "/data/openu60/screen/openu60-screen",
        "zte-agent-run.sh" => "/data/local/tmp/zte-agent-run.sh",
        "wifi_uplink.sh" => "/data/local/tmp/wifi_uplink.sh",
        "wifi_uplink_dhcp.sh" => "/data/local/tmp/wifi_uplink_dhcp.sh",
        _ => return None,
    })
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct FileEntry {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Manifest {
    pub build: u32,
    pub version: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub notes: Value,
    pub channels: std::collections::HashMap<String, Vec<FileEntry>>,
}

#[derive(Clone, Default, Serialize)]
struct State {
    /// idle, checking, downloading, installing, restarting, failed
    state: String,
    done: u64,
    total: u64,
    error: String,
    checked_at: u64,
    #[serde(skip)]
    latest: Option<Manifest>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State { state: "idle".into(), ..Default::default() }));

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn supported() -> bool {
    CHANNEL == "public" || CHANNEL == "relay"
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into()
}

fn fetch_small(url: &str) -> Result<Vec<u8>, String> {
    let mut r = http().get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let mut buf = Vec::new();
    r.body_mut().as_reader().take(1024 * 1024).read_to_end(&mut buf).map_err(|e| format!("{url}: {e}"))?;
    Ok(buf)
}

/// Downloads and checks the signed manifest.
pub fn fetch_manifest() -> Result<Manifest, String> {
    let body = fetch_small(MANIFEST_URL)?;
    let sig_b64 = fetch_small(&format!("{MANIFEST_URL}.sig"))?;
    verify(&body, &sig_b64)?;
    serde_json::from_slice(&body).map_err(|e| format!("manifest: {e}"))
}

fn verify(body: &[u8], sig_b64: &[u8]) -> Result<(), String> {
    let sig = base64::engine::general_purpose::STANDARD
        .decode(String::from_utf8_lossy(sig_b64).trim())
        .map_err(|_| "signature is not base64".to_string())?;
    let key = hex::decode(PUBLIC_KEY_HEX).map_err(|_| "bad key".to_string())?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(body, &sig)
        .map_err(|_| "update signature is not valid".to_string())
}

fn files_for(m: &Manifest) -> Result<Vec<FileEntry>, String> {
    let files = m.channels.get(CHANNEL).cloned().ok_or_else(|| format!("no files for channel {CHANNEL}"))?;
    for f in &files {
        if target(&f.name).is_none() {
            return Err(format!("unexpected file {}", f.name));
        }
        // Plain http only for a test build pointed at a local server.
        let scheme_ok = f.url.starts_with("https://") || (MANIFEST_URL.starts_with("http://") && f.url.starts_with("http://"));
        if !scheme_ok || f.sha256.len() != 64 {
            return Err(format!("bad entry for {}", f.name));
        }
    }
    if !files.iter().any(|f| f.name == "zte-agent") {
        return Err("release has no agent".into());
    }
    Ok(files)
}

#[derive(Serialize, Deserialize, Default)]
struct LastResult {
    build: u32,
    ok: bool,
    at: u64,
    message: String,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &str) -> Option<T> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

pub fn status() -> (u16, Value) {
    let s = STATE.lock().unwrap().clone();
    let latest = s.latest.as_ref().map(|m| {
        let size: u64 = m.channels.get(CHANNEL).map(|f| f.iter().map(|x| x.size).sum()).unwrap_or(0);
        json!({
            "build": m.build,
            "version": m.version,
            "date": m.date,
            "notes": m.notes,
            "size": size,
            "available": supported() && m.build > build(),
        })
    });
    let last: Option<LastResult> = read_json(&format!("{DIR}/result.json"));
    (200, json!({"ok": true, "data": {
        "channel": CHANNEL,
        "build": build(),
        "version": version(),
        "supported": supported(),
        "state": s.state,
        "done": s.done,
        "total": s.total,
        "error": s.error,
        "checked_at": s.checked_at,
        "latest": latest,
        "last_result": last.map(|l| json!({"build": l.build, "ok": l.ok, "at": l.at, "message": l.message})),
    }}))
}

pub fn check() -> (u16, Value) {
    if !supported() {
        return status();
    }
    {
        let mut s = STATE.lock().unwrap();
        if matches!(s.state.as_str(), "downloading" | "installing" | "restarting") {
            drop(s);
            return status();
        }
        s.state = "checking".into();
        s.error.clear();
    }
    let r = fetch_manifest();
    {
        let mut s = STATE.lock().unwrap();
        s.state = "idle".into();
        s.checked_at = now();
        match r {
            Ok(m) => s.latest = Some(m),
            Err(e) => {
                eprintln!("[update] check failed: {e}");
                s.error = e;
            }
        }
    }
    status()
}

pub fn apply() -> (u16, Value) {
    if !supported() {
        return (400, json!({"ok": false, "error": "this build does not update itself"}));
    }
    let manifest = {
        let mut s = STATE.lock().unwrap();
        if matches!(s.state.as_str(), "downloading" | "installing" | "restarting") {
            drop(s);
            return status();
        }
        let Some(m) = s.latest.clone() else {
            return (409, json!({"ok": false, "error": "check for updates first"}));
        };
        if m.build <= build() {
            return (409, json!({"ok": false, "error": "already up to date"}));
        }
        s.state = "downloading".into();
        s.error.clear();
        s.done = 0;
        s.total = 0;
        m
    };
    crate::system::spawn_named("update-apply", move || {
        if let Err(e) = run(&manifest) {
            eprintln!("[update] failed: {e}");
            let mut s = STATE.lock().unwrap();
            s.state = "failed".into();
            s.error = e;
        }
    });
    status()
}

fn set_progress(done: u64, total: u64) {
    let mut s = STATE.lock().unwrap();
    s.done = done;
    s.total = total;
}

fn download(f: &FileEntry, to: &Path, base: u64, total: u64) -> Result<(), String> {
    let mut r = http().get(&f.url).call().map_err(|e| format!("{}: {e}", f.name))?;
    let mut reader = r.body_mut().as_reader().take(f.size + 1);
    let mut out = std::fs::File::create(to).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n_total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("{}: {e}", f.name))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        n_total += n as u64;
        set_progress(base + n_total, total);
    }
    out.sync_all().map_err(|e| e.to_string())?;
    if n_total != f.size || hex::encode(hasher.finalize()) != f.sha256 {
        return Err(format!("{} did not download correctly", f.name));
    }
    Ok(())
}

fn run(m: &Manifest) -> Result<(), String> {
    let files = files_for(m)?;
    let staging = PathBuf::from(DIR).join("staging");
    let previous = PathBuf::from(DIR).join("previous");
    // Root-only: rollback.sh in here runs as root.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(DIR).map_err(|e| e.to_string())?;
        let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut base = 0;
    for f in &files {
        download(f, &staging.join(&f.name), base, total)?;
        base += f.size;
    }

    STATE.lock().unwrap().state = "installing".into();
    // Keep what runs now, and a script that puts it back.
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::create_dir_all(&previous).map_err(|e| e.to_string())?;
    let mut restore = String::from("#!/bin/sh\n# Puts back the router software that ran before the last update.\n");
    for f in &files {
        let dest = target(&f.name).unwrap();
        if Path::new(dest).exists() {
            std::fs::copy(dest, previous.join(&f.name)).map_err(|e| format!("backup {}: {e}", f.name))?;
            restore.push_str(&format!("cp {p}/{n} {d}.restore && mv {d}.restore {d}\n", p = previous.display(), n = f.name, d = dest));
        }
    }
    restore.push_str(&restore_tail(m.build));
    std::fs::write(format!("{DIR}/rollback.sh"), restore).map_err(|e| e.to_string())?;

    // Move the new files in (same filesystem, so each swap is atomic).
    let screen_changed = files.iter().any(|f| f.name == "openu60-screen");
    for f in &files {
        let dest = target(&f.name).unwrap();
        let tmp = format!("{dest}.new");
        if let Some(parent) = Path::new(dest).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(staging.join(&f.name), &tmp).map_err(|e| format!("{}: {e}", f.name))?;
        let mode = if f.name.ends_with(".sh") { 0o700 } else { 0o755 };
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        std::fs::rename(&tmp, dest).map_err(|e| format!("{}: {e}", f.name))?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::write(format!("{DIR}/pending"), m.build.to_string()).map_err(|e| e.to_string())?;

    if screen_changed {
        crate::screen_watch::restart_for_update();
    }

    // Watchdog, detached from the agent: undo the update unless the new agent confirms itself.
    // Only for this update (a later update writes its own build into `pending`): every 10 s,
    // stop once the new agent has confirmed itself; roll back when no agent has been running
    // for 30 s (a crash loop) or after 150 s without confirmation.
    let watchdog = watchdog_script(m.build);
    std::fs::write(format!("{DIR}/watchdog.sh"), watchdog).map_err(|e| e.to_string())?;
    let _ = std::process::Command::new("sh").arg("-c").arg(format!("(sh {DIR}/watchdog.sh) >/dev/null 2>&1 &")).status();

    eprintln!("[update] installed build {}; restarting", m.build);
    STATE.lock().unwrap().state = "restarting".into();
    std::thread::sleep(Duration::from_secs(2));
    // The supervisor starts the new binary a few seconds after this exit.
    std::process::exit(0);
}

/// End of rollback.sh: record the failure and restart the supervisor at once.
fn restore_tail(b: u32) -> String {
    format!(
        "echo '{{\"build\":{b},\"ok\":false,\"at\":'$(date +%s)',\"message\":\"rolled back: the new version did not start\"}}' > {DIR}/result.json\n\
         rm -f {DIR}/pending\n\
         # Restart the supervisor at once: after repeated crashes it waits 60 s between tries.\n\
         touch /tmp/zte-agent.stop\n\
         for d in /proc/[0-9]*; do [ \"$(tr '\\0' '\\n' < $d/cmdline 2>/dev/null | sed -n 2p)\" = /data/local/tmp/zte-agent-run.sh ] && kill ${{d#/proc/}} 2>/dev/null; done\n\
         killall zte-agent 2>/dev/null\nsleep 2\nrm -f /tmp/zte-agent.stop\nsh /data/local/tmp/start_zte_agent.sh\n"
    )
}

fn watchdog_script(b: u32) -> String {
    format!(
        "#!/bin/sh\nmine() {{ [ \"$(cat {DIR}/pending 2>/dev/null)\" = \"{b}\" ]; }}\n\
         down=0\nfor i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do\n  sleep 10\n  mine || exit 0\n\
           if pidof zte-agent >/dev/null; then down=0; else down=$((down + 1)); fi\n\
           [ $down -ge 3 ] && break\ndone\nmine && sh {DIR}/rollback.sh\n"
    )
}

/// On startup: look for updates now and then (so the app can show a badge without asking
/// GitHub itself), and confirm a pending update once this build has run for a minute.
pub fn start() {
    if supported() {
        crate::system::spawn_named("update-check", || {
            std::thread::sleep(Duration::from_secs(300));
            loop {
                check();
                std::thread::sleep(Duration::from_secs(12 * 3600));
            }
        });
    }
    let Some(pending) = std::fs::read_to_string(format!("{DIR}/pending")).ok().and_then(|s| s.trim().parse::<u32>().ok()) else {
        return;
    };
    if pending != build() {
        return; // an older agent is running; the watchdog decides
    }
    crate::system::spawn_named("update-confirm", move || {
        std::thread::sleep(Duration::from_secs(60));
        let r = LastResult { build: pending, ok: true, at: now(), message: String::new() };
        if let Ok(text) = serde_json::to_vec(&r) {
            let _ = std::fs::write(format!("{DIR}/result.json"), text);
        }
        let _ = std::fs::remove_file(format!("{DIR}/pending"));
        eprintln!("[update] build {pending} confirmed");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsigned_or_tampered_manifests() {
        assert!(verify(b"{}", b"not base64!").is_err());
        let zero_sig = base64::engine::general_purpose::STANDARD.encode([0u8; 64]);
        assert!(verify(b"{}", zero_sig.as_bytes()).is_err());
    }

    #[test]
    fn generated_scripts_are_valid_shell() {
        for (name, text) in [("tail", format!("#!/bin/sh\n{}", restore_tail(7))), ("watchdog", watchdog_script(7))] {
            let path = std::env::temp_dir().join(format!("openu60-{name}-{}.sh", std::process::id()));
            std::fs::write(&path, text).unwrap();
            let ok = std::process::Command::new("sh").arg("-n").arg(&path).status().unwrap().success();
            let _ = std::fs::remove_file(&path);
            assert!(ok, "{name} script has a syntax error");
        }
    }

    #[test]
    fn only_known_files_are_accepted() {
        assert_eq!(target("zte-agent"), Some("/data/zte-agent"));
        assert!(target("../etc/passwd").is_none());
        assert!(target("rc.local").is_none());
    }
}
