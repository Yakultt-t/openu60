use serde_json::{json, Value};

use crate::handlers::AppState;
use crate::ubus;

pub fn cell_lock_nr(state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    crate::modem_ext::note_user_modem_change(state);
    match ubus::call("zte_nwinfo_api", "nwinfo_lock_nr_cell", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_lock_lte(state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    crate::modem_ext::note_user_modem_change(state);
    match ubus::call("zte_nwinfo_api", "nwinfo_lock_lte_cell", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_lock_reset(state: &AppState) -> (u16, Value) {
    crate::modem_ext::note_user_modem_change(state);
    match ubus::call("zte_nwinfo_api", "nwinfo_reset_band_cell_setting", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_neighbors_scan(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_scan_nbr", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_neighbors_nr(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_get_nr5g_nbr_contents", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_neighbors_lte(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_get_lte_nbr_contents", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_band_nr(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    match ubus::call("zte_nwinfo_api", "nwinfo_set_nrbandlock", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_band_lte(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    match ubus::call("zte_nwinfo_api", "nwinfo_set_gwl_bandlock", Some(&parsed.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_band_reset(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_rest_band_rat", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

/// This firmware has no "get" for Smart Tower Connect; its settings and state live in UCI.
fn stc_uci(section: &str, key: &str) -> String {
    ubus::uci_get(&format!("zte_nwinfo.{section}.{key}")).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn cell_stc_params_get(_state: &AppState) -> (u16, Value) {
    (200, json!({"ok": true, "data": {
        "lte_collect_timer": stc_uci("stc_cell_lock_config", "collect_lte_cell_white_list_timer_max"),
        "nrsa_collect_timer": stc_uci("stc_cell_lock_config", "collect_nr5g_cell_white_list_timer_max"),
        "lte_whitelist_max": stc_uci("stc_cell_lock_config", "collect_lte_cell_white_list_num_max"),
        "nrsa_whitelist_max": stc_uci("stc_cell_lock_config", "collect_nr5g_cell_white_list_num_max"),
        "stc_enable": stc_uci("stc_cell_lock_status", "cell_white_list_enable_flag"),
    }}))
}

pub fn cell_stc_params_set(_state: &AppState, body: &[u8]) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    let pick = |app: &str, fw: &str| -> String {
        parsed.get(app).or_else(|| parsed.get(fw)).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }).unwrap_or_default()
    };
    let args = json!({
        "stc_lte_collect_timer": pick("lte_collect_timer", "stc_lte_collect_timer"),
        "stc_nrsa_collect_timer": pick("nrsa_collect_timer", "stc_nrsa_collect_timer"),
        "stc_lte_white_list_num_max": pick("lte_whitelist_max", "stc_lte_white_list_num_max"),
        "stc_nrsa_white_list_num_max": pick("nrsa_whitelist_max", "stc_nrsa_white_list_num_max"),
    });
    match ubus::call("zte_nwinfo_api", "nwinfo_set_stc_white_list_par", Some(&args.to_string())) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_stc_status(_state: &AppState) -> (u16, Value) {
    let k = |key: &str| stc_uci("stc_cell_lock_status", key);
    (200, json!({"ok": true, "data": {
        "stc_enable": k("cell_white_list_enable_flag"),
        "status": k("cell_white_list_enable_flag"),
        "available": k("cell_available_flag"),
        "lte_whitelist_count": k("current_lte_cell_white_list_num"),
        "nr_whitelist_count": k("current_nr5g_cell_white_list_num"),
        "lte_timer": k("current_lte_cell_white_list_timer"),
        "nr_timer": k("current_nr5g_cell_white_list_timer"),
        "run_time": k("collect_cell_white_list_run_time"),
    }}))
}

pub fn cell_stc_enable(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_stc_cell_lock_enable", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_stc_disable(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_stc_cell_lock_disable", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_stc_reset(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_stc_cell_lock_reset", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_signal_detect_start(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_start_detect_signal_quality", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_signal_detect_stop(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_end_detect_signal_quality", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_signal_detect_results(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_get_detect_quality_recorder", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}

pub fn cell_signal_detect_progress(_state: &AppState) -> (u16, Value) {
    match ubus::call("zte_nwinfo_api", "nwinfo_get_progress_and_quality", Some("{}")) {
        Ok(data) => (200, json!({"ok": true, "data": data})),
        Err(e) => (503, json!({"ok": false, "error": e})),
    }
}
