//! Network page: radio, live speeds, data usage, clients, battery, remote link.

use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::{bytes, quality, speed, Status};

pub fn draw(c: &mut Canvas, s: &Status, page: usize, pages: usize) {
    super::status_bar(c, s, true);

    // Radio card.
    let (x, y, w) = (14.0, 40.0, 292.0);
    c.glass(x, y, w, 72.0, 18.0);
    c.text(FontId::Digits, &s.net_type, x + 14.0, y + 44.0, 40.0, palette::WHITE, Align::Left);
    let sub = format!("{} · {} · {:.0} MHz", short_carrier(&s.carrier), s.band, s.bandwidth_mhz);
    c.text(FontId::Sans, &sub, x + 14.0, y + 62.0, 11.0, palette::DIM, Align::Left);
    let (q, level) = quality(s.rsrp, s.sinr);
    let qc = [palette::GREEN, palette::MINT, palette::AMBER, palette::RED][level as usize];
    let qw = c.fonts.measure(FontId::Sans, q, 12.0);
    c.circle(x + w - 22.0 - qw, y + 22.0, 3.0, qc);
    c.text(FontId::Sans, q, x + w - 14.0, y + 26.0, 12.0, qc, Align::Right);
    let line = format!("{:.0} dBm · SINR {:.1}", s.rsrp, s.sinr);
    c.text(FontId::Digits, &line, x + w - 14.0, y + 60.0, 14.0, palette::TEXT, Align::Right);
    let tag_x = x + 14.0 + c.fonts.measure(FontId::Digits, &s.net_type, 40.0) + 8.0;
    if s.wifi_uplink {
        c.text(FontId::Sans, &format!("经「{}」上网", crate::model::mode_name(&s.uplink_mode)), tag_x, y + 26.0, 10.0, palette::MINT, Align::Left);
    } else if s.roaming {
        c.text(FontId::Sans, "漫游", tag_x, y + 26.0, 10.0, palette::AMBER, Align::Left);
    }

    // Speeds.
    let y = 120.0;
    c.glass(x, y, w, 104.0, 18.0);
    c.text(FontId::Sans, "↓ 下行", x + 14.0, y + 20.0, 12.0, palette::DIM, Align::Left);
    c.text(FontId::Sans, "↑ 上行", x + w - 14.0, y + 20.0, 12.0, palette::DIM, Align::Right);
    let (dv, du) = speed(s.down_bps, s.speed_bytes);
    let adv = c.text(FontId::Digits, &dv, x + 14.0, y + 56.0, 34.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, du, x + 18.0 + adv, y + 56.0, 11.0, palette::DIM, Align::Left);
    let (uv, uu) = speed(s.up_bps, s.speed_bytes);
    let uw = c.fonts.measure(FontId::Sans, uu, 11.0);
    c.text(FontId::Sans, uu, x + w - 14.0, y + 56.0, 11.0, palette::DIM, Align::Right);
    c.text(FontId::Digits, &uv, x + w - 18.0 - uw, y + 56.0, 30.0, palette::WHITE, Align::Right);
    let top = s.down_hist.iter().chain(s.up_hist.iter()).cloned().fold(0.0, f32::max).max(1.0) * 1.15;
    c.sparkline(x + 12.0, y + 64.0, w - 24.0, 30.0, &s.up_hist, top, palette::ORANGE, false);
    c.sparkline(x + 12.0, y + 64.0, w - 24.0, 30.0, &s.down_hist, top, palette::SKY, true);

    // Data today / clients.
    let y = 232.0;
    let hw = (w - 8.0) / 2.0;
    c.glass(x, y, hw, 76.0, 18.0);
    c.text(FontId::Sans, "今日流量", x + 12.0, y + 20.0, 12.0, palette::DIM, Align::Left);
    let (tv, tu) = bytes(s.today_bytes);
    let a = c.text(FontId::Digits, &tv, x + 12.0, y + 50.0, 28.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, tu, x + 15.0 + a, y + 50.0, 11.0, palette::DIM, Align::Left);
    let (mv, mu) = bytes(s.month_bytes);
    let plan = match s.plan_bytes {
        Some(p) => { let (pv, pu) = bytes(p); format!("本月 {mv} {mu} · 共 {pv} {pu}") }
        None => format!("本月 {mv} {mu} · 不限量"),
    };
    c.text(FontId::Sans, &plan, x + 12.0, y + 66.0, 10.0, palette::DIM, Align::Left);

    let x2 = x + hw + 8.0;
    c.glass(x2, y, hw, 76.0, 18.0);
    c.text(FontId::Sans, "已连接", x2 + 12.0, y + 20.0, 12.0, palette::DIM, Align::Left);
    let a = c.text(FontId::Digits, &s.clients.to_string(), x2 + 12.0, y + 50.0, 28.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, "台", x2 + 15.0 + a, y + 50.0, 11.0, palette::DIM, Align::Left);
    let band = if s.clients == 0 { "暂无设备".to_string() } else { format!("都在 {}", s.clients_band) };
    c.text(FontId::Sans, &band, x2 + 12.0, y + 66.0, 10.0, palette::DIM, Align::Left);

    // Battery.
    let y = 316.0;
    c.glass(x, y, w, 76.0, 18.0);
    c.text(FontId::Sans, "电池", x + 14.0, y + 20.0, 12.0, palette::DIM, Align::Left);
    let a = c.text(FontId::Digits, &s.battery.to_string(), x + 14.0, y + 52.0, 32.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, "%", x + 17.0 + a, y + 52.0, 11.0, palette::DIM, Align::Left);
    let state = if s.charging { "正在充电" } else { "未接电源" };
    c.text(FontId::Sans, state, x + 14.0, y + 68.0, 11.0, palette::DIM, Align::Left);
    if let Some(hrs) = s.battery_hours {
        c.text(FontId::Sans, &format!("约可用 {hrs:.0} 小时"), x + w - 14.0, y + 20.0, 11.0, palette::DIM, Align::Right);
    }
    let col = if s.battery <= 15 { palette::RED } else if s.battery <= 30 { palette::AMBER } else { palette::GREEN };
    c.level_bar(x + 92.0, y + 38.0, w - 106.0, 12.0, s.battery as f32 / 100.0, col);

    // Remote link (new): how the phones reach this router.
    let y = 400.0;
    c.glass(x, y, w, 44.0, 16.0);
    let dot = if s.remote_mode.is_empty() { palette::FAINT } else { palette::GREEN };
    c.circle(x + 18.0, y + 22.0, 4.0, dot);
    c.text(FontId::Sans, "U60 助手", x + 30.0, y + 27.0, 13.0, palette::TEXT, Align::Left);
    let right = if s.paired_phones == 0 {
        "还没有配对手机".to_string()
    } else {
        format!("{} 台手机 · {}", s.paired_phones, if s.remote_mode.is_empty() { "待连接" } else { &s.remote_mode })
    };
    c.text(FontId::Sans, &right, x + w - 14.0, y + 27.0, 12.0, palette::DIM, Align::Right);

    c.page_dots(160.0, 462.0, pages, page);
    let _ = Rgba::rgb(0, 0, 0);
}

fn short_carrier(c: &str) -> &str {
    c.strip_prefix("中国").unwrap_or(c)
}
