//! The router's own web admin (ubus over JSON-RPC at http://<gateway>/ubus/). Used once, to
//! switch the USB port to debug mode so ADB appears. Same login as the web page: SHA-256 of
//! the password, then SHA-256 again with the router's salt (uppercase hex). Never stored.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ANON: &str = "00000000000000000000000000000000";

fn agent() -> ureq::Agent {
    // LAN only: never through a system proxy.
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .proxy(None)
        .http_status_as_error(false)
        .build()
        .into()
}

fn sha_upper(s: &str) -> String {
    hex::encode_upper(Sha256::digest(s.as_bytes()))
}

fn call(a: &ureq::Agent, gw: &str, session: &str, object: &str, method: &str, params: Value) -> Result<Value, String> {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let url = format!("http://{gw}/ubus/?t={ms}");
    let body = json!([{"jsonrpc": "2.0", "id": 1, "method": "call", "params": [session, object, method, params]}]);
    let mut resp = a
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Referer", &format!("http://{gw}/"))
        .header("Origin", &format!("http://{gw}"))
        .header("X-Requested-With", "XMLHttpRequest")
        .send(body.to_string())
        .map_err(|e| format!("连不上 http://{gw}：{e}。请确认电脑连着路由器的 Wi-Fi 或 USB 网络。"))?;
    let text = resp.body_mut().read_to_string().map_err(|e| format!("读取回应失败：{e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| "路由器的回应看不懂（这是中兴 U60 Pro 吗？）".to_string())?;
    let result = v.get(0).and_then(|r| r.get("result")).and_then(|r| r.as_array()).ok_or("路由器的回应看不懂")?;
    if let Some(code) = result.first().and_then(|c| c.as_i64()) {
        if code != 0 {
            return Err(format!("路由器拒绝了请求（错误 {code}）"));
        }
    }
    Ok(result.get(1).cloned().unwrap_or(Value::Null))
}

pub fn enable_debug_mode(gw: &str, password: &str) -> Result<(), String> {
    let a = agent();
    let info = call(&a, gw, ANON, "zwrt_web", "web_login_info", json!({"": ""}))?;
    let salt = info.get("zte_web_sault").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).ok_or_else(|| {
        format!("路由器没有返回登录参数，确认地址是 {gw}，并且这台电脑连着它。")
    })?;
    let h2 = sha_upper(&(sha_upper(password) + salt));
    let login = call(&a, gw, ANON, "zwrt_web", "web_login", json!({"password": h2}))
        .map_err(|_| "后台密码不对，或者登录次数太多被暂时锁定了。".to_string())?;
    let session = login
        .get("ubus_rpc_session")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("后台密码不对，或者登录次数太多被暂时锁定了。")?;
    call(&a, gw, session, "zwrt_bsp.usb", "set", json!({"mode": "debug"}))?;
    Ok(())
}

/// Logs in to the agent through an `adb forward` port and returns /api/tunnel/info.
pub fn agent_login(port: u16, password: &str) -> Result<Value, String> {
    let a: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(4)))
        .proxy(None)
        .build()
        .into();
    let mut r = a
        .post(&format!("http://127.0.0.1:{port}/api/auth/login"))
        .header("Content-Type", "application/json")
        .send(json!({"password": password}).to_string())
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let token = find_str(&v, "token").ok_or("no token")?;
    let mut r = a
        .get(&format!("http://127.0.0.1:{port}/api/tunnel/info"))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(v)
}

/// First string value under `key` anywhere in the JSON tree.
pub fn find_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(s) = m.get(key).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            m.values().find_map(|x| find_str(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_str(x, key)),
        _ => None,
    }
}
