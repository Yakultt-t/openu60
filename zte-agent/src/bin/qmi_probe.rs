//! Read-only QMI probe over QRTR: sends one request to (node, port) and dumps every reply
//! that arrives within two seconds, TLV by TLV.
//!   qmi_probe <node> <port> <msg_id hex> [tlv_type:hexbytes ...]
use std::mem;

const AF_QIPCRTR: libc::c_int = 42;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrQrtr {
    sq_family: libc::sa_family_t,
    sq_node: u32,
    sq_port: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: qmi_probe <node> <port> <msg_id hex> [type:hex ...]");
        std::process::exit(2);
    }
    let node: u32 = args[0].parse().unwrap();
    let port: u32 = args[1].parse().unwrap();
    // Several requests on one client: "msg [type:hex...] -- msg [type:hex...]". Services such
    // as IMSA need a Bind on the same client before anything else answers.
    let mut requests: Vec<(u16, Vec<u8>)> = Vec::new();
    for group in args[2..].split(|a| a == "--") {
        if group.is_empty() { continue; }
        let msg = u16::from_str_radix(group[0].trim_start_matches("0x"), 16).unwrap();
        let mut body = Vec::new();
        for a in &group[1..] {
            let (t, v) = a.split_once(':').expect("type:hex");
            let t = u8::from_str_radix(t.trim_start_matches("0x"), 16).unwrap();
            let v: Vec<u8> = (0..v.len()).step_by(2).map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap()).collect();
            body.push(t);
            body.extend_from_slice(&(v.len() as u16).to_le_bytes());
            body.extend_from_slice(&v);
        }
        requests.push((msg, body));
    }
    unsafe {
        let fd = libc::socket(AF_QIPCRTR, libc::SOCK_DGRAM, 0);
        assert!(fd >= 0, "socket");
        let wait: i64 = std::env::var("WAIT_IND").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        // HEXMAX=<n> prints up to n bytes of each TLV (default 64).
        let hexmax: usize = std::env::var("HEXMAX").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
        let tv = libc::timeval { tv_sec: wait as _, tv_usec: 0 };
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, mem::size_of::<libc::timeval>() as u32);
        let dst = SockaddrQrtr { sq_family: AF_QIPCRTR as libc::sa_family_t, sq_node: node, sq_port: port };
        for (txn, (msg, body)) in requests.iter().enumerate() {
        let mut buf = vec![0x00u8];
        buf.extend_from_slice(&((txn as u16) + 7).to_le_bytes());
        buf.extend_from_slice(&msg.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u16).to_le_bytes());
        buf.extend_from_slice(body);
        let n = libc::sendto(fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0, &dst as *const _ as *const libc::sockaddr, mem::size_of::<SockaddrQrtr>() as u32);
        assert!(n > 0, "sendto");
        loop {
            let mut r = vec![0u8; 8192];
            let n = libc::recv(fd, r.as_mut_ptr() as *mut libc::c_void, r.len(), 0);
            if n < 7 {
                break;
            }
            r.truncate(n as usize);
            let kind = r[0];
            let id = u16::from_le_bytes([r[3], r[4]]);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            println!("{now} {} msg 0x{:04x} len {}", match kind { 2 => "RESP", 4 => "IND", _ => "MSG" }, id, n - 7);
            let mut i = 7;
            while i + 3 <= r.len() {
                let t = r[i];
                let l = u16::from_le_bytes([r[i + 1], r[i + 2]]) as usize;
                i += 3;
                if i + l > r.len() { break; }
                let v = &r[i..i + l];
                let hex: String = v.iter().take(hexmax).map(|b| format!("{b:02x}")).collect();
                let txt: String = v.iter().take(64).map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
                println!("  tlv 0x{t:02x} len {l:3}: {hex}  {txt}");
                i += l;
            }
            if kind == 2 && std::env::var("WAIT_IND").is_err() { break; }
        }
        }
        libc::close(fd);
    }
}
