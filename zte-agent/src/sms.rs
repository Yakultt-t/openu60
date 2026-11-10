use std::process::Command;

use serde_json::{json, Value};

use crate::handlers::AppState;
use crate::ubus;

const SMS_DB_PATH: &str = "/etc_rw/ztembb/ztesms/sms_db/sms.db";

pub fn sms_list(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    let mut data = match ubus::call("zwrt_wms", "zte_libwms_get_sms_data", Some(&parsed.to_string())) {
        Ok(data) => data,
        Err(e) => return (503, json!({"ok": false, "error": e})),
    };
    // STD firmware encrypts number/content with a session key; a browser login replaces
    // that key, so one re-key + refetch covers the stale-key case.
    if let Err(first) = crate::sms_crypto::decrypt_messages(&mut data) {
        eprintln!("[sms] decrypt failed ({first}); re-keying");
        if let Err(e) = crate::sms_crypto::rekey() {
            return (503, json!({"ok": false, "error": format!("sms key: {e}")}));
        }
        data = match ubus::call("zwrt_wms", "zte_libwms_get_sms_data", Some(&parsed.to_string())) {
            Ok(data) => data,
            Err(e) => return (503, json!({"ok": false, "error": e})),
        };
        if let Err(e) = crate::sms_crypto::decrypt_messages(&mut data) {
            return (503, json!({"ok": false, "error": format!("sms decrypt: {e}")}));
        }
    }
    (200, json!({"ok": true, "data": data}))
}

pub fn sms_capacity(_state: &AppState) -> (u16, Value) {
    match ubus::call("zwrt_wms", "zwrt_wms_get_wms_capacity", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn sms_send(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let mut parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    // The firmware rejects anything but digits and a leading "+" ("Invalid argument"); numbers
    // picked from Contacts carry spaces, dashes, brackets and invisible direction marks.
    // Several recipients are separated by ";".
    if let Some(raw) = parsed["number"].as_str() {
        let cleaned: Vec<String> = raw
            .split([';', ','])
            .map(|n| {
                let n = n.trim();
                let plus = n.starts_with('+') || n.starts_with('\u{ff0b}');
                let digits: String = n.chars().filter(|c| c.is_ascii_digit()).collect();
                if plus { format!("+{digits}") } else { digits }
            })
            .filter(|n| n.trim_start_matches('+').len() >= 3)
            .collect();
        if cleaned.is_empty() {
            return (400, json!({"ok": false, "error": "invalid phone number"}));
        }
        parsed["number"] = Value::String(cleaned.join(";"));
    }
    // Always register a fresh key right before sending so the firmware decrypts with the
    // key we encrypt with, even if a browser session changed it meanwhile.
    if let Err(e) = crate::sms_crypto::rekey() {
        return (503, json!({"ok": false, "error": format!("sms key: {e}")}));
    }
    if let Err(e) = crate::sms_crypto::encrypt_send_request(&mut parsed) {
        return (503, json!({"ok": false, "error": format!("sms encrypt: {e}")}));
    }
    match ubus::call("zwrt_wms", "zte_libwms_send_sms", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

/// Delete one or more SMS by id.
///
/// Body shape (legacy ZTE format): `{"id": "3681;3682;"}` — semicolon-joined ids with trailing `;`.
///
/// Firmware bug: `zwrt_wms_delete_sms` works for NV-stored messages but silently returns
/// `{"result": 3}` without deleting SIM-stored rows. The daemon's listing reads from
/// `/etc_rw/ztembb/ztesms/sms_db/sms.db`, so we fall back to a direct SQLite DELETE for any
/// id that survived the ubus call.
pub fn sms_delete(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };

    let ids = match parse_ids(parsed.get("id")) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => return (400, json!({"ok": false, "error": "no ids in 'id' field"})),
        Err(e) => return (400, json!({"ok": false, "error": e})),
    };

    let ubus_result = ubus::call("zwrt_wms", "zwrt_wms_delete_sms", Some(&parsed.to_string()));

    let survivors = match db_filter_existing(&ids) {
        Ok(v) => v,
        Err(e) => {
            return match ubus_result {
                Ok(data) => (200, json!({"ok": true, "data": data, "warning": format!("db check skipped: {e}")})),
                Err(ubus_err) => (503, json!({"ok": false, "error": format!("ubus: {ubus_err}; db: {e}")})),
            };
        }
    };

    if survivors.is_empty() {
        return (
            200,
            json!({"ok": true, "data": ubus_result.unwrap_or(Value::Null), "deleted_via": "ubus"}),
        );
    }

    match db_delete_ids(&survivors) {
        Ok(()) => (
            200,
            json!({"ok": true, "deleted_via": "sqlite", "ids": survivors}),
        ),
        Err(e) => (503, json!({"ok": false, "error": format!("sqlite delete failed: {e}")})),
    }
}

/// Parse the legacy ZTE id format (semicolon-joined with trailing `;`).
fn parse_ids(field: Option<&Value>) -> Result<Vec<i64>, String> {
    let raw = field.ok_or_else(|| "missing 'id' field".to_string())?;
    let s = raw.as_str().ok_or_else(|| "'id' must be a string".to_string())?;
    let mut out = Vec::new();
    for part in s.split(';') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let n: i64 = p
            .parse()
            .map_err(|_| format!("invalid id '{p}' (must be integer)"))?;
        out.push(n);
    }
    Ok(out)
}

/// Return the subset of `ids` still present in the WMS sms table.
fn db_filter_existing(ids: &[i64]) -> Result<Vec<i64>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let in_clause = ids
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM sms WHERE id IN ({in_clause});");
    let output = Command::new("/usr/bin/sqlite3")
        .args(["-cmd", ".timeout 2000", "-readonly", SMS_DB_PATH, &sql])
        .output()
        .map_err(|e| format!("spawn sqlite3: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Ok(n) = l.parse::<i64>() {
            out.push(n);
        }
    }
    Ok(out)
}

/// Direct DELETE bypassing the broken ubus path.
fn db_delete_ids(ids: &[i64]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let in_clause = ids
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("DELETE FROM sms WHERE id IN ({in_clause});");
    let output = Command::new("/usr/bin/sqlite3")
        .args(["-cmd", ".timeout 2000", SMS_DB_PATH, &sql])
        .output()
        .map_err(|e| format!("spawn sqlite3: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

pub fn sms_mark_read(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    match ubus::call("zwrt_wms", "zwrt_wms_modify_tag", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}
