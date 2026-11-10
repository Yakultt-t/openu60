use std::process::Command;
use std::sync::{Mutex, MutexGuard};

const PORTS: &[&str] = &[
    // at_mdm0/at_mdm1 are shared with port-bridge / port-bridge-at, which steal ~1 in 4
    // replies; at_mdm2 has no other reader on this firmware.
    "/dev/at_mdm2",
    "/dev/at_mdm0",
    "/dev/at_mdm1",
    "/dev/at_usb0",
    "/dev/smd7",
    "/dev/smd11",
];

/// Serialises every AT exchange process-wide. The modem node is a packet
/// device with a single receive queue shared by all opens, so two concurrent
/// `cat` readers split (or steal) each other's reply; HTTP workers, the AT
/// terminal, the policy threads and `busy_check` all go through here.
static AT_LOCK: Mutex<()> = Mutex::new(());

fn lock_port() -> MutexGuard<'static, ()> {
    AT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct AtPort {
    cached: Mutex<Option<String>>,
}

impl AtPort {
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    pub fn detect(&self) -> Option<String> {
        if let Some(port) = self.cached() {
            return Some(port);
        }
        let _g = lock_port();
        self.probe()
    }

    fn cached(&self) -> Option<String> {
        self.cached.lock().unwrap().clone()
    }

    /// Probe the candidate ports with a bare `AT`. Caller must hold `AT_LOCK`.
    fn probe(&self) -> Option<String> {
        if let Some(port) = self.cached() {
            return Some(port);
        }

        for &port in PORTS {
            if !std::path::Path::new(port).exists() {
                continue;
            }
            let script = format!(
                "cat {p} & PID=$! ; sleep 0.3 ; echo -e 'AT\\r' > {p} ; sleep 1 ; kill $PID 2>/dev/null",
                p = port
            );
            let output = Command::new("sh").args(["-c", &script]).output().ok();
            if let Some(out) = output {
                let resp = String::from_utf8_lossy(&out.stdout);
                if resp.contains("OK") {
                    let mut cached = self.cached.lock().unwrap();
                    *cached = Some(port.to_string());
                    return Some(port.to_string());
                }
            }
        }
        None
    }
}

/// Send an AT command and return the raw response text.
///
/// Holds `AT_LOCK` for the whole exchange (port probe included) so no two
/// commands ever share the node's receive queue.
pub fn send(at_port: &AtPort, command: &str, timeout_secs: u64) -> Result<String, String> {
    let _g = lock_port();
    let port = at_port.probe().ok_or("no serial port found")?;
    // Queries (AT+X? / AT+X) can be re-asked safely; dial/answer/hang-up must not be duplicated.
    let upper = command.to_ascii_uppercase();
    let idempotent = !(upper.starts_with("ATD") || upper == "ATA" || upper.starts_with("AT+CHUP") || upper.starts_with("AT+VTS") || upper.starts_with("AT+CMGS"));
    let first = if idempotent { send_direct_ms(&port, command, 600) } else { send_direct(&port, command, timeout_secs) };
    match first {
        // Occasionally only the echo comes back (the reply went to the port bridge); ask again.
        Ok(out) if idempotent && !is_final(out.as_bytes()) => {
            std::thread::sleep(std::time::Duration::from_millis(50));
            send_direct(&port, command, timeout_secs)
        }
        Ok(out) => Ok(out),
        Err(e) => {
            eprintln!("[at] direct I/O failed ({e}); falling back to shell");
            send_shell(&port, command, timeout_secs)
        }
    }
}

/// Final result codes that end an AT exchange (3GPP 27.007 / V.250).
fn is_final(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    text.lines().rev().take(3).any(|l| {
        let l = l.trim();
        l == "OK"
            || l == "ERROR"
            || l.starts_with("+CME ERROR")
            || l.starts_with("+CMS ERROR")
            || l == "NO CARRIER"
            || l == "BUSY"
            || l == "NO ANSWER"
            || l == "NO DIALTONE"
            || l == "CONNECT"
    })
}

/// Talk to the modem port directly and return as soon as the final result code arrives
/// (the old shell pipeline always slept for the full timeout, ~3 s per command, which made
/// call state changes and hang-ups feel sluggish).
/// Persistent blocking reader on the modem port. The glink packet driver only delivers
/// data to a reader that is blocked in read() (that call queues the rx intent), so one
/// long-lived reader thread collects everything and `send_direct` waits on its buffer.
struct PortReader {
    buf: std::sync::Mutex<Vec<u8>>,
    cv: std::sync::Condvar,
    /// Long-lived blocking write descriptor (reopening per command loses replies).
    wfd: std::sync::Mutex<Option<i32>>,
}

static READER: std::sync::OnceLock<std::sync::Arc<PortReader>> = std::sync::OnceLock::new();

fn reader_for(port: &str) -> std::sync::Arc<PortReader> {
    READER
        .get_or_init(|| {
            let reader = std::sync::Arc::new(PortReader { buf: std::sync::Mutex::new(Vec::new()), cv: std::sync::Condvar::new(), wfd: std::sync::Mutex::new(None) });
            let r = std::sync::Arc::clone(&reader);
            let path = port.to_string();
            std::thread::Builder::new()
                .name("at-reader".into())
                .spawn(move || {
                    use std::ffi::CString;
                    let cpath = CString::new(path.clone()).unwrap_or_default();
                    loop {
                        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NOCTTY) };
                        if fd < 0 {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            continue;
                        }
                        let mut buf = [0u8; 4096];
                        loop {
                            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                            if n < 0 {
                                let err = std::io::Error::last_os_error();
                                if err.raw_os_error() == Some(libc::EINTR) { continue; }
                                break;
                            }
                            if n == 0 { break; }
                            let mut g = r.buf.lock().unwrap_or_else(|e| e.into_inner());
                            g.extend_from_slice(&buf[..n as usize]);
                            if g.len() > 65536 { let cut = g.len() - 32768; g.drain(..cut); }
                            r.cv.notify_all();
                        }
                        unsafe { libc::close(fd) };
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                })
                .ok();
            reader
        })
        .clone()
}

fn send_direct(port: &str, command: &str, timeout_secs: u64) -> Result<String, String> {
    send_direct_ms(port, command, timeout_secs.max(1) * 1000)
}

fn send_direct_ms(port: &str, command: &str, timeout_ms: u64) -> Result<String, String> {
    use std::ffi::CString;
    use std::time::{Duration, Instant};

    let reader = reader_for(port);
    // Discard anything unsolicited that arrived before this command.
    reader.buf.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let cpath = CString::new(port).map_err(|_| "bad port path".to_string())?;
    let line = format!("{command}\r");
    {
        let mut wslot = reader.wfd.lock().unwrap_or_else(|e| e.into_inner());
        if wslot.is_none() {
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_WRONLY | libc::O_NOCTTY) };
            if fd < 0 {
                return Err(format!("open(w) {port}: {}", std::io::Error::last_os_error()));
            }
            *wslot = Some(fd);
        }
        let fd = wslot.unwrap_or(-1);
        let written = unsafe { libc::write(fd, line.as_ptr() as *const libc::c_void, line.len()) };
        if written < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            *wslot = None;
            return Err(format!("write {port}: {e}"));
        }
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(100));
    let mut guard = reader.buf.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if is_final(&guard) {
            // Let the rest of the same burst land (e.g. a trailing newline), then take it.
            let (g, _) = reader.cv.wait_timeout(guard, Duration::from_millis(20)).unwrap_or_else(|e| e.into_inner());
            guard = g;
            let out = String::from_utf8_lossy(&guard).to_string();
            guard.clear();
            return Ok(out);
        }
        let now = Instant::now();
        if now >= deadline {
            let out = String::from_utf8_lossy(&guard).to_string();
            guard.clear();
            return Ok(out);
        }
        let (g, _) = reader.cv.wait_timeout(guard, deadline - now).unwrap_or_else(|e| e.into_inner());
        guard = g;
    }
}

/// Original implementation, kept as a fallback for ports that reject direct I/O.
fn send_shell(port: &str, command: &str, timeout_secs: u64) -> Result<String, String> {
    let script = format!(
        "cat {p} & PID=$! ; sleep 0.3 ; echo -e '{cmd}\\r' > {p} ; sleep {t} ; kill $PID 2>/dev/null",
        p = port,
        cmd = command,
        t = timeout_secs
    );
    let output = Command::new("sh")
        .args(["-c", &script])
        .output()
        .map_err(|e| format!("failed to open port: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Tests run in parallel and another test may be inside `send` for a
    /// few microseconds, so poll rather than demand the lock is free *now*.
    fn lock_is_free() -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if AT_LOCK.try_lock().is_ok() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// On a dev box none of `PORTS` exist, so `send` fails in `probe()` via
    /// the `?` early return; the lock guard must still be dropped so the next
    /// exchange (and a competing thread) can proceed.
    #[test]
    fn send_releases_lock_on_early_return() {
        let port = AtPort::new();
        assert!(send(&port, "AT", 1).is_err());
        assert!(lock_is_free(), "AT_LOCK still held after send");
        assert!(send(&port, "AT", 1).is_err());
        assert!(port.detect().is_none());
        assert!(lock_is_free(), "AT_LOCK still held after detect");
    }

    #[test]
    fn concurrent_senders_do_not_deadlock() {
        let port = std::sync::Arc::new(AtPort::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let port = std::sync::Arc::clone(&port);
                std::thread::spawn(move || send(&port, "AT", 1).is_err())
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap());
        }
        assert!(lock_is_free());
    }
}
