//! Minimal `log` backends: stderr for the daemons, a callback for the iOS library.

use std::sync::OnceLock;

use log::{LevelFilter, Log, Metadata, Record};

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }
    fn log(&self, r: &Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {} {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static CALLBACK: OnceLock<Box<dyn Fn(log::Level, String) + Send + Sync>> = OnceLock::new();

struct Callback;

impl Log for Callback {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }
    fn log(&self, r: &Record) {
        if let Some(cb) = CALLBACK.get() {
            cb(r.level(), format!("{} {}", r.target(), r.args()));
        }
    }
    fn flush(&self) {}
}

/// Prints tunnel logs to stderr; a no-op if a logger is already installed.
pub fn init_stderr(level: LevelFilter) {
    if log::set_logger(&Stderr).is_ok() {
        log::set_max_level(level);
    }
}

pub fn init_callback(level: LevelFilter, cb: impl Fn(log::Level, String) + Send + Sync + 'static) {
    let _ = CALLBACK.set(Box::new(cb));
    if log::set_logger(&Callback).is_ok() {
        log::set_max_level(level);
    }
}
