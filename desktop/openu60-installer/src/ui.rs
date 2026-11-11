//! Plain console output and prompts (no colours, so it reads the same in every Windows console).

use std::io::{self, BufRead, Write};

pub fn step(n: usize, total: usize, title: &str) {
    println!("\n[{n}/{total}] {title}");
}
pub fn info(s: &str) {
    println!("    {s}");
}
pub fn ok(s: &str) {
    println!("  √ {s}");
}
pub fn warn(s: &str) {
    println!("  ! {s}");
}
pub fn fail(s: &str) {
    println!("\n  × {s}");
}

pub fn prompt(q: &str) -> String {
    print!("{q}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    line.trim().to_string()
}

pub fn prompt_default(q: &str, default: &str) -> String {
    let a = prompt(&format!("{q}（直接回车 = {default}）："));
    if a.is_empty() { default.to_string() } else { a }
}

/// Yes/no question; `default` is what a bare Enter means.
pub fn confirm(q: &str, default: bool) -> bool {
    let hint = if default { "Y/n" } else { "y/N" };
    let a = prompt(&format!("{q} [{hint}]：")).to_lowercase();
    match a.as_str() {
        "" => default,
        "y" | "yes" | "是" | "好" => true,
        _ => false,
    }
}

pub fn secret(q: &str) -> String {
    rpassword::prompt_password(q).unwrap_or_default()
}

pub fn progress(done: u64, total: u64, started: std::time::Instant) {
    let secs = started.elapsed().as_secs_f64().max(0.1);
    let speed = done as f64 / secs;
    let eta = if speed > 0.0 { (total.saturating_sub(done)) as f64 / speed } else { 0.0 };
    print!(
        "\r    {:.2} / {:.2} GB  {:.1} MB/s  还要约 {} 分钟     ",
        done as f64 / 1e9,
        total as f64 / 1e9,
        speed / 1e6,
        (eta / 60.0).ceil() as u64
    );
    let _ = io::stdout().flush();
}
