//! Qualcomm DIAG client over the firmware's own `diag_socket_log` bridge.
//!
//! QPST reads and writes NV items and EFS files through the DIAG port that only the USB
//! factory composition exposes. On the router itself the same protocol is reachable without
//! USB: `diag_socket_log -a <ip> -p <port>` switches the diag router to socket mode and
//! connects out to a TCP listener, carrying HDLC-framed DIAG packets. We listen on loopback,
//! start it, talk, then `diag_socket_log -k` puts diag back the way it was.
//!
//! Verified on a U60 Pro (MPSS.DE.7.0) on 2026-11-17: version query, NV_READ of item 5280 and
//! EFS2 GET of /nv/item_files/modem/mmode/sms_only. The write commands (NV_WRITE, EFS2 PUT)
//! follow the same documented layouts but are only checked by reading back after the write.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One DIAG session at a time: the diag router has a single socket client.
static LOCK: Mutex<()> = Mutex::new(());

const NV_READ: u8 = 0x26;
const NV_WRITE: u8 = 0x27;
const SPC: u8 = 0x41;
const BAD_SECURITY_MODE: u8 = 0x42;
const SUBSYS: u8 = 0x4B;
const SUBSYS_FS: u8 = 0x13;
const EFS2_PUT: u16 = 38;
const EFS2_GET: u16 = 39;
/// NV item payloads are always 128 bytes on the wire.
pub const NV_DATA_LEN: usize = 128;

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    !crc
}

pub fn hdlc_encode(payload: &[u8]) -> Vec<u8> {
    let mut raw = payload.to_vec();
    raw.extend_from_slice(&crc16(payload).to_le_bytes());
    let mut out = Vec::with_capacity(raw.len() + 4);
    for b in raw {
        if b == 0x7D || b == 0x7E {
            out.push(0x7D);
            out.push(b ^ 0x20);
        } else {
            out.push(b);
        }
    }
    out.push(0x7E);
    out
}

/// One frame without its 0x7E terminator; None when the CRC does not match.
pub fn hdlc_decode(frame: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.len());
    let mut esc = false;
    for &b in frame {
        if esc {
            out.push(b ^ 0x20);
            esc = false;
        } else if b == 0x7D {
            esc = true;
        } else {
            out.push(b);
        }
    }
    if out.len() < 3 {
        return None;
    }
    let (body, crc) = out.split_at(out.len() - 2);
    (crc16(body) == u16::from_le_bytes([crc[0], crc[1]])).then(|| body.to_vec())
}

pub struct Session {
    stream: TcpStream,
    bridge: Child,
    buf: Vec<u8>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Session {
    /// Starts the bridge and waits for it to connect (up to 15 s).
    pub fn open() -> Result<Session, String> {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut bridge = Command::new("diag_socket_log")
            .args(["-a", "127.0.0.1", "-p", &port.to_string(), "-r", "3", "-m", "2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("diag_socket_log: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline || matches!(bridge.try_wait(), Ok(Some(_))) {
                        stop_bridge(&mut bridge);
                        return Err("diag bridge did not connect".into());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    stop_bridge(&mut bridge);
                    return Err(format!("accept: {e}"));
                }
            }
        };
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
        Ok(Session { stream, bridge, buf: Vec::new(), _guard: guard })
    }

    /// Sends one request and returns the first reply that starts with `want` (other traffic,
    /// such as log packets, is skipped). 4 s timeout.
    pub fn request(&mut self, payload: &[u8], want: &[u8]) -> Result<Vec<u8>, String> {
        self.stream.write_all(&hdlc_encode(payload)).map_err(|e| format!("send: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut chunk = [0u8; 8192];
        loop {
            while let Some(pos) = self.buf.iter().position(|&b| b == 0x7E) {
                let frame: Vec<u8> = self.buf.drain(..=pos).collect();
                if let Some(p) = hdlc_decode(&frame[..frame.len() - 1]) {
                    if p.starts_with(want) {
                        return Ok(p);
                    }
                    // Error replies echo the request after their own code.
                    if matches!(p.first(), Some(0x13..=0x15) | Some(&BAD_SECURITY_MODE))
                        && p.get(1..1 + want.len()) == Some(want)
                    {
                        return Ok(p);
                    }
                }
            }
            if Instant::now() > deadline {
                return Err("no reply".into());
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("bridge closed".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }

    /// NV item contents (128 bytes) and the NV status (0 = done, 5 = never written).
    pub fn nv_read(&mut self, item: u16) -> Result<(u16, Vec<u8>), String> {
        let mut req = vec![NV_READ];
        req.extend_from_slice(&item.to_le_bytes());
        req.extend_from_slice(&[0u8; NV_DATA_LEN + 2]);
        let r = self.request(&req, &[NV_READ])?;
        if r[0] != NV_READ || r.len() < 3 + NV_DATA_LEN + 2 {
            return Err(format!("nv read rejected (0x{:02x})", r[0]));
        }
        let status = u16::from_le_bytes([r[3 + NV_DATA_LEN], r[4 + NV_DATA_LEN]]);
        Ok((status, r[3..3 + NV_DATA_LEN].to_vec()))
    }

    /// Writes all 128 bytes of an NV item. Sends the default SPC once if the modem asks.
    pub fn nv_write(&mut self, item: u16, data: &[u8]) -> Result<(), String> {
        if data.len() != NV_DATA_LEN {
            return Err("nv data must be 128 bytes".into());
        }
        let mut req = vec![NV_WRITE];
        req.extend_from_slice(&item.to_le_bytes());
        req.extend_from_slice(data);
        req.extend_from_slice(&[0, 0]);
        let mut r = self.request(&req, &[NV_WRITE])?;
        if r[0] == BAD_SECURITY_MODE {
            let mut spc = vec![SPC];
            spc.extend_from_slice(b"000000");
            let s = self.request(&spc, &[SPC])?;
            if s.get(1) != Some(&1) {
                return Err("modem refused the service code".into());
            }
            r = self.request(&req, &[NV_WRITE])?;
        }
        if r[0] != NV_WRITE || r.len() < 3 + NV_DATA_LEN + 2 {
            return Err(format!("nv write rejected (0x{:02x})", r[0]));
        }
        match u16::from_le_bytes([r[3 + NV_DATA_LEN], r[4 + NV_DATA_LEN]]) {
            0 => Ok(()),
            s => Err(format!("nv write status {s}")),
        }
    }

    fn efs(&mut self, cmd: u16, body: &[u8]) -> Result<Vec<u8>, String> {
        let mut req = vec![SUBSYS, SUBSYS_FS];
        req.extend_from_slice(&cmd.to_le_bytes());
        let head = req.clone();
        req.extend_from_slice(body);
        let r = self.request(&req, &head)?;
        if !r.starts_with(&head) {
            return Err(format!("efs command rejected (0x{:02x})", r[0]));
        }
        Ok(r)
    }

    /// An EFS item file of exactly `len` bytes. Err("ENOENT") when it does not exist.
    pub fn efs_get(&mut self, path: &str, len: u32) -> Result<Vec<u8>, String> {
        let p = c_path(path);
        let mut body = len.to_le_bytes().to_vec();
        body.extend_from_slice(&(p.len() as u32).to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&p);
        let r = self.efs(EFS2_GET, &body)?;
        if r.len() < 14 {
            return Err("short efs reply".into());
        }
        let n = i32::from_le_bytes(r[4..8].try_into().unwrap());
        let errno = i32::from_le_bytes(r[8..12].try_into().unwrap());
        match (n, errno) {
            (_, 2) => Err("ENOENT".into()),
            (n, 0) if n >= 0 && r.len() >= 14 + n as usize => Ok(r[14..14 + n as usize].to_vec()),
            (_, e) => Err(format!("efs get errno {e}")),
        }
    }

    /// Replaces an EFS item file.
    pub fn efs_put(&mut self, path: &str, data: &[u8]) -> Result<(), String> {
        const O_CREAT: i32 = 0o100;
        let p = c_path(path);
        let mut body = (data.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&O_CREAT.to_le_bytes());
        body.extend_from_slice(&0o777i32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(&p);
        let r = self.efs(EFS2_PUT, &body)?;
        if r.len() < 10 {
            return Err("short efs reply".into());
        }
        match i32::from_le_bytes(r[6..10].try_into().unwrap()) {
            0 => Ok(()),
            e => Err(format!("efs put errno {e}")),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        stop_bridge(&mut self.bridge);
    }
}

fn c_path(path: &str) -> Vec<u8> {
    let mut p = path.as_bytes().to_vec();
    p.push(0);
    p
}

/// `diag_socket_log -k` hands diag back to its previous mode; then reap our child.
fn stop_bridge(child: &mut Child) {
    let _ = Command::new("diag_socket_log")
        .arg("-k")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdlc_round_trips_escaped_bytes() {
        let payload = [0x26, 0x7E, 0x7D, 0x00, 0x14];
        let enc = hdlc_encode(&payload);
        assert_eq!(*enc.last().unwrap(), 0x7E);
        assert!(!enc[..enc.len() - 1].contains(&0x7E));
        assert_eq!(hdlc_decode(&enc[..enc.len() - 1]).unwrap(), payload);
    }

    #[test]
    fn crc_matches_the_frame_seen_from_the_modem() {
        // Version query: 0x00 encodes to 00 78 F0 7E (QPST/libqcdm reference).
        assert_eq!(hdlc_encode(&[0x00]), vec![0x00, 0x78, 0xF0, 0x7E]);
    }

    #[test]
    fn corrupt_frames_are_dropped() {
        let mut enc = hdlc_encode(&[0x26, 0xA0, 0x14]);
        enc[1] ^= 1;
        assert!(hdlc_decode(&enc[..enc.len() - 1]).is_none());
    }
}
