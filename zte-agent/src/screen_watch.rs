//! Keeps the OpenU60 touchscreen watcher (`openu60-screen watch`) running next to the stock
//! UI when it is installed. The watcher itself holds a lock, so a second copy (for example
//! after an agent restart while the old one still runs) exits at once.

use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = "/data/openu60/screen/openu60-screen";

const REOPEN_MARKER: &str = "/data/openu60/update/reopen-screen";
const OPEN_REQUEST: &str = "/tmp/openu60-screen-open";

/// Processes whose command line is `<BIN> <arg>`.
fn screen_pids(arg: &str) -> Vec<i32> {
    let want = format!("{BIN}\0{arg}\0");
    std::fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let pid: i32 = e.file_name().to_str()?.parse().ok()?;
                    let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
                    (cmd == want.as_bytes()).then_some(pid)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An update replaced the screen program: stop the old one (the stock UI comes back) and,
/// if ours was on screen, reopen the new one once the next agent's watcher is up.
pub fn restart_for_update() {
    let running = screen_pids("run");
    if !running.is_empty() {
        let _ = std::fs::write(REOPEN_MARKER, b"1");
    }
    for pid in running {
        // SAFETY: plain kill(2) on a pid we just read.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    std::thread::sleep(Duration::from_secs(2));
    for pid in screen_pids("watch") {
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

pub fn start() {
    if std::path::Path::new(REOPEN_MARKER).exists() {
        crate::system::spawn_named("screen-reopen", || {
            // The watcher below starts after 30 s; it opens on request once it runs.
            std::thread::sleep(Duration::from_secs(45));
            let _ = std::fs::write(OPEN_REQUEST, b"");
            let _ = std::fs::remove_file(REOPEN_MARKER);
        });
    }
    crate::system::spawn_named("screen-watch", || {
        // Let the stock UI finish booting first.
        std::thread::sleep(Duration::from_secs(30));
        loop {
            if !std::path::Path::new(BIN).exists() {
                std::thread::sleep(Duration::from_secs(60));
                continue;
            }
            let started = Instant::now();
            match Command::new(BIN).arg("watch").status() {
                Ok(s) => eprintln!("[screen] watcher exited ({s})"),
                Err(e) => eprintln!("[screen] watcher failed to start: {e}"),
            }
            // A watcher that exits at once (lock held, crash) is retried slowly.
            let pause = if started.elapsed() < Duration::from_secs(10) { 60 } else { 5 };
            std::thread::sleep(Duration::from_secs(pause));
        }
    });
}
