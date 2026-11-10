//! Read-only QMI WMS listener over QRTR: registers for new-message event reports and dumps
//! every indication, so WAP Push (MMS notification) PDUs can be captured without touching
//! the vendor SMS daemon's routes or acknowledgements. Prototype for the MMS work.
//!
//! usage: wms_listen <node> <port> [--raw-read <storage> <index>] [--routes]
//! (node/port of "Wireless Messaging Service" from `qrtr-lookup`)

use std::io::Write;
use std::mem;
use std::time::{SystemTime, UNIX_EPOCH};

const AF_QIPCRTR: libc::c_int = 42;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrQrtr {
    sq_family: libc::sa_family_t,
    sq_node: u32,
    sq_port: u32,
}

struct Qrtr {
    fd: libc::c_int,
    dst: SockaddrQrtr,
    txn: u16,
}

impl Qrtr {
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
        self.txn = self.txn.wrapping_add(1);
        let mut buf = Vec::with_capacity(7 + tlvs.len());
        buf.push(0x00); // request
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

    fn recv(&self) -> Option<(SockaddrQrtr, Vec<u8>)> {
        let mut buf = vec![0u8; 4096];
        let mut from: SockaddrQrtr = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0,
                           &mut from as *mut _ as *mut libc::sockaddr, &mut len)
        };
        if n < 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some((from, buf))
    }
}

fn tlv(t: u8, v: &[u8]) -> Vec<u8> {
    let mut out = vec![t];
    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
    out.extend_from_slice(v);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

fn parse_tlvs(data: &[u8]) -> Vec<(u8, &[u8])> {
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

fn stored_mt(msg: &[u8]) -> Option<(u8, u32)> {
    if msg.len() < 7 || msg[0] != 0x04 || u16::from_le_bytes([msg[3], msg[4]]) != 0x0001 {
        return None;
    }
    let len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
    let body = &msg[7..msg.len().min(7 + len)];
    parse_tlvs(body).into_iter().find(|(t, v)| *t == 0x10 && v.len() >= 5)
        .map(|(_, v)| (v[0], u32::from_le_bytes([v[1], v[2], v[3], v[4]])))
}

fn describe(msg: &[u8]) {
    if msg.len() < 7 {
        println!("[{}] short message: {}", now(), hex(msg));
        return;
    }
    let kind = msg[0];
    let txn = u16::from_le_bytes([msg[1], msg[2]]);
    let id = u16::from_le_bytes([msg[3], msg[4]]);
    let len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
    let body = &msg[7..msg.len().min(7 + len)];
    let kind_s = match kind { 0x00 => "request", 0x02 => "response", 0x04 => "indication", _ => "?" };
    println!("[{}] {} msg=0x{:04x} txn={} len={}", now(), kind_s, id, txn, len);
    for (t, v) in parse_tlvs(body) {
        match (kind, id, t) {
            (0x02, _, 0x02) if v.len() >= 4 => {
                let result = u16::from_le_bytes([v[0], v[1]]);
                let error = u16::from_le_bytes([v[2], v[3]]);
                println!("    result={} error={}", if result == 0 { "ok" } else { "FAIL" }, error);
            }
            (0x04, 0x0001, 0x10) if v.len() >= 5 => {
                let idx = u32::from_le_bytes([v[1], v[2], v[3], v[4]]);
                println!("    MT message stored: storage={} index={}", v[0], idx);
            }
            (0x04, 0x0001, 0x11) if v.len() >= 8 => {
                let txid = u32::from_le_bytes([v[1], v[2], v[3], v[4]]);
                let format = v[5];
                let dlen = u16::from_le_bytes([v[6], v[7]]) as usize;
                let pdu = &v[8..v.len().min(8 + dlen)];
                println!("    TRANSFER ROUTE MT: ack_indicator={} txid={} format={} pdu_len={}", v[0], txid, format, pdu.len());
                println!("    PDU {}", hex(pdu));
            }
            (0x04, 0x0001, 0x12) => println!("    message mode={}", hex(v)),
            (0x02, 0x0022, 0x01) if v.len() >= 4 => {
                let dlen = u16::from_le_bytes([v[2], v[3]]) as usize;
                println!("    RAW READ: tag={} format={} len={}", v[0], v[1], dlen);
                println!("    PDU {}", hex(&v[4..v.len().min(4 + dlen)]));
            }
            (0x02, 0x0033, 0x01) if v.len() >= 2 => {
                let n = u16::from_le_bytes([v[0], v[1]]) as usize;
                println!("    routes: {n}");
                for k in 0..n {
                    let e = &v[2 + k * 4..];
                    if e.len() >= 4 {
                        println!("      type={} class={} storage={} receipt_action={}", e[0], e[1], e[2], e[3]);
                    }
                }
            }
            _ => println!("    tlv 0x{:02x} ({} bytes) {}", t, v.len(), hex(v)),
        }
    }
    let _ = std::io::stdout().flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: wms_listen <node> <port> [--raw-read <storage> <index>] [--routes]");
        std::process::exit(2);
    }
    let node: u32 = args[1].parse().expect("node");
    let port: u32 = args[2].parse().expect("port");
    let mut q = Qrtr::open(node, port).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1) });
    println!("[{}] WMS listener -> node {} port {}", now(), node, port);

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--routes" => {
                q.send(0x0033, &[]).expect("send");
                i += 1;
            }
            "--raw-read" if i + 2 < args.len() => {
                let storage: u8 = args[i + 1].parse().expect("storage");
                let index: u32 = args[i + 2].parse().expect("index");
                let mut v = vec![storage];
                v.extend_from_slice(&index.to_le_bytes());
                let mut body = tlv(0x01, &v);
                body.extend(tlv(0x10, &[0x01])); // message mode: GW
                q.send(0x0022, &body).expect("send");
                i += 3;
            }
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }

    // Per-client event report: new MT messages. Does not touch routes or acks.
    q.send(0x0001, &tlv(0x10, &[0x01])).expect("send");
    loop {
        if let Some((from, msg)) = q.recv() {
            print!("[from node {} port {}] ", from.sq_node, from.sq_port);
            describe(&msg);
            // A stored-message indication: read the raw PDU back at once, before the vendor
            // daemon moves it out of modem storage.
            if let Some((storage, index)) = stored_mt(&msg) {
                let mut v = vec![storage];
                v.extend_from_slice(&index.to_le_bytes());
                let mut body = tlv(0x01, &v);
                body.extend(tlv(0x10, &[0x01]));
                match q.send(0x0022, &body) {
                    Ok(_) => println!("    -> raw read requested for storage={} index={}", storage, index),
                    Err(e) => println!("    -> raw read failed: {e}"),
                }
            }
        }
    }
}
