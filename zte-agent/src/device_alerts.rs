//! Device status notifications: things the owner should know about the router itself.
//!
//! Checked once a minute; each condition notifies once when it starts (after a short grace
//! period, so a blip does not buzz the phone) and once when it clears:
//!   - battery low (20 %, then 10 %) while discharging
//!   - no mobile service (no LTE/NR signal for 3 minutes: no SMS or calls can arrive)
//!   - VoLTE (IMS) not registered for 3 minutes while there is service: calls fail
//!   - Wi-Fi uplink lost in client/repeater mode for 3 minutes (traffic falls back to the SIM)
//!   - battery temperature 50 °C or more
//!   - charging finished (off by default)
//! Missed calls are reported from missed.rs through `missed_call`.
//!
//! "The router is offline" cannot come from the router: it sends a signed heartbeat with the
//! interested devices' tokens to the push gateway once a minute, and the gateway notifies them
//! when the heartbeats stop (and again when they resume).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::handlers::AppState;
use crate::push::{DeviceAlert, Kind};

const GRACE_CHECKS: u32 = 3;

#[derive(Default)]
struct Flag {
    bad_for: u32,
    alerted: bool,
}

impl Flag {
    /// Feeds one check; returns Some(true) when an alert should go out, Some(false) for "cleared".
    fn step(&mut self, bad: bool, grace: u32) -> Option<bool> {
        if bad {
            self.bad_for += 1;
            if !self.alerted && self.bad_for >= grace {
                self.alerted = true;
                return Some(true);
            }
        } else {
            self.bad_for = 0;
            if self.alerted {
                self.alerted = false;
                return Some(false);
            }
        }
        None
    }
}

fn num(v: &Value, key: &str) -> Option<f64> {
    match &v[key] {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn start(state: Arc<AppState>) {
    {
        let state = Arc::clone(&state);
        crate::missed::set_hook(Box::new(move |number: &str| {
            let zh_body = format!("{number} 给你打过电话");
            let en_body = format!("Missed call from {number}");
            state.push.notify_device(Kind::Missed, ("未接来电", &zh_body), ("Missed call", &en_body), "openu60-missed");
        }));
    }
    let monitor = Arc::clone(&state);
    crate::system::spawn_named("device-alerts", move || monitor_loop(monitor));
    crate::system::spawn_named("gw-heartbeat", move || heartbeat_loop(state));
}

fn monitor_loop(state: Arc<AppState>) {
    // Let the modem, IMS and the uplink settle after a (re)start before judging anything.
    std::thread::sleep(Duration::from_secs(180));
    let (mut service, mut volte, mut uplink, mut heat) = (Flag::default(), Flag::default(), Flag::default(), Flag::default());
    let mut low_level = 100u8; // lowest level already announced (20 or 10)
    let mut was_charging = false;
    let mut charged_sent = false;
    let push = &state.push;
    loop {
        // Battery.
        if let Some(b) = crate::system::read_battery() {
            let b = serde_json::to_value(b).unwrap_or(Value::Null);
            let capacity = num(&b, "capacity").unwrap_or(-1.0);
            let status = b["status"].as_str().unwrap_or("");
            let discharging = status == "Discharging";
            if discharging && capacity >= 0.0 {
                for level in [10u8, 20] {
                    if capacity <= level as f64 && level < low_level {
                        low_level = level;
                        let zh = format!("电量还剩 {}%，记得给 U60 充电，没电后就收不到短信和电话了。", capacity as i64);
                        let en = format!("Battery at {}%. Charge the U60 or it will stop receiving SMS and calls.", capacity as i64);
                        push.notify_device(Kind::Device(DeviceAlert::Battery), ("U60 电量低", &zh), ("U60 battery low", &en), "openu60-battery");
                        break;
                    }
                }
            } else if !discharging || capacity >= 30.0 {
                low_level = 100;
            }
            // Charging finished: was charging, now full or held at the charge limit.
            if status == "Charging" {
                was_charging = true;
                charged_sent = false;
            } else if was_charging && !charged_sent && (status == "Full" || (status == "Not charging" && capacity >= 60.0)) {
                charged_sent = true;
                was_charging = false;
                let zh = format!("已充到 {}%。", capacity as i64);
                let en = format!("Charged to {}%.", capacity as i64);
                push.notify_device(Kind::Device(DeviceAlert::Charged), ("U60 充电完成", &zh), ("U60 charged", &en), "openu60-charged");
            } else if discharging {
                was_charging = false;
            }
            // Temperature (tenths of a degree in sysfs).
            if let Some(t) = num(&b, "temperature").map(|t| t / 10.0) {
                let bad = if heat.alerted { t >= 45.0 } else { t >= 50.0 };
                match heat.step(bad, 2) {
                    Some(true) => {
                        let zh = format!("电池温度 {t:.0}°C，请把 U60 放到通风处，避免暴晒或捂在包里。");
                        let en = format!("Battery at {t:.0}°C. Move the U60 somewhere cooler and ventilated.");
                        push.notify_device(Kind::Device(DeviceAlert::Temperature), ("U60 温度过高", &zh), ("U60 is hot", &en), "openu60-temp");
                    }
                    Some(false) => push.notify_device(Kind::Device(DeviceAlert::Temperature), ("U60 温度已恢复正常", ""), ("U60 has cooled down", ""), "openu60-temp"),
                    None => {}
                }
            }
        }

        // Mobile service and VoLTE.
        let (_, sig) = crate::handlers::network_signal(&state);
        let sig = sig.get("data").cloned().unwrap_or(sig);
        let has_service = num(&sig, "nr5g_rsrp").map(|v| v != 0.0).unwrap_or(false)
            || num(&sig, "lte_rsrp").map(|v| v != 0.0).unwrap_or(false);
        match service.step(!has_service, GRACE_CHECKS) {
            Some(true) => push.notify_device(
                Kind::Device(DeviceAlert::Service),
                ("U60 没有信号", "SIM 卡没有服务，暂时收不到短信和电话。请检查 SIM 卡或换个位置。"),
                ("U60 has no service", "No mobile service: SMS and calls cannot arrive. Check the SIM or move the router."),
                "openu60-service",
            ),
            Some(false) => push.notify_device(Kind::Device(DeviceAlert::Service), ("U60 信号已恢复", "又可以收短信和电话了。"), ("U60 service is back", "SMS and calls can arrive again."), "openu60-service"),
            None => {}
        }
        if has_service {
            let ims = crate::ims_guard::ims_registered();
            match volte.step(ims == Some(false), GRACE_CHECKS) {
                Some(true) => push.notify_device(
                    Kind::Device(DeviceAlert::Volte),
                    ("VoLTE 未注册", "国内来电可能打不进来，路由器正在自动重试。"),
                    ("VoLTE not registered", "Incoming calls may fail; the router keeps retrying."),
                    "openu60-volte",
                ),
                Some(false) => push.notify_device(Kind::Device(DeviceAlert::Volte), ("VoLTE 已恢复", "可以正常接打电话了。"), ("VoLTE is back", "Calls work again."), "openu60-volte"),
                None => {}
            }
        }

        // Wi-Fi uplink (only meaningful in client/repeater mode).
        let down = crate::wifi_wan::uplink_state() == Some(false);
        match uplink.step(down, GRACE_CHECKS) {
            Some(true) => push.notify_device(
                Kind::Device(DeviceAlert::Uplink),
                ("Wi-Fi 上行断开", "U60 连不上家里的 Wi-Fi，正在自动重连；如果开着移动数据，会改走 SIM 卡流量。"),
                ("Wi-Fi uplink lost", "The U60 lost the home Wi-Fi and keeps reconnecting; with mobile data on it uses the SIM meanwhile."),
                "openu60-uplink",
            ),
            Some(false) => push.notify_device(Kind::Device(DeviceAlert::Uplink), ("Wi-Fi 上行已恢复", ""), ("Wi-Fi uplink is back", ""), "openu60-uplink"),
            None => {}
        }

        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Tells the push gateway "still here" once a minute, with the devices to warn if that stops.
fn heartbeat_loop(state: Arc<AppState>) {
    let Some(base) = crate::push::gateway_url().map(|u| u.trim_end_matches('/').trim_end_matches("/v1/push").to_string()) else {
        return;
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .build()
    else {
        return;
    };
    std::thread::sleep(Duration::from_secs(30));
    loop {
        if let Some(tunnel) = state.tunnel.as_ref() {
            let watchers = state.push.offline_watchers();
            let body = json!({"watchers": watchers}).to_string();
            if let Err(e) = crate::watch_link::signed_post(&client, tunnel.identity(), &format!("{base}/v1/heartbeat"), &body) {
                eprintln!("[device-alerts] heartbeat: {e}");
            }
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}
