//! Runs what a tap asked for against zte-agent, off the UI thread, then asks the data
//! thread to refresh at once and shows the outcome as a toast.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use crate::data::{Api, REFRESH};
use crate::model::Status;
use crate::pages::Action;

const MODE_FILE: &str = "/data/local/tmp/wifi_mode.conf";

/// (ssid, psk) of the last repeater network the router used.
pub fn last_relay() -> Option<(String, String)> {
    let text = std::fs::read_to_string(MODE_FILE).ok()?;
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("").to_string();
    let ssid = get("ssid");
    (!ssid.is_empty()).then(|| (ssid, get("psk")))
}

fn toast(status: &Arc<Mutex<Status>>, text: impl Into<String>) {
    status.lock().unwrap().toast = Some((text.into(), Instant::now()));
}

fn result(v: Option<Value>, ok: &str, fail: &str) -> String {
    match v {
        Some(_) => ok.to_string(),
        None => fail.to_string(),
    }
}

pub fn run(action: Action, status: Arc<Mutex<Status>>) {
    std::thread::spawn(move || {
        let mut api = Api::new();
        let snapshot = status.lock().unwrap().clone();
        let msg = match action {
            Action::SetMode(mode) => {
                if mode == "hotspot" {
                    result(api.send("PUT", "/api/wifi/wan/mode", json!({"mode": "hotspot"})), "已切到热点模式，用 SIM 卡上网", "切换失败")
                } else {
                    // Keep the network in use; otherwise the last one the router joined.
                    let target = if snapshot.wifi_uplink && !snapshot.relay_ssid.is_empty() {
                        last_relay().filter(|(s, _)| *s == snapshot.relay_ssid).or_else(|| Some((snapshot.relay_ssid.clone(), String::new())))
                    } else {
                        last_relay()
                    };
                    match target {
                        Some((ssid, psk)) => {
                            status.lock().unwrap().relay_connecting = true;
                            let r = api.send("PUT", "/api/wifi/wan/mode", json!({"mode": mode, "ssid": ssid, "psk": psk}));
                            status.lock().unwrap().relay_connecting = false;
                            let ok = if mode == "client" { format!("已切到「连接 Wi-Fi」，热点关闭，手机请连「{ssid}」") } else { format!("已切到「Wi-Fi 中继」：{ssid}") };
                            result(r, &ok, "切换失败，换一个网络试试")
                        }
                        None => "还没连过 Wi-Fi，请在下面选一个网络".to_string(),
                    }
                }
            }
            Action::RelayScan => {
                status.lock().unwrap().scanning = true;
                let _ = api.send("POST", "/api/wifi/wan/scan", json!({}));
                std::thread::sleep(std::time::Duration::from_secs(4));
                let nets = api.get("/api/wifi/wan/scan");
                let mut s = status.lock().unwrap();
                s.scanning = false;
                if let Some(list) = nets.as_ref().and_then(|v| v["networks"].as_array().cloned()) {
                    let mut seen = std::collections::HashSet::new();
                    s.scan = list
                        .iter()
                        .filter_map(|n| {
                            let ssid = n["ssid"].as_str()?.to_string();
                            (!ssid.is_empty() && seen.insert(ssid.clone())).then(|| crate::model::Net {
                                ssid,
                                signal: n["signal"].as_i64().unwrap_or(-90) as i32,
                                secure: n["security"].as_str().map(|v| !v.is_empty() && v != "open" && v != "none").unwrap_or(true),
                                freq: n["freq"].as_u64().unwrap_or(0) as u32,
                            })
                        })
                        .collect();
                    s.scan.sort_by(|a, b| b.signal.cmp(&a.signal));
                    String::new()
                } else {
                    "扫描失败".to_string()
                }
            }
            Action::RelayJoin(ssid, known) => {
                if !known {
                    format!("「{ssid}」需要密码，请在手机 SIM 随行 app 里连接")
                } else {
                    let psk = last_relay().filter(|(s, _)| *s == ssid).map(|(_, p)| p).unwrap_or_default();
                    // Joining from the screen keeps the hotspot unless "连接 Wi-Fi" is chosen.
                    let mode = if snapshot.uplink_mode == "client" { "client" } else { "repeater" };
                    status.lock().unwrap().relay_connecting = true;
                    let r = api.send("PUT", "/api/wifi/wan/mode", json!({"mode": mode, "ssid": ssid, "psk": psk}));
                    status.lock().unwrap().relay_connecting = false;
                    result(r, &format!("已连上「{ssid}」"), "连接失败")
                }
            }
            Action::ChargeLimitToggle => {
                let on = !snapshot.charge_limit_enabled;
                let r = api.send("PUT", "/api/device/charge-control", json!({"charge_limit_enabled": on, "charge_limit": snapshot.charge_limit.max(50)}));
                if r.is_some() { status.lock().unwrap().charge_limit_enabled = on; }
                result(r, if on { "已打开直供电" } else { "已关闭直供电" }, "设置失败")
            }
            Action::ChargeLimit(delta) => {
                let limit = (snapshot.charge_limit as i32 + delta).clamp(50, 100) as u32;
                status.lock().unwrap().charge_limit = limit;
                let r = api.send("PUT", "/api/device/charge-control", json!({"charge_limit": limit, "charge_limit_enabled": snapshot.charge_limit_enabled}));
                if r.is_none() { "设置失败".to_string() } else { String::new() }
            }
            Action::PowerbankToggle => {
                let on = !snapshot.powerbank;
                let r = api.send("PUT", "/api/usb/powerbank", json!({"state": if on { 1 } else { 0 }}));
                if r.is_some() { status.lock().unwrap().powerbank = on; }
                result(r, if on { "已打开反向供电" } else { "已关闭反向供电" }, "设置失败")
            }
            Action::AdblockToggle => {
                let on = !snapshot.adblock.enabled;
                let r = api.send("PUT", "/api/adblock/config", json!({"enabled": on}));
                if r.is_some() { status.lock().unwrap().adblock.enabled = on; }
                result(r, if on { "已打开广告拦截，首次会先下载规则" } else { "已关闭广告拦截" }, "设置失败")
            }
            Action::AdblockPause(minutes) => {
                let body = if minutes == 0 {
                    // Until 07:00 local time tomorrow (or today, if it is still before 7).
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                    let off = (snapshot.tz_hours * 3600.0) as i64;
                    let local = now + off;
                    let mut target = local - local.rem_euclid(86_400) + 7 * 3600;
                    if target <= local { target += 86_400; }
                    json!({"until": (target - off) as u64})
                } else {
                    json!({"minutes": minutes})
                };
                let r = api.send("POST", "/api/adblock/pause", body);
                let label = if minutes == 0 { "已暂停到明早 7 点".to_string() } else if minutes >= 60 { format!("已暂停 {} 小时", minutes / 60) } else { format!("已暂停 {minutes} 分钟") };
                result(r, &label, "暂停失败")
            }
            Action::AdblockResume => result(api.send("POST", "/api/adblock/resume", json!({})), "已恢复拦截", "恢复失败"),
            Action::AdblockUpdate => result(api.send("POST", "/api/adblock/update", json!({})), "正在更新广告规则", "更新失败"),
            Action::DismissMissed => {
                let r = api.send("POST", "/api/call/missed/clear", json!({}));
                if r.is_some() { status.lock().unwrap().missed.clear(); }
                String::new()
            }
            Action::Hangup => result(api.send("POST", "/api/call/hangup", json!({})), "已挂断", "挂断失败"),
            Action::SmsMarkRead(ids) => {
                let id = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(";") + ";";
                if api.send("POST", "/api/sms/read", json!({"id": id, "tag": 0})).is_some() {
                    let mut s = status.lock().unwrap();
                    for m in s.sms.iter_mut().filter(|m| ids.contains(&m.id)) {
                        m.unread = false;
                    }
                }
                String::new()
            }
            Action::TzAuto(on) => {
                // Turning automatic off starts from the offset shown now.
                let body = if on {
                    json!({"tz_mode": "auto"})
                } else {
                    json!({"tz_mode": "manual", "zone": crate::clock_label(snapshot.clock.offset_min)})
                };
                let r = api.send("PUT", "/api/system/time", body);
                result(r, if on { "已改成自动时区" } else { "已改成手动时区" }, "设置失败")
            }
            Action::TzStep(step) => {
                let off = (snapshot.clock.offset_min + step).clamp(-12 * 60, 14 * 60);
                status.lock().unwrap().clock.offset_min = off;
                let r = api.send("PUT", "/api/system/time", json!({"tz_mode": "manual", "zone": crate::clock_label(off)}));
                if r.is_none() { "设置失败".to_string() } else { String::new() }
            }
            Action::TimeAutoToggle => {
                let on = !snapshot.clock.time_auto;
                let r = api.send("PUT", "/api/system/time", json!({"time_auto": on}));
                result(r, if on { "已打开自动校时" } else { "已关闭自动校时" }, "设置失败")
            }
            Action::SyncNow => {
                let r = api.send("PUT", "/api/system/time", json!({"sync_now": true}));
                match r {
                    Some(v) if v["last_sync_error"].as_str().map(|e| e.is_empty()).unwrap_or(true) => "时间已校准".to_string(),
                    Some(_) => "校准失败，请检查网络".to_string(),
                    None => "校准失败".to_string(),
                }
            }
            _ => String::new(),
        };
        if !msg.is_empty() {
            toast(&status, msg);
        }
        REFRESH.store(true, Ordering::SeqCst);
    });
}
