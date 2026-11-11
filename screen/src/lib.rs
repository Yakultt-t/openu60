pub mod actions;
pub mod data;
pub mod display;
pub mod gfx;
pub mod input;
pub mod model;
pub mod pages;
pub mod settings;
pub mod wallpaper;

/// "UTC+08:00" for an offset in minutes.
pub fn clock_label(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}
