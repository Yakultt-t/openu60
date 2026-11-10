//! Missed calls: an incoming call that stopped ringing without ever being answered. Fed by
//! every place that reads the modem's call list; kept (last 50) in /data/openu60/missed.json
//! so the router screen and the app can show them until someone clears the list.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};

const FILE: &str = "/data/openu60/missed.json";

struct Tracker {
    /// Call id -> (number, answered).
    ringing: BTreeMap<i64, (String, bool)>,
    list: Vec<(String, u64)>,
    loaded: bool,
}

/// Called (on its own thread) for every new missed call: device_alerts sends the notification.
static HOOK: std::sync::OnceLock<Box<dyn Fn(&str) + Send + Sync>> = std::sync::OnceLock::new();

pub fn set_hook(f: Box<dyn Fn(&str) + Send + Sync>) {
    let _ = HOOK.set(f);
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker { ringing: BTreeMap::new(), list: Vec::new(), loaded: false });

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load(t: &mut Tracker) {
    if t.loaded {
        return;
    }
    t.loaded = true;
    if let Some(v) = std::fs::read_to_string(FILE).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        t.list = v.as_array().cloned().unwrap_or_default().iter()
            .filter_map(|c| Some((c["number"].as_str()?.to_string(), c["at"].as_u64().unwrap_or(0))))
            .collect();
    }
}

fn save(t: &Tracker) {
    let body: Vec<Value> = t.list.iter().map(|(n, at)| json!({"number": n, "at": at})).collect();
    let _ = std::fs::create_dir_all("/data/openu60");
    let tmp = format!("{FILE}.tmp");
    if std::fs::write(&tmp, Value::Array(body).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, FILE);
    }
}

/// Call with the current voice call list (parsed +CLCC entries).
pub fn observe(calls: &[Value]) {
    let mut t = TRACKER.lock().unwrap();
    load(&mut t);
    let voice: Vec<&Value> = calls.iter().filter(|c| c["mode"].as_i64() == Some(0)).collect();
    for c in &voice {
        let id = c["id"].as_i64().unwrap_or(-1);
        match c["stat"].as_str() {
            Some("incoming") | Some("waiting") => {
                let number = c["number"].as_str().unwrap_or("").to_string();
                t.ringing.entry(id).or_insert((number, false));
            }
            Some("active") | Some("held") => {
                if let Some(r) = t.ringing.get_mut(&id) {
                    r.1 = true;
                }
            }
            _ => {}
        }
    }
    let present: Vec<i64> = voice.iter().map(|c| c["id"].as_i64().unwrap_or(-1)).collect();
    let gone: Vec<i64> = t.ringing.keys().filter(|id| !present.contains(id)).cloned().collect();
    let mut changed = false;
    for id in gone {
        if let Some((number, answered)) = t.ringing.remove(&id) {
            if !answered {
                if let Some(hook) = HOOK.get() {
                    let n = number.clone();
                    let hook: &'static (dyn Fn(&str) + Send + Sync) = hook.as_ref();
                    std::thread::spawn(move || hook(&n));
                }
                t.list.insert(0, (number, now()));
                t.list.truncate(50);
                changed = true;
                eprintln!("[call] missed call recorded");
            }
        }
    }
    if changed {
        save(&t);
    }
}

pub fn list() -> (u16, Value) {
    let mut t = TRACKER.lock().unwrap();
    load(&mut t);
    let calls: Vec<Value> = t.list.iter().map(|(n, at)| json!({"number": n, "at": at})).collect();
    (200, json!({"ok": true, "data": {"calls": calls}}))
}

pub fn clear() -> (u16, Value) {
    let mut t = TRACKER.lock().unwrap();
    load(&mut t);
    t.list.clear();
    save(&t);
    (200, json!({"ok": true}))
}
