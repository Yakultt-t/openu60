//! Keeps the router off mobile data while the data switch is off.
//!
//! On LTE the network sets up a default bearer when the SIM attaches, and the firmware puts
//! it on the internet profile (cid 1, `attach_cid_enable`), so cid 1 stays connected with a
//! fallback default route even with mobile data switched off (`enable` 0). The router's own
//! traffic (relay, push, DNS, time) then goes out over it whenever the Wi-Fi uplink drops,
//! which on a roaming SIM can start a daily roaming charge (2026-11-27, Alan's China Mobile
//! SIM: about 1.5 MB in a day with the switch off).
//!
//! So while the switch is off, everything leaving through cid 1's interface is dropped, in
//! the router's own traffic (OUTPUT) and in what it forwards for its clients (FORWARD). The
//! rules live in their own chain, so turning data on (or a change of interface) just
//! empties it. IMS and MMS run on other cids and interfaces; cid 1's interface is left alone
//! whenever another session uses it too.

use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::ubus;

const CHAIN: &str = "openu60_data_off";
const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Other sessions (IMS, MMS…) whose interfaces must never be blocked.
const OTHER_CIDS: [u8; 3] = [2, 3, 4];

/// The interface currently blocked, if any; the outer None until the first check, which
/// always writes the chain (rules left by an earlier run may no longer apply).
static BLOCKED: Mutex<Option<Option<String>>> = Mutex::new(None);

fn iface(cid: u8) -> Option<(bool, String)> {
    let v = ubus::call("zwrt_data", "get_wwaniface", Some(&format!(r#"{{"cid":{cid}}}"#))).ok()?;
    let enabled = int(&v["enable"]) != Some(0);
    Some((enabled, dev(&v)?))
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The session's interface while it is up.
fn dev(v: &Value) -> Option<String> {
    let status = v["connect_status"].as_str().unwrap_or("");
    if !status.ends_with("connected") || status.starts_with("dis") {
        return None;
    }
    ["ipv4_dev_name", "ipv6_dev_name"]
        .iter()
        .filter_map(|k| v[*k].as_str())
        .find(|d| !d.is_empty() && *d != "null")
        .map(String::from)
}

/// Which interface to block: cid 1's, when the switch is off and no other session shares it.
pub fn target(cid1: Option<(bool, String)>, others: &[String]) -> Option<String> {
    let (enabled, dev) = cid1?;
    (!enabled && !others.contains(&dev)).then_some(dev)
}

fn ipt(tool: &str, args: &[&str]) -> bool {
    Command::new(tool).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

fn apply(dev: Option<&str>) {
    for tool in ["iptables", "ip6tables"] {
        if !ipt(tool, &["-n", "-L", CHAIN]) {
            ipt(tool, &["-N", CHAIN]);
        }
        // The firmware rebuilds its firewall now and then; put the jumps back each time.
        for parent in ["OUTPUT", "FORWARD"] {
            if !ipt(tool, &["-C", parent, "-j", CHAIN]) {
                ipt(tool, &["-I", parent, "1", "-j", CHAIN]);
            }
        }
        match dev {
            Some(d) => {
                if !ipt(tool, &["-C", CHAIN, "-o", d, "-j", "DROP"]) {
                    ipt(tool, &["-F", CHAIN]);
                    ipt(tool, &["-A", CHAIN, "-o", d, "-j", "DROP"]);
                }
            }
            None => {
                ipt(tool, &["-F", CHAIN]);
            }
        }
    }
}

/// One check; also run shortly after the app changes the data switch.
pub fn check() {
    let cid1 = iface(1);
    if cid1.is_none() && ubus::call("zwrt_data", "get_wwaniface", Some(r#"{"cid":1}"#)).is_err() {
        return; // data daemon not answering: change nothing
    }
    let others: Vec<String> = OTHER_CIDS.iter().filter_map(|c| iface(*c)).map(|(_, d)| d).collect();
    let want = target(cid1, &others);
    let mut blocked = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
    if want.is_none() && *blocked == Some(None) {
        return; // nothing blocked and nothing to block: leave the firewall alone
    }
    apply(want.as_deref());
    if blocked.as_ref() != Some(&want) {
        match &want {
            Some(d) => eprintln!("[data-guard] mobile data is off: blocking traffic out of {d} (the LTE attach session)"),
            None => eprintln!("[data-guard] mobile data not blocked"),
        }
        *blocked = Some(want);
    }
}

/// After a change of the data switch: the data daemon applies it asynchronously.
pub fn check_soon() {
    let _ = crate::system::try_spawn_named("data-guard-now", || {
        std::thread::sleep(Duration::from_secs(3));
        check();
    });
}

pub fn start() {
    crate::system::spawn_named("data-guard", || loop {
        check();
        std::thread::sleep(CHECK_EVERY);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blocks_only_cid1_with_the_switch_off_and_never_a_shared_interface() {
        let ims = vec!["rmnet_data1".to_string()];
        assert_eq!(target(Some((false, "rmnet_data0".into())), &ims).as_deref(), Some("rmnet_data0"));
        assert_eq!(target(Some((true, "rmnet_data0".into())), &ims), None);
        assert_eq!(target(Some((false, "rmnet_data1".into())), &ims), None);
        assert_eq!(target(None, &ims), None);
    }

    #[test]
    fn reads_the_interface_only_while_connected() {
        let up = json!({"connect_status": "ipv4_connected", "ipv4_dev_name": "rmnet_data0", "ipv6_dev_name": "null"});
        assert_eq!(dev(&up).as_deref(), Some("rmnet_data0"));
        let v6 = json!({"connect_status": "ipv6_connected", "ipv4_dev_name": "null", "ipv6_dev_name": "rmnet_data2"});
        assert_eq!(dev(&v6).as_deref(), Some("rmnet_data2"));
        assert_eq!(dev(&json!({"connect_status": "disconnected", "ipv4_dev_name": "rmnet_data0"})), None);
        assert_eq!(dev(&json!({"connect_status": "connecting", "ipv4_dev_name": "rmnet_data0"})), None);
    }
}
