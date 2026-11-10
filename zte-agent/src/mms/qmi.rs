//! Second, read-only QMI WMS client over QRTR. The vendor SMS daemon keeps its routes and
//! acknowledgements; we only ask the modem to tell us about new stored messages and read
//! each one back before the daemon moves it, which is how WAP Push notifications are
//! captured (the daemon itself throws their payload away).

use std::mem;
use std::process::Command;
use std::time::{Duration, Instant};

const AF_QIPCRTR: libc::c_int = 42;
const WMS_SERVICE: u32 = 5;
const MSG_SET_EVENT_REPORT: u16 = 0x0001;
const MSG_EVENT_REPORT_IND: u16 = 0x0001;
const MSG_RAW_READ: u16 = 0x0022;
const MSG_GET_MESSAGE_PROTOCOL: u16 = 0x0030;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrQrtr {
    sq_family: libc::sa_family_t,
    sq_node: u32,
    sq_port: u32,
}

struct Client {
    fd: libc::c_int,
    dst: SockaddrQrtr,
    txn: u16,
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Finds the WMS service address with the firmware's `qrtr-lookup`.
pub fn lookup_wms() -> Result<(u32, u32), String> {
    let out = Command::new("qrtr-lookup").output().map_err(|e| format!("qrtr-lookup: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 5 && cols[0] == WMS_SERVICE.to_string() {
            if let (Ok(node), Ok(port)) = (cols[3].parse(), cols[4].parse()) {
                return Ok((node, port));
            }
        }
    }
    Err("WMS service not advertised on QRTR".into())
}

impl Client {
    fn open(node: u32, port: u32) -> Result<Self, String> {
        let fd = unsafe { libc::socket(AF_QIPCRTR, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(format!("socket(AF_QIPCRTR): {}", std::io::Error::last_os_error()));
        }
        let tv = libc::timeval { tv_sec: 1, tv_usec: 0 };
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void,
                             mem::size_of::<libc::timeval>() as libc::socklen_t);
        }
        Ok(Self { fd, dst: SockaddrQrtr { sq_family: AF_QIPCRTR as libc::sa_family_t, sq_node: node, sq_port: port }, txn: 1 })
    }

    fn send(&mut self, msg_id: u16, tlvs: &[u8]) -> Result<u16, String> {
        let txn = self.txn;
        self.txn = self.txn.wrapping_add(1).max(1);
        let mut buf = Vec::with_capacity(7 + tlvs.len());
        buf.push(0x00);
        buf.extend_from_slice(&txn.to_le_bytes());
        buf.extend_from_slice(&msg_id.to_le_bytes());
        buf.extend_from_slice(&(tlvs.len() as u16).to_le_bytes());
        buf.extend_from_slice(tlvs);
        let n = unsafe {
            libc::sendto(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0,
                         &self.dst as *const _ as *const libc::sockaddr, mem::size_of::<SockaddrQrtr>() as libc::socklen_t)
        };
        if n < 0 {
            return Err(format!("sendto: {}", std::io::Error::last_os_error()));
        }
        Ok(txn)
    }

    fn recv(&self) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 8192];
        let mut from: SockaddrQrtr = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0,
                           &mut from as *mut _ as *mut libc::sockaddr, &mut len)
        };
        if n < 7 {
            return None;
        }
        buf.truncate(n as usize);
        Some(buf)
    }
}

fn tlv(t: u8, v: &[u8]) -> Vec<u8> {
    let mut out = vec![t];
    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
    out.extend_from_slice(v);
    out
}

fn tlvs(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let t = data[i];
        let l = u16::from_le_bytes([data[i + 1], data[i + 2]]) as usize;
        i += 3;
        if i + l > data.len() {
            break;
        }
        out.push((t, &data[i..i + l]));
        i += l;
    }
    out
}

struct Msg<'a> {
    kind: u8,
    id: u16,
    body: &'a [u8],
}

fn parse(msg: &[u8]) -> Option<Msg<'_>> {
    let len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
    Some(Msg { kind: msg[0], id: u16::from_le_bytes([msg[3], msg[4]]), body: msg.get(7..7 + len)? })
}

/// Runs forever: (re)connects to the modem's WMS service, reports new stored messages
/// and hands every raw PDU read back to `on_pdu`. `status` receives short state strings.
pub fn run(on_pdu: impl Fn(&[u8]), status: impl Fn(String)) {
    let mut backoff = 5u64;
    loop {
        let (node, port) = match lookup_wms() {
            Ok(v) => v,
            Err(e) => {
                status(format!("lookup failed: {e}"));
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(120);
                continue;
            }
        };
        let mut client = match Client::open(node, port) {
            Ok(c) => c,
            Err(e) => {
                status(format!("open failed: {e}"));
                std::thread::sleep(Duration::from_secs(backoff));
                continue;
            }
        };
        if let Err(e) = client.send(MSG_SET_EVENT_REPORT, &tlv(0x10, &[0x01])) {
            status(format!("event report failed: {e}"));
            std::thread::sleep(Duration::from_secs(backoff));
            continue;
        }
        status(format!("listening on node {node} port {port}"));
        backoff = 5;
        let mut registered = false;
        let mut last_heartbeat = Instant::now();
        let mut heartbeat_pending: Option<Instant> = None;
        loop {
            match client.recv() {
                Some(msg) => {
                    let Some(m) = parse(&msg) else { continue };
                    match (m.kind, m.id) {
                        (0x02, MSG_SET_EVENT_REPORT) => registered = true,
                        (0x02, MSG_GET_MESSAGE_PROTOCOL) => heartbeat_pending = None,
                        (0x04, MSG_EVENT_REPORT_IND) => {
                            let kinds: Vec<String> = tlvs(m.body).iter().map(|(t, v)| format!("{t:02x}/{}", v.len())).collect();
                            eprintln!("[mms] wms event report [{}]", kinds.join(" "));
                            for (t, v) in tlvs(m.body) {
                                match t {
                                    0x10 if v.len() >= 5 => {
                                        // Stored MT message: read it back right away.
                                        let mut id = vec![v[0]];
                                        id.extend_from_slice(&v[1..5]);
                                        let mut body = tlv(0x01, &id);
                                        body.extend(tlv(0x10, &[0x01]));
                                        if let Err(e) = client.send(MSG_RAW_READ, &body) {
                                            status(format!("raw read failed: {e}"));
                                        }
                                    }
                                    0x11 if v.len() >= 8 => {
                                        // Transfer-route message: the PDU is inline.
                                        let dlen = u16::from_le_bytes([v[6], v[7]]) as usize;
                                        if let Some(pdu) = v.get(8..8 + dlen) {
                                            on_pdu(pdu);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        (0x02, MSG_RAW_READ) => {
                            for (t, v) in tlvs(m.body) {
                                if t == 0x01 && v.len() >= 4 {
                                    let dlen = u16::from_le_bytes([v[2], v[3]]) as usize;
                                    if let Some(pdu) = v.get(4..4 + dlen) {
                                        on_pdu(pdu);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                None => {
                    // Receive timeout: keep the session honest with a cheap request every
                    // minute; no answer within 5 s means the modem side is gone.
                    if let Some(at) = heartbeat_pending {
                        if at.elapsed() > Duration::from_secs(5) {
                            status("heartbeat lost, reconnecting".into());
                            break;
                        }
                    } else if last_heartbeat.elapsed() > Duration::from_secs(60) {
                        last_heartbeat = Instant::now();
                        heartbeat_pending = Some(Instant::now());
                        if client.send(MSG_GET_MESSAGE_PROTOCOL, &[]).is_err() {
                            status("send failed, reconnecting".into());
                            break;
                        }
                    }
                    if !registered && last_heartbeat.elapsed() > Duration::from_secs(10) {
                        status("event report unanswered, reconnecting".into());
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
