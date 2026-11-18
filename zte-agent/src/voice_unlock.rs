//! Voice calls on China-market U60 Pro units (experimental, off by default).
//!
//! Those units ship "SMS only": two modem settings keep calls off, and both must be right for
//! IMS to bring voice up after a reboot (reported by users, 2026-11):
//! - EFS item /nv/item_files/modem/mmode/sms_only: 01 locked, 00 normal;
//! - NV 5280 (NV_DISABLE_CM_CALL_TYPE_I): first byte bit 0 set when locked (01 02 00 00 …),
//!   clear when unlocked (00 02 00 00 …). Clearing it alone gets undone at the next boot
//!   while sms_only is still 01.
//! A carrier profile (MBN) load after a SIM change often writes the locked values back.
//!
//! With the switch on, the agent checks after boot and after a SIM change and fixes only the
//! exact locked values above, reads them back, and reboots once so IMS picks them up. If the
//! same SIM comes back locked after that, it stops (no write, no reboot) until the SIM
//! changes. SIMs are told apart by a salted hash; the ICCID itself is never stored or logged.
//! Units that are not locked (overseas firmware) are never written.
//!
//! GET/PUT /api/modem/voice-unlock. Settings and history: /data/openu60/voice_unlock.json.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::diag;
use crate::handlers::AppState;
use crate::ubus;

const FILE: &str = "/data/openu60/voice_unlock.json";
/// One JSON line per check, for testers to send in. Rotated at 256 KB (one old file kept).
pub const LOG: &str = "/data/openu60/voice_unlock.log";
const LOG_MAX: u64 = 256 * 1024;
/// While the switch is on, re-read every 30 min to catch values that revert without a reboot.
const OBSERVE_EVERY_TICKS: u64 = 360;
pub const NV_ITEM: u16 = 5280;
pub const SMS_ONLY_PATH: &str = "/nv/item_files/modem/mmode/sms_only";
/// The modem loads its carrier profile after the SIM is ready; let that finish first.
const SETTLE: Duration = Duration::from_secs(90);

#[derive(Clone, Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    /// SIM hash and boot id of the last time we wrote the fix.
    #[serde(default)]
    pub fixed_sim: String,
    #[serde(default)]
    pub fixed_boot: String,
    #[serde(default)]
    pub fixed_at: u64,
    /// SIM whose fix did not survive a reboot: left alone until the SIM changes.
    #[serde(default)]
    pub suppressed_sim: String,
    #[serde(default)]
    pub last_result: String,
    #[serde(default)]
    pub last_check: u64,
}

/// What the modem holds right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// None when the EFS item does not exist.
    pub sms_only: Option<u8>,
    pub nv: Vec<u8>,
    pub nv_status: u16,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Calls are allowed by both settings.
    Unlocked,
    /// The known China-market locked values: safe to fix.
    Locked { fix_sms_only: bool, fix_nv: bool },
    /// Something else (other firmware, unexpected bytes): never written.
    Unknown(String),
}

pub fn judge(r: &Reading) -> Verdict {
    let nv_locked = r.nv_status == 0 && r.nv.first() == Some(&0x01);
    let nv_ok = r.nv_status == 5 || (r.nv_status == 0 && r.nv.first().is_some_and(|b| b & 1 == 0));
    let sms_locked = r.sms_only == Some(0x01);
    let sms_ok = matches!(r.sms_only, None | Some(0x00));
    match (sms_locked || sms_ok, nv_locked || nv_ok) {
        (true, true) if sms_ok && nv_ok => Verdict::Unlocked,
        (true, true) => Verdict::Locked { fix_sms_only: sms_locked, fix_nv: nv_locked },
        _ => Verdict::Unknown(format!(
            "sms_only={:?} nv5280 status {} first byte {:02x?}",
            r.sms_only,
            r.nv_status,
            r.nv.first()
        )),
    }
}

/// What to do on a check, given the verdict and history.
#[derive(Debug, PartialEq)]
pub enum Action {
    Nothing(&'static str),
    Fix,
    /// Same SIM, fixed on an earlier boot, locked again: give up on this SIM.
    Suppress,
}

pub fn decide(v: &Verdict, s: &Settings, sim: &str, boot: &str) -> Action {
    if !s.enabled {
        return Action::Nothing("off");
    }
    match v {
        Verdict::Unlocked => Action::Nothing("unlocked"),
        Verdict::Unknown(_) => Action::Nothing("unknown values, not touched"),
        Verdict::Locked { .. } if sim.is_empty() => Action::Nothing("no SIM"),
        Verdict::Locked { .. } if s.suppressed_sim == sim => Action::Nothing("suppressed for this SIM"),
        Verdict::Locked { .. } if s.fixed_sim == sim && s.fixed_boot == boot => {
            Action::Nothing("already fixed this boot")
        }
        Verdict::Locked { .. } if s.fixed_sim == sim => Action::Suppress,
        Verdict::Locked { .. } => Action::Fix,
    }
}

pub struct VoiceUnlock {
    settings: Mutex<Settings>,
    busy: AtomicBool,
    /// Set by the API when the switch is turned on; the background thread checks at once.
    check_now: AtomicBool,
}

impl Default for VoiceUnlock {
    fn default() -> Self {
        let settings = std::fs::read(FILE)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        VoiceUnlock { settings: Mutex::new(settings), busy: AtomicBool::new(false), check_now: AtomicBool::new(false) }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Salted hash of the ICCID; empty when no SIM is ready.
fn sim_hash() -> String {
    let Ok(info) = ubus::call("zwrt_zte_mdm.api", "get_sim_info", Some("{}")) else { return String::new() };
    if info["sim_states"].as_str() != Some("sim ready") {
        return String::new();
    }
    let iccid = info["sim_iccid"].as_str().unwrap_or("").trim();
    if iccid.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(format!("openu60-voice-unlock:{iccid}").as_bytes());
    hex::encode(&digest[..8])
}

fn firmware_version() -> String {
    std::process::Command::new("uci")
        .args(["-q", "get", "zwrt_common_info.common_config.wa_inner_version"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Appends one event to the test log with what the modem and network looked like.
fn log_event(kind: &str, reading: Option<&Result<Reading, String>>, result: &str) {
    let net = ubus::call("zte_nwinfo_api", "nwinfo_get_netinfo", Some("{}")).unwrap_or(Value::Null);
    let modem = match reading {
        Some(Ok(r)) => json!({
            "sms_only": r.sms_only,
            "nv5280_status": r.nv_status,
            "nv5280_head": hex::encode(&r.nv[..8.min(r.nv.len())]),
            "verdict": format!("{:?}", judge(r)),
        }),
        Some(Err(e)) => json!({"error": e}),
        None => Value::Null,
    };
    let line = json!({
        "t": now(),
        "kind": kind,
        "boot": boot_id().chars().take(8).collect::<String>(),
        "sim": sim_hash(),
        "fw": firmware_version(),
        "network_type": net["network_type"],
        "provider": net["network_provider"],
        "ims_registered": crate::ims_guard::ims_registered(),
        "modem": modem,
        "result": result,
    });
    if std::fs::metadata(LOG).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(LOG, format!("{LOG}.1"));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn read_modem() -> Result<Reading, String> {
    let mut s = diag::Session::open()?;
    read_with(&mut s)
}

fn read_with(s: &mut diag::Session) -> Result<Reading, String> {
    let sms_only = match s.efs_get(SMS_ONLY_PATH, 1) {
        Ok(v) => v.first().copied(),
        Err(e) if e == "ENOENT" => None,
        Err(e) => return Err(e),
    };
    let (nv_status, nv) = s.nv_read(NV_ITEM)?;
    Ok(Reading { sms_only, nv, nv_status })
}

/// Writes the fixes the verdict asks for and reads both back.
fn apply_fix(fix_sms_only: bool, fix_nv: bool) -> Result<Reading, String> {
    let mut s = diag::Session::open()?;
    let before = read_with(&mut s)?;
    // Re-judge on the fresh reading: never write on a stale verdict.
    if judge(&before) != (Verdict::Locked { fix_sms_only, fix_nv }) {
        return Err("values changed before writing".into());
    }
    if fix_sms_only {
        s.efs_put(SMS_ONLY_PATH, &[0x00])?;
    }
    if fix_nv {
        let mut nv = before.nv.clone();
        nv[0] &= !1;
        s.nv_write(NV_ITEM, &nv)?;
    }
    let after = read_with(&mut s)?;
    if judge(&after) != Verdict::Unlocked {
        return Err(format!("read back still locked: {:?}", judge(&after)));
    }
    Ok(after)
}

impl VoiceUnlock {
    fn save(&self, s: &Settings) {
        let _ = std::fs::create_dir_all("/data/openu60");
        if let Ok(text) = serde_json::to_vec_pretty(s) {
            let tmp = format!("{FILE}.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, FILE);
            }
        }
    }

    fn update(&self, f: impl FnOnce(&mut Settings)) -> Settings {
        let mut s = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
        self.save(&s);
        s.clone()
    }

    /// One check: read, decide, maybe fix and reboot. Returns a short result for the log/API.
    pub fn check(&self, state: &AppState) -> String {
        if self.busy.swap(true, Ordering::SeqCst) {
            return "a check is already running".into();
        }
        let (result, reading, reboot) = self.check_inner(state);
        eprintln!("[voice-unlock] {result}");
        log_event("check", reading.as_ref(), &result);
        self.update(|s| {
            s.last_result = result.clone();
            s.last_check = now();
        });
        self.busy.store(false, Ordering::SeqCst);
        if reboot {
            // Saved and logged first: the reboot ends this process.
            std::thread::sleep(Duration::from_secs(3));
            let _ = ubus::call("system", "reboot", Some("{}"));
        }
        result
    }

    /// Read-only: logs what the modem holds now, without acting on it.
    pub fn observe(&self, kind: &str) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let reading = read_modem();
        log_event(kind, Some(&reading), "observed");
        self.busy.store(false, Ordering::SeqCst);
    }

    fn check_inner(&self, state: &AppState) -> (String, Option<Result<Reading, String>>, bool) {
        let settings = self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !settings.enabled {
            return ("off".into(), None, false);
        }
        let reading = match read_modem() {
            Ok(r) => r,
            Err(e) => return (format!("could not read the modem: {e}"), Some(Err(e)), false),
        };
        let verdict = judge(&reading);
        let (sim, boot) = (sim_hash(), boot_id());
        let action = decide(&verdict, &settings, &sim, &boot);
        let seen = Some(Ok(reading));
        match action {
            Action::Nothing(why) => (why.to_string(), seen, false),
            Action::Suppress => {
                self.update(|s| s.suppressed_sim = sim.clone());
                ("locked again after the fix on this SIM; leaving it alone until the SIM changes".into(), seen, false)
            }
            Action::Fix => {
                let Verdict::Locked { fix_sms_only, fix_nv } = verdict else { unreachable!() };
                if state.call_active.load(Ordering::Relaxed) {
                    return ("locked; waiting for the call to end".into(), seen, false);
                }
                match apply_fix(fix_sms_only, fix_nv) {
                    Ok(after) => {
                        self.update(|s| {
                            s.fixed_sim = sim.clone();
                            s.fixed_boot = boot.clone();
                            s.fixed_at = now();
                        });
                        let what = match (fix_sms_only, fix_nv) {
                            (true, true) => "sms_only and NV 5280",
                            (true, false) => "sms_only",
                            _ => "NV 5280",
                        };
                        (format!("fixed {what}, read back OK; rebooting so IMS reloads"), Some(Ok(after)), true)
                    }
                    Err(e) => (format!("fix failed, nothing rebooted: {e}"), seen, false),
                }
            }
        }
    }
}

/// Background: one check after boot settles, then whenever the SIM changes or the switch is
/// turned on.
pub fn start(state: Arc<AppState>) {
    crate::system::spawn_named("voice-unlock", move || {
        let vu = &state.voice_unlock;
        // Wait out boot, but answer a turn-on right away.
        for _ in 0..SETTLE.as_secs() / 5 {
            if vu.check_now.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        let mut last_sim = String::new();
        let mut tick = 0u64;
        loop {
            let now_requested = vu.check_now.swap(false, Ordering::SeqCst);
            if now_requested || tick % 12 == 0 {
                let enabled = vu.settings.lock().map(|s| s.enabled).unwrap_or(false);
                let sim = if enabled { sim_hash() } else { String::new() };
                if enabled && !sim.is_empty() && (now_requested || sim != last_sim) {
                    if !now_requested && !last_sim.is_empty() {
                        // New SIM: give the carrier profile time to load (and possibly re-lock).
                        std::thread::sleep(SETTLE);
                    }
                    last_sim = sim;
                    vu.check(&state);
                }
            }
            if tick > 0 && tick % OBSERVE_EVERY_TICKS == 0 && vu.settings.lock().map(|s| s.enabled).unwrap_or(false) {
                vu.observe("periodic");
            }
            tick += 1;
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn status_json(s: &Settings, reading: Option<Result<Reading, String>>) -> Value {
    let modem = match reading {
        Some(Ok(r)) => {
            let verdict = judge(&r);
            json!({
                "sms_only": r.sms_only,
                "nv5280_status": r.nv_status,
                "nv5280_head": hex::encode(&r.nv[..4.min(r.nv.len())]),
                "state": match verdict {
                    Verdict::Unlocked => "unlocked",
                    Verdict::Locked { .. } => "locked",
                    Verdict::Unknown(_) => "unknown",
                },
                "detail": match verdict { Verdict::Unknown(d) => d, _ => String::new() },
            })
        }
        Some(Err(e)) => json!({"state": "error", "detail": e}),
        None => Value::Null,
    };
    json!({
        "enabled": s.enabled,
        "modem": modem,
        "last_result": s.last_result,
        "last_check": s.last_check,
        "fixed_at": s.fixed_at,
        "suppressed": !s.suppressed_sim.is_empty(),
    })
}

/// GET /api/modem/voice-unlock: settings plus a live read of both values.
pub fn get(state: &AppState) -> (u16, Value) {
    let s = state.voice_unlock.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let reading = read_modem();
    log_event("status", Some(&reading), "read for the app");
    let mut v = status_json(&s, Some(reading));
    v["firmware"] = json!(firmware_version());
    (200, json!({"ok": true, "data": v}))
}

/// GET /api/modem/voice-unlock/log: the test log (older file first), plus current settings.
pub fn log(state: &AppState) -> (u16, Value) {
    let s = state.voice_unlock.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut text = std::fs::read_to_string(format!("{LOG}.1")).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(LOG).unwrap_or_default());
    (200, json!({"ok": true, "data": {"settings": s, "firmware": firmware_version(), "log": text}}))
}

/// PUT /api/modem/voice-unlock {"enabled": bool}. Turning it on checks right away in the
/// background thread (a fix reboots the router).
pub fn set(state: &AppState, body: &[u8]) -> (u16, Value) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (400, json!({"ok": false, "error": "invalid JSON"}));
    };
    let Some(enabled) = v["enabled"].as_bool() else {
        return (400, json!({"ok": false, "error": "enabled must be true or false"}));
    };
    let s = state.voice_unlock.update(|s| {
        s.enabled = enabled;
        if enabled {
            // A deliberate turn-on gives a suppressed SIM another chance.
            s.suppressed_sim.clear();
        }
    });
    if enabled {
        state.voice_unlock.check_now.store(true, Ordering::SeqCst);
    }
    (200, json!({"ok": true, "data": status_json(&s, None)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(sms: Option<u8>, nv0: u8) -> Reading {
        let mut nv = vec![0u8; 128];
        nv[0] = nv0;
        nv[1] = 0x02;
        Reading { sms_only: sms, nv, nv_status: 0 }
    }

    #[test]
    fn overseas_values_are_unlocked() {
        // Read from an overseas unit on 2026-11-17: sms_only 00, NV 5280 all zero.
        let r = Reading { sms_only: Some(0), nv: vec![0; 128], nv_status: 0 };
        assert_eq!(judge(&r), Verdict::Unlocked);
    }

    #[test]
    fn china_locked_values_are_fixable() {
        assert_eq!(judge(&reading(Some(1), 0x01)), Verdict::Locked { fix_sms_only: true, fix_nv: true });
        assert_eq!(judge(&reading(Some(0), 0x01)), Verdict::Locked { fix_sms_only: false, fix_nv: true });
        assert_eq!(judge(&reading(Some(1), 0x00)), Verdict::Locked { fix_sms_only: true, fix_nv: false });
    }

    #[test]
    fn unexpected_values_are_never_touched() {
        assert!(matches!(judge(&reading(Some(2), 0x00)), Verdict::Unknown(_)));
        assert!(matches!(judge(&reading(Some(0), 0x03)), Verdict::Unknown(_)));
        let mut r = reading(Some(1), 0x01);
        r.nv_status = 6;
        assert!(matches!(judge(&r), Verdict::Unknown(_)));
    }

    #[test]
    fn fixes_once_per_sim_and_stops_if_it_does_not_stick() {
        let locked = Verdict::Locked { fix_sms_only: true, fix_nv: true };
        let mut s = Settings { enabled: true, ..Default::default() };
        assert_eq!(decide(&locked, &s, "simA", "boot1"), Action::Fix);
        s.fixed_sim = "simA".into();
        s.fixed_boot = "boot1".into();
        assert_eq!(decide(&locked, &s, "simA", "boot1"), Action::Nothing("already fixed this boot"));
        assert_eq!(decide(&locked, &s, "simA", "boot2"), Action::Suppress);
        s.suppressed_sim = "simA".into();
        assert_eq!(decide(&locked, &s, "simA", "boot3"), Action::Nothing("suppressed for this SIM"));
        // A different SIM gets one more try.
        assert_eq!(decide(&locked, &s, "simB", "boot3"), Action::Fix);
    }

    #[test]
    fn off_or_no_sim_does_nothing() {
        let locked = Verdict::Locked { fix_sms_only: true, fix_nv: true };
        let off = Settings::default();
        assert_eq!(decide(&locked, &off, "simA", "b"), Action::Nothing("off"));
        let on = Settings { enabled: true, ..Default::default() };
        assert_eq!(decide(&locked, &on, "", "b"), Action::Nothing("no SIM"));
        assert_eq!(decide(&Verdict::Unlocked, &on, "simA", "b"), Action::Nothing("unlocked"));
    }
}
