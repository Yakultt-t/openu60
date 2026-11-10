//! Keeps the OpenU60 touchscreen watcher (`openu60-screen watch`) running next to the stock
//! UI when it is installed. The watcher itself holds a lock, so a second copy (for example
//! after an agent restart while the old one still runs) exits at once.

use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = "/data/openu60/screen/openu60-screen";

pub fn start() {
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
