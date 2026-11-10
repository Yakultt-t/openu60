//! Call state the app can wait on instead of polling blind.
//!
//! `AT+CLCC` is cheap but each request from the phone costs a round trip (a long one through
//! the relay) plus the modem's answer, and the app had to poll every 600 ms to feel live. While
//! anybody is watching, a thread here asks the modem every 250 ms and keeps the parsed list
//! with a version number; `GET /api/call/status?wait=<ms>&since=<version>` then answers at once
//! from the cache, or parks until the version moves. A change reaches the phone one round trip
//! after the modem reports it, and an idle call screen costs one request every `wait` ms.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::at_cmd;
use crate::handlers::AppState;
use crate::telephony::parse_clcc;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Keep polling this long after the last status request.
const WATCH_WINDOW: Duration = Duration::from_secs(20);
/// A cached answer older than this is refreshed inline before replying.
const MAX_STALE: Duration = Duration::from_millis(400);
const MAX_WAIT: Duration = Duration::from_secs(25);

struct State {
    calls: Vec<Value>,
    version: u64,
    updated: Option<Instant>,
    watched_until: Instant,
    last_error: Option<String>,
}

pub struct CallMonitor {
    state: Mutex<State>,
    changed: Condvar,
}

impl CallMonitor {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                calls: Vec::new(),
                version: 1,
                updated: None,
                watched_until: Instant::now(),
                last_error: None,
            }),
            changed: Condvar::new(),
        }
    }

    pub fn start(self: &Arc<Self>, app: Arc<AppState>) {
        let me = Arc::clone(self);
        crate::system::spawn_named("call-monitor", move || loop {
            let watched = me.state.lock().unwrap().watched_until > Instant::now();
            if watched {
                me.refresh(&app);
                std::thread::sleep(POLL_INTERVAL);
            } else {
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    /// One `AT+CLCC`; bumps the version when the call list changed.
    fn refresh(&self, app: &AppState) {
        match at_cmd::send(&app.at_port, "AT+CLCC", 2) {
            Ok(resp) => {
                let mut calls = parse_clcc(&resp);
                crate::ims_guard::fill_missing_numbers(&mut calls);
                if resp.contains("OK") {
                    crate::missed::observe(&calls);
                }
                // Fail closed, like the plain handler: only a terminal OK with no voice line
                // proves there is no call. The modem always lists its data bearers (mode 1),
                // which are not calls.
                if calls.iter().any(|c| c["mode"].as_i64() == Some(0)) {
                    app.call_active.store(true, Ordering::Relaxed);
                } else if resp.contains("OK") {
                    app.call_active.store(false, Ordering::Relaxed);
                } else {
                    return;
                }
                let mut st = self.state.lock().unwrap();
                if st.calls != calls || st.updated.is_none() {
                    let summary: Vec<String> = calls
                        .iter()
                        .filter(|c| c["mode"].as_i64() == Some(0))
                        .map(|c| format!("{}:{}:{}", c["id"], c["dir"].as_str().unwrap_or("?"), c["stat"].as_str().unwrap_or("?")))
                        .collect();
                    eprintln!("[call] clcc voice=[{}] at {}", summary.join(" "), crate::conn_watchdog::unix_now());
                    st.calls = calls;
                    st.version += 1;
                    self.changed.notify_all();
                }
                st.updated = Some(Instant::now());
                st.last_error = None;
            }
            Err(e) => {
                let mut st = self.state.lock().unwrap();
                st.last_error = Some(e);
                st.updated = Some(Instant::now());
                self.changed.notify_all();
            }
        }
    }

    /// The cached call list when it is fresh (someone is polling), so other pollers need not
    /// ask the modem themselves.
    pub fn recent_calls(&self) -> Option<Vec<Value>> {
        let st = self.state.lock().unwrap();
        let fresh = st.watched_until > Instant::now() && st.updated.map(|t| t.elapsed() < Duration::from_millis(1500)).unwrap_or(false);
        if fresh && st.last_error.is_none() { Some(st.calls.clone()) } else { None }
    }

    /// GET /api/call/status[?wait=ms&since=version]
    pub fn status(&self, app: &AppState, wait: Option<Duration>, since: Option<u64>) -> (u16, Value) {
        app.last_status_poll.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        let stale = {
            let mut st = self.state.lock().unwrap();
            st.watched_until = Instant::now() + WATCH_WINDOW;
            st.updated.map(|t| t.elapsed() > MAX_STALE).unwrap_or(true)
        };
        if stale {
            self.refresh(app);
        }
        let mut st = self.state.lock().unwrap();
        if let (Some(wait), Some(since)) = (wait, since) {
            let deadline = Instant::now() + wait.min(MAX_WAIT);
            while st.version == since && st.last_error.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let (guard, _) = self.changed.wait_timeout(st, left).unwrap();
                st = guard;
            }
        }
        if let Some(e) = &st.last_error {
            return (503, json!({"ok": false, "error": e}));
        }
        (200, json!({"ok": true, "data": {"calls": st.calls, "version": st.version}}))
    }
}
