//! Keeps the modem's IMS (VoLTE) service switched on.
//!
//! With some carrier configurations (seen with a China Mobile SIM roaming on Telstra) the
//! modem comes up with "IMS registration service" disabled in its IMS settings, so it never
//! registers VoLTE: no incoming calls, no outgoing SMS, while a phone with the same SIM works.
//! Every minute this asks the IMS settings service (QMI 0x12, over QRTR) and turns the IMS
//! service back on if it is off.
use std::mem;
use std::process::Command;
use std::time::Duration;

const AF_QIPCRTR: libc::c_int = 42;
const IMSS_SERVICE: &str = "18";
const MSG_BIND: u16 = 0x0098;
const MSG_SET_SERVICES: u16 = 0x008f;
const MSG_GET_SERVICES: u16 = 0x0090;
const TLV_REGISTRATION_ENABLED: u8 = 0x19;
const TLV_SET_IMS_SERVICE: u8 = 0x18;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrQrtr {
    sq_family: libc::sa_family_t,
    sq_node: u32,
    sq_port: u32,
}

const IMSA_SERVICE: &str = "33";
const IMSA_BIND: u16 = 0x0033;
const IMSA_GET_REG: u16 = 0x0020;

fn lookup() -> Option<(u32, u32)> {
    lookup_service(IMSS_SERVICE)
}

fn lookup_service(service: &str) -> Option<(u32, u32)> {
    let out = Command::new("qrtr-lookup").output().ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let c: Vec<&str> = line.split_whitespace().collect();
        if c.len() >= 5 && c[0] == service {
            return Some((c[3].parse().ok()?, c[4].parse().ok()?));
        }
    }
    None
}

struct Client {
    fd: libc::c_int,
    dst: SockaddrQrtr,
    txn: u16,
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: fd is owned by this client.
        unsafe { libc::close(self.fd) };
    }
}

impl Client {
    fn open(node: u32, port: u32) -> Option<Self> {
        // SAFETY: plain socket calls; the timeval outlives setsockopt.
        unsafe {
            let fd = libc::socket(AF_QIPCRTR, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return None;
            }
            let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, mem::size_of::<libc::timeval>() as libc::socklen_t);
            Some(Self { fd, dst: SockaddrQrtr { sq_family: AF_QIPCRTR as libc::sa_family_t, sq_node: node, sq_port: port }, txn: 1 })
        }
    }

    /// One request, its response TLVs (`None` on timeout or QMI error).
    fn call(&mut self, msg: u16, tlvs: &[(u8, &[u8])]) -> Option<Vec<(u8, Vec<u8>)>> {
        let mut body = Vec::new();
        for (t, v) in tlvs {
            body.push(*t);
            body.extend_from_slice(&(v.len() as u16).to_le_bytes());
            body.extend_from_slice(v);
        }
        let txn = self.txn;
        self.txn = self.txn.wrapping_add(1).max(1);
        let mut buf = vec![0u8];
        buf.extend_from_slice(&txn.to_le_bytes());
        buf.extend_from_slice(&msg.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u16).to_le_bytes());
        buf.extend_from_slice(&body);
        // SAFETY: buffers are valid for the given lengths.
        unsafe {
            if libc::sendto(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0, &self.dst as *const _ as *const libc::sockaddr, mem::size_of::<SockaddrQrtr>() as libc::socklen_t) < 0 {
                return None;
            }
            for _ in 0..4 {
                let mut r = vec![0u8; 4096];
                let n = libc::recv(self.fd, r.as_mut_ptr() as *mut libc::c_void, r.len(), 0);
                if n < 7 {
                    return None;
                }
                r.truncate(n as usize);
                if r[0] != 2 || u16::from_le_bytes([r[1], r[2]]) != txn {
                    continue; // an indication or someone else's answer
                }
                let mut out = Vec::new();
                let mut i = 7;
                while i + 3 <= r.len() {
                    let t = r[i];
                    let l = u16::from_le_bytes([r[i + 1], r[i + 2]]) as usize;
                    i += 3;
                    if i + l > r.len() {
                        break;
                    }
                    out.push((t, r[i..i + l].to_vec()));
                    i += l;
                }
                // TLV 0x02: result (u16) + error (u16); non-zero result is a failure.
                let ok = out.iter().find(|(t, _)| *t == 2).map(|(_, v)| v.len() >= 2 && v[0] == 0 && v[1] == 0).unwrap_or(false);
                return ok.then_some(out);
            }
            None
        }
    }
}

/// Checks once; returns a short description of what it found or did.
fn check() -> Result<&'static str, String> {
    let (node, port) = lookup().ok_or("IMS settings service not on QRTR")?;
    let mut c = Client::open(node, port).ok_or("qrtr socket")?;
    c.call(MSG_BIND, &[(0x01, &0u32.to_le_bytes())]).ok_or("bind failed")?;
    let tlvs = c.call(MSG_GET_SERVICES, &[]).ok_or("get services failed")?;
    let enabled = tlvs.iter().find(|(t, _)| *t == TLV_REGISTRATION_ENABLED).map(|(_, v)| v.first() == Some(&1));
    match enabled {
        Some(true) => Ok("on"),
        Some(false) => {
            c.call(MSG_SET_SERVICES, &[(TLV_SET_IMS_SERVICE, &[1u8])]).ok_or("enable failed")?;
            Ok("was off, switched on")
        }
        None => Ok("not reported"),
    }
}

/// True when IMS is fully registered (IMSA registration status 2).
pub fn ims_registered() -> Option<bool> {
    let (node, port) = lookup_service(IMSA_SERVICE)?;
    let mut c = Client::open(node, port)?;
    c.call(IMSA_BIND, &[(0x10, &0u32.to_le_bytes())])?;
    let tlvs = c.call(IMSA_GET_REG, &[])?;
    let status = tlvs.iter().find(|(t, _)| *t == 0x12).and_then(|(_, v)| v.first().copied())?;
    Some(status == 2)
}

/// ZTE's SMS daemon decides IMS vs. the regular network path from `wms_ims_flag`, which it
/// does not refresh when IMS registers later. Over a roaming LTE network without that hint
/// outgoing SMS fail, so keep the flag in step with the modem.
fn sync_sms_flag() {
    let Some(registered) = ims_registered() else { return };
    let want = if registered { "1" } else { "0" };
    let have = crate::ubus::uci_get("zwrt_wms.config.wms_ims_flag").unwrap_or_default();
    if have.trim() != want {
        let arg = format!("{{\"option\":\"wms_ims_flag\",\"value\":\"{want}\"}}");
        let _ = crate::ubus::call("zwrt_wms", "zwrt_wms_uci_set", Some(&arg));
        eprintln!("[ims] IMS {}; SMS daemon told so", if registered { "registered" } else { "not registered" });
    }
}

pub fn start() {
    crate::system::spawn_named("ims-guard", || {
        // Give the modem time to load the SIM's carrier configuration first.
        std::thread::sleep(Duration::from_secs(45));
        let mut last = "";
        loop {
            match check() {
                Ok(state) => {
                    if state != last || state.starts_with("was off") {
                        eprintln!("[ims] IMS service {state}");
                    }
                    last = state;
                    sync_sms_flag();
                }
                Err(e) => {
                    if last != "error" {
                        eprintln!("[ims] check failed: {e}");
                    }
                    last = "error";
                }
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

const VOICE_SERVICE: &str = "9";
const VOICE_GET_ALL_CALL_INFO: u16 = 0x002f;
const TLV_REMOTE_PARTY_NUMBER: u8 = 0x11;

/// The caller number the modem holds for each current call, as (call id, number).
/// AT+CLCC leaves the number empty for some IMS calls (China Mobile roaming); the QMI voice
/// service still has it.
pub fn call_numbers() -> Vec<(i64, String)> {
    let Some((node, port)) = lookup_service(VOICE_SERVICE) else { return Vec::new() };
    let Some(mut c) = Client::open(node, port) else { return Vec::new() };
    let Some(tlvs) = c.call(VOICE_GET_ALL_CALL_INFO, &[]) else { return Vec::new() };
    let Some((_, v)) = tlvs.iter().find(|(t, _)| *t == TLV_REMOTE_PARTY_NUMBER) else { return Vec::new() };
    parse_remote_numbers(v)
}

/// TLV 0x11: count, then per call: id (u8), presentation (u8), length (u8), ASCII digits.
fn parse_remote_numbers(v: &[u8]) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    let Some(&count) = v.first() else { return out };
    let mut i = 1;
    for _ in 0..count {
        if i + 3 > v.len() {
            break;
        }
        let (id, len) = (v[i] as i64, v[i + 2] as usize);
        i += 3;
        if i + len > v.len() {
            break;
        }
        let number: String = String::from_utf8_lossy(&v[i..i + len])
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '+')
            .collect();
        i += len;
        if !number.is_empty() {
            out.push((id, number));
        }
    }
    out
}

#[cfg(test)]
mod voice_tests {
    #[test]
    fn parses_remote_party_numbers() {
        let mut v = vec![1u8, 5, 0, 4];
        v.extend_from_slice(b"+861");
        assert_eq!(super::parse_remote_numbers(&v), vec![(5, "+861".to_string())]);
        assert!(super::parse_remote_numbers(&[]).is_empty());
        assert!(super::parse_remote_numbers(&[1, 5, 0, 9, b'1']).is_empty());
    }
}

/// Fills in the number of voice calls whose AT+CLCC line came without one, from the QMI
/// voice service. The modem is asked only once per call id.
pub fn fill_missing_numbers(calls: &mut [serde_json::Value]) {
    use std::sync::Mutex;
    static KNOWN: Mutex<Vec<(i64, String)>> = Mutex::new(Vec::new());
    let voice = |c: &serde_json::Value| c["mode"].as_i64() == Some(0);
    let mut known = KNOWN.lock().unwrap();
    if !calls.iter().any(voice) {
        known.clear();
        return;
    }
    let missing: Vec<i64> = calls
        .iter()
        .filter(|c| voice(c) && c["number"].as_str().unwrap_or("").is_empty())
        .filter_map(|c| c["id"].as_i64())
        .collect();
    if missing.is_empty() {
        return;
    }
    if missing.iter().any(|id| !known.iter().any(|(k, _)| k == id)) {
        let fresh = call_numbers();
        // One voice call and one modem number: they are the same call even if the ids differ.
        if fresh.len() == 1 && missing.len() == 1 {
            known.retain(|(k, _)| *k != missing[0]);
            known.push((missing[0], fresh[0].1.clone()));
        }
        for (id, n) in fresh {
            if !known.iter().any(|(k, _)| *k == id) {
                known.push((id, n));
            }
        }
        // Remember a miss too, so the modem is not asked every 250 ms.
        for id in &missing {
            if !known.iter().any(|(k, _)| k == id) {
                known.push((*id, String::new()));
            }
        }
    }
    for c in calls.iter_mut().filter(|c| voice(c)) {
        if !c["number"].as_str().unwrap_or("").is_empty() {
            continue;
        }
        let id = c["id"].as_i64().unwrap_or(-1);
        if let Some((_, n)) = known.iter().find(|(k, _)| *k == id) {
            if !n.is_empty() {
                c["number"] = serde_json::Value::String(n.clone());
            }
        }
    }
}
