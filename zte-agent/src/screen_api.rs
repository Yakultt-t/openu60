//! Lets the phone app manage the router's touchscreen UI: its settings file (the screen
//! reloads it within two seconds), a custom wallpaper, and opening the UI remotely.

use base64::Engine;
use serde_json::{json, Map, Value};

const HOME: &str = "/data/openu60/screen";
const SETTINGS: &str = "/data/openu60/screen/settings.json";
const WALLPAPER: &str = "/data/openu60/screen/wallpaper.png";
const OPEN_REQUEST: &str = "/tmp/openu60-screen-open";
const BIN: &str = "/data/openu60/screen/openu60-screen";
const THEMES: [&str; 4] = ["earth", "space", "aurora", "custom"];

fn read() -> Map<String, Value> {
    std::fs::read_to_string(SETTINGS).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn write(m: &Map<String, Value>) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(HOME).map_err(|e| e.to_string())?;
    let tmp = format!("{SETTINGS}.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(Value::Object(m.clone()).to_string().as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, SETTINGS).map_err(|e| e.to_string())
}

fn running() -> bool {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        std::fs::read(e.path().join("cmdline")).map(|c| c.starts_with(BIN.as_bytes()) && c.ends_with(b"run\0")).unwrap_or(false)
    })
}

/// Settings as the app sees them: the PIN itself never leaves the router.
fn public(m: &Map<String, Value>) -> Value {
    let mut out = m.clone();
    let has_pin = out.remove("pin").and_then(|p| p.as_str().map(|s| !s.is_empty())).unwrap_or(false);
    out.insert("pin_set".into(), json!(has_pin));
    out.insert("installed".into(), json!(std::path::Path::new(BIN).exists()));
    out.insert("running".into(), json!(running()));
    out.insert("custom_wallpaper".into(), json!(std::path::Path::new(WALLPAPER).exists()));
    Value::Object(out)
}

pub fn get() -> (u16, Value) {
    (200, json!({"ok": true, "data": public(&read())}))
}

/// PUT /api/screen/settings — any of brightness, timeout_secs, theme, wake_cover,
/// battery_pct, speed_bytes, pin ("" clears the lock, or four digits).
pub fn put(body: &[u8]) -> (u16, Value) {
    let patch: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
    };
    let mut m = read();
    if let Some(b) = patch["brightness"].as_u64() {
        m.insert("brightness".into(), json!(b.clamp(10, 255)));
    }
    if let Some(t) = patch["timeout_secs"].as_u64() {
        if [30, 60, 120, 300].contains(&t) {
            m.insert("timeout_secs".into(), json!(t));
        }
    }
    if let Some(t) = patch["theme"].as_str() {
        if !THEMES.contains(&t) {
            return (400, json!({"ok": false, "error": "unknown theme"}));
        }
        m.insert("theme".into(), json!(t));
    }
    for key in ["wake_cover", "battery_pct", "speed_bytes"] {
        if let Some(v) = patch[key].as_bool() {
            m.insert(key.into(), json!(v));
        }
    }
    if let Some(p) = patch["pin"].as_str() {
        if !(p.is_empty() || (p.len() == 4 && p.bytes().all(|b| b.is_ascii_digit()))) {
            return (400, json!({"ok": false, "error": "PIN must be four digits"}));
        }
        m.insert("pin".into(), json!(p));
    }
    match write(&m) {
        Ok(()) => (200, json!({"ok": true, "data": public(&m)})),
        Err(e) => (500, json!({"ok": false, "error": e})),
    }
}

/// PUT /api/screen/wallpaper — {"png_base64": "..."}: a 320x480 PNG; switches to it.
pub fn put_wallpaper(body: &[u8]) -> (u16, Value) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let Some(b64) = v["png_base64"].as_str() else {
        return (400, json!({"ok": false, "error": "missing png_base64"}));
    };
    let Ok(png) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return (400, json!({"ok": false, "error": "invalid base64"}));
    };
    // PNG signature, IHDR size 320x480.
    let ok = png.len() > 24 && png.len() < 3 * 1024 * 1024 && png.starts_with(b"\x89PNG\r\n\x1a\n")
        && u32::from_be_bytes([png[16], png[17], png[18], png[19]]) == 320
        && u32::from_be_bytes([png[20], png[21], png[22], png[23]]) == 480;
    if !ok {
        return (400, json!({"ok": false, "error": "need a 320x480 PNG under 3 MB"}));
    }
    let _ = std::fs::create_dir_all(HOME);
    let tmp = format!("{WALLPAPER}.tmp");
    if let Err(e) = std::fs::write(&tmp, &png).and_then(|_| std::fs::rename(&tmp, WALLPAPER)) {
        return (500, json!({"ok": false, "error": e.to_string()}));
    }
    let mut m = read();
    m.insert("theme".into(), json!("custom"));
    let _ = write(&m);
    (200, json!({"ok": true, "data": public(&m)}))
}

pub fn delete_wallpaper() -> (u16, Value) {
    let _ = std::fs::remove_file(WALLPAPER);
    let mut m = read();
    if m.get("theme").and_then(|t| t.as_str()) == Some("custom") {
        m.insert("theme".into(), json!("earth"));
        let _ = write(&m);
    }
    (200, json!({"ok": true, "data": public(&m)}))
}

/// POST /api/screen/open — the watcher switches the screen to the OpenU60 UI.
pub fn open() -> (u16, Value) {
    if !std::path::Path::new(BIN).exists() {
        return (409, json!({"ok": false, "error": "screen UI not installed"}));
    }
    match std::fs::write(OPEN_REQUEST, b"1") {
        Ok(()) => (200, json!({"ok": true})),
        Err(e) => (500, json!({"ok": false, "error": e.to_string()})),
    }
}
