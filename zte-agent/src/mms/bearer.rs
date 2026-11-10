//! The MMS data path: a second PDN on the carrier's MMS APN (firmware PDN slot 4 by default),
//! plus the MMSC URL and WAP proxy to use through it. Carrier defaults come from the
//! Android APN table; `/data/local/tmp/mms.json` overrides any field.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ubus;

pub const CONFIG_PATH: &str = "/data/local/tmp/mms.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub carrier: String,
    pub apn: String,
    pub mmsc: String,
    /// "host:port" of the WAP/MMS proxy, empty when the MMSC is reached directly.
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Firmware PDN slot to use for the MMS APN.
    #[serde(default = "default_cid")]
    pub cid: u32,
}

/// Not slot 2: on many carrier configurations that is the IMS (VoLTE) profile, and writing
/// the MMS APN into it breaks calls. Slot 4 is unused by the firmware.
fn default_cid() -> u32 {
    4
}

impl Default for Profile {
    /// Used for carriers missing from the table; `mms.json` fills in the rest.
    fn default() -> Self {
        Profile {
            carrier: String::new(), apn: String::new(), mmsc: String::new(), proxy: String::new(),
            username: String::new(), password: String::new(), cid: default_cid(),
        }
    }
}

fn table(mccmnc: &str) -> Option<Profile> {
    let p = |carrier: &str, apn: &str, mmsc: &str, proxy: &str| Profile {
        carrier: carrier.into(), apn: apn.into(), mmsc: mmsc.into(), proxy: proxy.into(),
        username: String::new(), password: String::new(), cid: default_cid(),
    };
    Some(match mccmnc {
        "46000" | "46002" | "46004" | "46007" | "46008" | "46013" | "46015" | "46060" =>
            p("China Mobile", "cmwap", "http://mmsc.monternet.com", "10.0.0.172:80"),
        "46001" | "46006" | "46009" =>
            p("China Unicom", "3gwap", "http://mmsc.myuni.com.cn", "10.0.0.172:80"),
        "46003" | "46005" | "46011" =>
            p("China Telecom", "ctwap", "http://mmsc.vnet.mobi", "10.0.0.200:80"),
        "50501" => p("Telstra", "telstra.mms", "http://mmsc.telstra.com:8002", "10.1.1.180:80"),
        "50502" => p("Optus", "mms", "http://mmsc.optus.com.au:8002/", "61.88.190.10:8070"),
        "50503" => p("Vodafone AU", "live.vodafone.com", "http://pxt.vodafone.net.au/pxtsend", "10.202.2.60:8080"),
        _ => return None,
    })
}

/// Registered (visited) network.
pub fn plmn() -> Option<String> {
    ubus::uci_get("zte_nwinfo.plmn_info.rplmn_mccmnc").ok().map(|s| s.trim().to_string()).filter(|s| s.len() >= 5)
}

/// The SIM's own operator (from the SIM, not the network it is camped on). MMS is always
/// served by the home operator's MMSC, also while roaming: a China Mobile card in Australia
/// still uses cmwap / mmsc.monternet.com through its home-routed APN.
pub fn home_plmn() -> Option<String> {
    let v = ubus::call("zwrt_zte_mdm.api", "get_sim_info", Some("{}")).ok()?;
    let mcc = v["mdm_mcc"].as_str()?.trim().to_string();
    let mnc = v["mdm_mnc"].as_str()?.trim().to_string();
    if mcc.len() == 3 && (2..=3).contains(&mnc.len()) && mcc.chars().all(|c| c.is_ascii_digit()) && mnc.chars().all(|c| c.is_ascii_digit()) {
        Some(format!("{mcc}{mnc}"))
    } else {
        None
    }
}

/// Effective profile: the home operator's defaults (falling back to the registered network)
/// with any overrides.
pub fn profile() -> Result<Profile, String> {
    let home = home_plmn();
    let mccmnc = home
        .clone()
        .filter(|h| table(h).is_some())
        .or_else(plmn)
        .unwrap_or_default();
    let mut p = table(&mccmnc).unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(CONFIG_PATH) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            for (k, slot) in [("apn", &mut p.apn), ("mmsc", &mut p.mmsc), ("proxy", &mut p.proxy),
                              ("username", &mut p.username), ("password", &mut p.password), ("carrier", &mut p.carrier)] {
                if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                    *slot = s.to_string();
                }
            }
            if let Some(c) = v.get("cid").and_then(|x| x.as_u64()) {
                p.cid = c as u32;
            }
        }
    }
    if p.carrier.is_empty() {
        p.carrier = if mccmnc.is_empty() { "unknown".into() } else { mccmnc.clone() };
    }
    if p.apn.is_empty() || p.mmsc.is_empty() {
        return Err(format!("no MMS settings for network {mccmnc}; write {CONFIG_PATH}"));
    }
    Ok(p)
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Link {
    pub device: String,
    pub address: String,
    pub cid: u32,
}

fn iface(cid: u32) -> Result<Value, String> {
    ubus::call("zwrt_data", "get_wwaniface", Some(&json!({"cid": cid}).to_string()))
}

fn link_of(v: &Value, cid: u32) -> Option<Link> {
    let status = v["connect_status"].as_str().unwrap_or("");
    let dev = v["ipv4_dev_name"].as_str().unwrap_or("null");
    if status.starts_with("ipv4_connected") && dev != "null" && !dev.is_empty() {
        Some(Link { device: dev.to_string(), address: v["ipv4_address"].as_str().unwrap_or("").to_string(), cid })
    } else {
        None
    }
}

pub fn current(cid: u32) -> Option<Link> {
    iface(cid).ok().and_then(|v| link_of(&v, cid))
}

/// Brings the MMS PDN up (idempotent) and returns the interface to bind to.
pub fn ensure_up(p: &Profile) -> Result<Link, String> {
    if let Some(l) = current(p.cid) {
        // Same APN already dialled on this slot?
        let apn_now = ubus::call("zwrt_apn_object", "get_apn_at_cid", Some(&json!({"cid": p.cid}).to_string()))
            .ok().and_then(|v| v["wanapn"].as_str().map(|s| s.to_string())).unwrap_or_default();
        if apn_now.eq_ignore_ascii_case(&p.apn) {
            return Ok(l);
        }
        eprintln!("[mms] slot {} carries APN {apn_now}, re-dialling for {}", p.cid, p.apn);
        let _ = ubus::call("zwrt_data", "set_wwaniface", Some(&json!({"source_module": "zte-agent", "cid": p.cid, "enable": 0}).to_string()));
        std::thread::sleep(Duration::from_secs(2));
    }
    let apn = json!({
        "cid": p.cid, "profilename": format!("OpenU60 MMS ({})", p.carrier), "wanapn": p.apn,
        "username": p.username, "password": p.password, "pdpType": 1, "pppAuthMode": if p.username.is_empty() { 0 } else { 3 },
        "profileId": format!("openu60_mms{}", p.cid), "isEnable": true, "isValid": 1, "extraInt1": 0, "roamingPdpType": 1
    });
    ubus::call("zwrt_apn_object", "set_apn_at_cid", Some(&apn.to_string())).map_err(|e| format!("set_apn_at_cid: {e}"))?;
    ubus::call("zwrt_data", "set_wwaniface", Some(&json!({"source_module": "zte-agent", "cid": p.cid, "enable": 1, "connect_mode": 1}).to_string()))
        .map_err(|e| format!("set_wwaniface: {e}"))?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(l) = current(p.cid) {
            eprintln!("[mms] PDN up: {} {} (cid {})", l.device, l.address, l.cid);
            return Ok(l);
        }
    }
    Err(format!("MMS APN {} did not connect within 30 s", p.apn))
}

/// Tears the MMS PDN down and dials it again; the MMSC occasionally stops answering or
/// rejects the originator until the bearer is fresh.
pub fn redial(p: &Profile) -> Result<Link, String> {
    eprintln!("[mms] re-dialling MMS PDN on cid {}", p.cid);
    let _ = ubus::call("zwrt_data", "set_wwaniface", Some(&json!({"source_module": "zte-agent", "cid": p.cid, "enable": 0}).to_string()));
    std::thread::sleep(Duration::from_secs(3));
    ensure_up(p)
}

pub fn status() -> Value {
    match profile() {
        Ok(p) => {
            let link = current(p.cid);
            json!({
                "carrier": p.carrier, "plmn": plmn(), "home_plmn": home_plmn(), "apn": p.apn, "mmsc": p.mmsc, "proxy": p.proxy, "cid": p.cid,
                "connected": link.is_some(), "device": link.as_ref().map(|l| l.device.clone()),
                "address": link.as_ref().map(|l| l.address.clone()),
            })
        }
        Err(e) => json!({"plmn": plmn(), "home_plmn": home_plmn(), "error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_carriers_get_the_spare_pdn_slot() {
        assert_eq!(Profile::default().cid, 4);
        assert!(table("99999").is_none());
        let parsed: Profile = serde_json::from_str(r#"{"carrier":"x","apn":"mms","mmsc":"http://m"}"#).unwrap();
        assert_eq!(parsed.cid, 4);
        assert_eq!(table("46000").unwrap().cid, 4);
    }
}
