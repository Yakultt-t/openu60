//! Screen settings, stored in /data/openu60/screen/settings.json. The phone app edits the
//! same file through zte-agent, so the UI reloads it when it changes.

use serde::{Deserialize, Serialize};

pub const HOME: &str = "/data/openu60/screen";
pub const TIMEOUTS: [u32; 4] = [30, 60, 120, 300];
pub const THEMES: [(&str, &str); 4] = [("earth", "地球"), ("space", "深空"), ("aurora", "极光"), ("custom", "自定义")];

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub brightness: u8,
    pub timeout_secs: u32,
    pub theme: String,
    /// After waking, show the cover (true) or the page that was open.
    pub wake_cover: bool,
    pub battery_pct: bool,
    /// Speeds in bytes per second (MB/s) instead of bits (Mbps).
    pub speed_bytes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            brightness: 180,
            timeout_secs: 120,
            theme: "earth".into(),
            wake_cover: true,
            battery_pct: false,
            speed_bytes: false,
        }
    }
}

pub fn path() -> String {
    format!("{HOME}/settings.json")
}

pub fn load() -> Settings {
    let mut s: Settings = std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    s.brightness = s.brightness.clamp(10, 255);
    if !TIMEOUTS.contains(&s.timeout_secs) {
        s.timeout_secs = 120;
    }
    if !THEMES.iter().any(|(k, _)| *k == s.theme) {
        s.theme = "earth".into();
    }
    s
}

pub fn save(s: &Settings) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::create_dir_all(HOME);
    let tmp = format!("{HOME}/settings.json.tmp");
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp) {
        if f.write_all(serde_json::to_string_pretty(s).unwrap_or_default().as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}

/// Modification time of a file, to notice edits made by the app.
pub fn mtime(p: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}
