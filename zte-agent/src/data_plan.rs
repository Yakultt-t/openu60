//! Monthly data plan and usage alerts.
//!
//! The firmware already keeps a cellular byte counter that resets on a configurable day of the
//! month (`zwrt_data get_wwandst`, `set_wwandst_clearday`). The plan here adds a quota, alert
//! thresholds and an optional daily limit on top of it. A thread checks every five minutes and
//! pushes an alert to the devices that want data alerts the first time a threshold is crossed
//! in a cycle. Stored in /data/openu60/data_plan.json.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::handlers::AppState;
use crate::push::Kind;
use crate::ubus;

const PLAN_FILE: &str = "/data/openu60/data_plan.json";
const CHECK_EVERY: Duration = Duration::from_secs(300);
const MB: u64 = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Plan {
    #[serde(default)]
    pub enabled: bool,
    /// Monthly allowance in MiB; 0 = unlimited (alerts off, usage still shown).
    #[serde(default)]
    pub quota_mb: u64,
    /// Day of the month the counter starts over (1–28).
    #[serde(default = "first_day")]
    pub reset_day: u8,
    /// Percent thresholds that each send one alert per cycle.
    #[serde(default = "default_alerts")]
    pub alerts: Vec<u8>,
    /// Daily limit in MiB; 0 = off.
    #[serde(default)]
    pub daily_mb: u64,
}

fn first_day() -> u8 {
    1
}
fn default_alerts() -> Vec<u8> {
    vec![80, 100]
}

impl Default for Plan {
    fn default() -> Self {
        Self { enabled: false, quota_mb: 0, reset_day: 1, alerts: default_alerts(), daily_mb: 0 }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    last_month: u64,
    #[serde(default)]
    alerted: Vec<u8>,
    #[serde(default)]
    last_day: u64,
    #[serde(default)]
    day_alerted: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    plan: Plan,
    #[serde(default)]
    ledger: Ledger,
}

pub struct DataPlan {
    saved: Mutex<Saved>,
}

fn load() -> Saved {
    std::fs::read(PLAN_FILE).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn persist(s: &Saved) {
    if let Ok(text) = serde_json::to_vec_pretty(s) {
        let tmp = format!("{PLAN_FILE}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, PLAN_FILE);
        }
    }
}

/// (cycle bytes, today bytes) from the firmware counter, which it commits to UCI.
fn usage() -> Option<(u64, u64)> {
    let get = |k: &str| -> Option<u64> {
        ubus::uci_get(&format!("zwrt_data_commit.wwancid1dst.{k}")).ok()?.trim().parse().ok()
    };
    let month = get("month_rx_bytes")? + get("month_tx_bytes")?;
    let day = get("day_rx_bytes").unwrap_or(0) + get("day_tx_bytes").unwrap_or(0);
    Some((month, day))
}

/// (year, month 1-12, day) in the router's local time.
fn today() -> (i32, u32, u32) {
    // SAFETY: plain libc time calls on a zeroed tm.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_year + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32)
    }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Days until the counter resets, counting today as day 0 of what is left.
fn days_left(reset_day: u32) -> u32 {
    let (y, m, d) = today();
    if d < reset_day {
        reset_day - d
    } else {
        days_in_month(y, m) - d + reset_day
    }
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * MB as f64);
    if gb >= 1.0 { format!("{gb:.2} GB") } else { format!("{:.0} MB", bytes as f64 / MB as f64) }
}

impl DataPlan {
    pub fn new() -> Self {
        Self { saved: Mutex::new(load()) }
    }

    pub fn start(self: &Arc<Self>, state: Arc<AppState>) {
        let me = Arc::clone(self);
        crate::system::spawn_named("data-plan", move || {
            std::thread::sleep(Duration::from_secs(60));
            loop {
                me.check(&state);
                std::thread::sleep(CHECK_EVERY);
            }
        });
    }

    fn check(&self, state: &AppState) {
        let Some((month, day)) = usage() else { return };
        let mut s = self.saved.lock().unwrap();
        let plan = s.plan.clone();
        let mut alerts: Vec<Value> = Vec::new();
        // A counter that went down means the firmware started a new cycle (or day).
        if month + MB < s.ledger.last_month {
            s.ledger.alerted.clear();
        }
        if day + MB < s.ledger.last_day {
            s.ledger.day_alerted = false;
        }
        s.ledger.last_month = month;
        s.ledger.last_day = day;
        if plan.enabled && plan.quota_mb > 0 {
            let quota = plan.quota_mb * MB;
            let percent = (month.saturating_mul(100) / quota).min(999) as u8;
            let crossed = plan.alerts.iter().copied().filter(|p| percent >= *p && !s.ledger.alerted.contains(p)).max();
            if let Some(p) = crossed {
                // One alert for the highest new threshold; the lower ones count as sent.
                for q in plan.alerts.iter().copied().filter(|q| *q <= p) {
                    if !s.ledger.alerted.contains(&q) {
                        s.ledger.alerted.push(q);
                    }
                }
                let key = if p >= 100 { "You have used all of your %@ data plan (%@ used)." } else { "You have used %@%% of your %@ data plan (%@ used)." };
                let args: Vec<String> = if p >= 100 { vec![human(quota), human(month)] } else { vec![p.to_string(), human(quota), human(month)] };
                alerts.push(json!({"aps": {
                    "alert": {"title-loc-key": "Data usage alert", "loc-key": key, "loc-args": args},
                    "sound": "default", "thread-id": "openu60-data"}, "kind": "data_alert"}));
            }
        }
        if plan.enabled && plan.daily_mb > 0 && !s.ledger.day_alerted && day >= plan.daily_mb * MB {
            s.ledger.day_alerted = true;
            alerts.push(json!({"aps": {
                "alert": {"title-loc-key": "Data usage alert", "loc-key": "Today's mobile data use is %@, over your daily limit of %@.", "loc-args": [human(day), human(plan.daily_mb * MB)]},
                "sound": "default", "thread-id": "openu60-data"}, "kind": "data_alert"}));
        }
        persist(&s);
        drop(s);
        for payload in alerts {
            eprintln!("[data-plan] alert: {}", payload["aps"]["alert"]["loc-args"]);
            if let Some(e) = state.push.alert_all(Kind::Data, &payload, Some("openu60-data")) {
                eprintln!("[data-plan] push failed: {e}");
            }
        }
    }

    /// GET /api/data-plan
    pub fn get(&self) -> (u16, Value) {
        let plan = self.saved.lock().unwrap().plan.clone();
        let (month, day) = usage().unwrap_or((0, 0));
        let quota = plan.quota_mb * MB;
        (200, json!({"ok": true, "data": {
            "plan": plan,
            "usage": {
                "cycle_bytes": month,
                "today_bytes": day,
                "quota_bytes": quota,
                "percent": if quota > 0 { (month as f64 * 100.0 / quota as f64).min(999.0) } else { 0.0 },
                "days_left": days_left(plan.reset_day as u32),
            }
        }}))
    }

    /// PUT /api/data-plan — body: a `Plan`.
    pub fn set(&self, body: &[u8]) -> (u16, Value) {
        let mut plan: Plan = match serde_json::from_slice(body) {
            Ok(p) => p,
            Err(_) => return (400, json!({"ok": false, "error": "invalid JSON"})),
        };
        if !(1..=28).contains(&plan.reset_day) {
            return (400, json!({"ok": false, "error": "reset_day must be 1-28"}));
        }
        if plan.quota_mb > 100 * 1024 * 1024 || plan.daily_mb > 100 * 1024 * 1024 {
            return (400, json!({"ok": false, "error": "limit too large"}));
        }
        plan.alerts.retain(|p| (1..=100).contains(p));
        plan.alerts.sort_unstable();
        plan.alerts.dedup();
        let arg = json!({"source_module": "web", "cid": 1, "enable": 1, "clearday": plan.reset_day});
        if let Err(e) = ubus::call("zwrt_data", "set_wwandst_clearday", Some(&arg.to_string())) {
            return (502, json!({"ok": false, "error": format!("ubus call failed: {e}")}));
        }
        {
            let mut s = self.saved.lock().unwrap();
            if s.plan.quota_mb != plan.quota_mb || s.plan.alerts != plan.alerts {
                // New allowance: thresholds already passed under the old one may alert again.
                s.ledger.alerted.clear();
            }
            if s.plan.daily_mb != plan.daily_mb {
                s.ledger.day_alerted = false;
            }
            s.plan = plan;
            persist(&s);
        }
        self.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_lengths() {
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 11), 30);
    }

    #[test]
    fn plan_defaults() {
        let p: Plan = serde_json::from_str("{}").unwrap();
        assert_eq!(p, Plan::default());
        assert_eq!(human(1536 * MB), "1.50 GB");
        assert_eq!(human(300 * MB), "300 MB");
    }
}
