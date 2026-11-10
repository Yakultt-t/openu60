pub mod adblock;
pub mod call;
pub mod charge;
pub mod cover;
pub mod more;
pub mod network;
pub mod pair;
pub mod wifi;

use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Status;

/// Something a tap on the screen asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Brightness(i32),
    Timeout(u32),
    StockUi,
    PowerMenu,
    /// Switch to "hotspot", "repeater" or "client".
    SetMode(String),
    RelayScan,
    RelayJoin(String, bool),
    ChargeLimitToggle,
    ChargeLimit(i32),
    PowerbankToggle,
    Hangup,
    AdblockToggle,
    /// Minutes; 0 means "until 7 tomorrow morning".
    AdblockPause(u32),
    AdblockResume,
    Open(Sub),
    Back,
    Theme(String),
    ToggleWakeCover,
    ToggleBatteryPct,
    ToggleSpeedUnit,
    Reboot,
    PowerOff,
    AdblockUpdate,
    DismissMissed,
}

pub type Hits = Vec<(Action, [f32; 4])>;

/// Sub-pages of the settings page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sub {
    Appearance,
    Display,
    Advanced,
}

/// "‹ 设置" back link at the top left of a sub-page.
pub fn back_header(c: &mut Canvas, title: &str, hits: &mut Hits) {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(26.0, 52.0);
    pb.line_to(19.0, 59.0);
    pb.line_to(26.0, 66.0);
    if let Some(p) = pb.finish() {
        c.stroke_path(&p, palette::SKY, 2.2);
    }
    c.text(FontId::Sans, title, 38.0, 66.0, 20.0, palette::WHITE, Align::Left);
    hits.push((Action::Back, [0.0, 36.0, 200.0, 44.0]));
}

/// Small tile with an icon square, a title and a note; `on` adds a switch.
pub fn tile(c: &mut Canvas, x: f32, y: f32, w: f32, h: f32, icon: (&str, Rgba), title: &str, note: &str, on: Option<bool>) -> [f32; 4] {
    c.glass(x, y, w, h, 16.0);
    let mut tx = x + 12.0;
    if !icon.0.is_empty() {
        c.rrect(x + 10.0, y + (h - 30.0) / 2.0, 30.0, 30.0, 9.0, icon.1);
        c.text(FontId::Sans, icon.0, x + 25.0, y + h / 2.0 + 5.0, 14.0, palette::WHITE, Align::Center);
        tx = x + 48.0;
    }
    let (ty, has_note) = if note.is_empty() { (y + h / 2.0 + 5.0, false) } else { (y + h / 2.0 - 1.0, true) };
    c.text(FontId::Sans, title, tx, ty, 13.0, palette::TEXT, Align::Left);
    if has_note {
        c.text(FontId::Sans, note, tx, ty + 16.0, 10.0, palette::DIM, Align::Left);
    }
    if let Some(v) = on {
        let sx = x + w - 44.0;
        c.rrect(sx, y + (h - 22.0) / 2.0, 36.0, 22.0, 11.0, if v { palette::BLUE } else { Rgba(255, 255, 255, 50) });
        c.circle(if v { sx + 25.0 } else { sx + 11.0 }, y + h / 2.0, 9.0, palette::WHITE);
    }
    [x, y, w, h]
}

pub fn chevron(c: &mut Canvas, x: f32, y: f32) {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(x - 4.0, y - 6.0);
    pb.line_to(x + 2.0, y);
    pb.line_to(x - 4.0, y + 6.0);
    if let Some(p) = pb.finish() {
        c.stroke_path(&p, palette::DIM, 1.8);
    }
}

/// Page titles in swipe order (the cover comes before them).
pub const TITLES: [&str; 6] = ["网络", "上网方式", "充电", "广告拦截", "连接与配对", "设置"];

pub fn header(c: &mut Canvas, title: &str) {
    c.text(FontId::Sans, title, 18.0, 66.0, 22.0, palette::WHITE, Align::Left);
}

/// Card row with a label, an optional note and a switch on the right; returns its hit box.
pub fn switch_row(c: &mut Canvas, x: f32, y: f32, w: f32, label: &str, note: &str, on: bool) -> [f32; 4] {
    let h = if note.is_empty() { 52.0 } else { 62.0 };
    c.glass(x, y, w, h, 16.0);
    let ty = if note.is_empty() { y + 32.0 } else { y + 27.0 };
    c.text(FontId::Sans, label, x + 14.0, ty, 14.0, palette::TEXT, Align::Left);
    if !note.is_empty() {
        c.text(FontId::Sans, note, x + 14.0, y + 47.0, 11.0, palette::DIM, Align::Left);
    }
    c.toggle(x + w - 58.0, y + (h - 26.0) / 2.0, on);
    [x, y, w, h]
}

/// Top bar. `clock` shows the time on the left (inner pages); the cover shows the carrier
/// there instead because its big clock is right below.
pub fn status_bar(c: &mut Canvas, s: &Status, clock: bool) {
    let y = 24.0;
    if clock {
        c.text(FontId::Digits, &format!("{:02}:{:02}", s.hour, s.minute), 18.0, y + 2.0, 19.0, palette::TEXT, Align::Left);
    } else {
        signal_bars(c, 18.0, y - 11.0, s.bars);
        let w = c.text(FontId::Sans, &format!("{} {}", s.carrier, s.net_type), 42.0, y, 13.0, palette::TEXT, Align::Left);
        if s.wifi_uplink {
            let label = if s.uplink_mode == "client" { "Wi-Fi" } else { "中继" };
            wifi_bars(c, 52.0 + w, y - 1.0, s.wifi_uplink_signal);
            c.text(FontId::Sans, label, 62.0 + w, y, 11.0, palette::MINT, Align::Left);
        }
    }
    let mut x = 302.0;
    battery_icon(c, x - 24.0, y - 10.0, s.battery, s.charging);
    x -= 30.0;
    if clock && s.battery_pct {
        x -= c.text(FontId::Digits, &format!("{}%", s.battery), x, y + 1.0, 13.0, palette::TEXT, Align::Right) + 8.0;
    }
    if clock {
        signal_bars(c, x - 16.0, y - 11.0, s.bars);
        x -= 20.0;
        x -= c.text(FontId::Digits, &s.net_type, x, y + 1.0, 14.0, palette::TEXT, Align::Right) + 4.0;
        if s.roaming && !s.wifi_uplink {
            x -= c.text(FontId::Sans, "R", x, y, 10.0, palette::AMBER, Align::Right) + 6.0;
        }
        uplink_icon(c, x, y, s);
    } else {
        c.text(FontId::Digits, &format!("{}%", s.battery), x, y + 1.0, 14.0, palette::TEXT, Align::Right);
    }
}

/// Wi-Fi fan plus "中继" / "Wi-Fi" right-aligned at `x` when the internet comes from Wi-Fi.
fn uplink_icon(c: &mut Canvas, x: f32, y: f32, s: &Status) {
    if !s.wifi_uplink {
        return;
    }
    let label = if s.uplink_mode == "client" { "Wi-Fi" } else { "中继" };
    let w = c.text(FontId::Sans, label, x, y, 10.0, palette::MINT, Align::Right);
    wifi_bars(c, x - w - 10.0, y - 1.0, s.wifi_uplink_signal);
}

pub fn signal_bars(c: &mut Canvas, x: f32, y: f32, bars: u8) {
    for i in 0..4u8 {
        let h = 4.0 + i as f32 * 2.5;
        let col = if i < bars { palette::TEXT } else { Rgba(255, 255, 255, 70) };
        c.rrect(x + i as f32 * 4.0, y + 11.0 - h, 3.0, h, 1.0, col);
    }
}

pub fn wifi_bars(c: &mut Canvas, x: f32, y: f32, dbm: i32) {
    let bars = if dbm >= -55 { 3 } else if dbm >= -67 { 2 } else if dbm >= -78 { 1 } else { 0 };
    for i in 0..3 {
        let r = 4.0 + i as f32 * 4.0;
        let col = if i < bars.max(1) { palette::TEXT } else { Rgba(255, 255, 255, 60) };
        let mut pb = tiny_skia::PathBuilder::new();
        let steps = 10;
        for k in 0..=steps {
            let a = -std::f32::consts::FRAC_PI_4 * 3.0 + std::f32::consts::FRAC_PI_2 * k as f32 / steps as f32;
            let (px, py) = (x + r * a.cos(), y + r * a.sin());
            if k == 0 { pb.move_to(px, py) } else { pb.line_to(px, py) }
        }
        if let Some(p) = pb.finish() {
            c.stroke_path(&p, col, 1.8);
        }
    }
    c.circle(x, y, 1.6, palette::TEXT);
}

pub fn battery_icon(c: &mut Canvas, x: f32, y: f32, pct: u8, charging: bool) {
    c.stroke_path(&crate::gfx::rounded_rect(x + 0.5, y + 0.5, 21.0, 11.0, 3.0), Rgba(255, 255, 255, 150), 1.0);
    c.rrect(x + 22.5, y + 3.5, 2.0, 4.0, 1.0, Rgba(255, 255, 255, 150));
    let fill = if pct <= 15 { palette::RED } else if charging { palette::GREEN } else { palette::TEXT };
    let w = 17.0 * pct.min(100) as f32 / 100.0;
    if w > 0.5 {
        c.rrect(x + 2.5, y + 2.5, w, 7.0, 1.5, fill);
    }
}

pub fn button(c: &mut Canvas, x: f32, y: f32, w: f32, h: f32, label: &str, fill: Rgba) -> [f32; 4] {
    c.rrect(x, y, w, h, 12.0, fill);
    c.text(FontId::Sans, label, x + w / 2.0, y + h / 2.0 + 5.0, 13.0, palette::WHITE, Align::Center);
    [x, y, w, h]
}
