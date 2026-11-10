//! Cover / lock screen: date, big clock, three rings, live speed card, alerts, "滑动进入".

use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::{bytes, speed, weekday_cn, Status};

pub fn draw(c: &mut Canvas, s: &Status) {
    super::status_bar(c, s, false);
    c.text(FontId::Sans, &format!("{}月{}日  {}", s.month, s.day, weekday_cn(s.weekday)), 160.0, 62.0, 15.0, palette::TEXT.alpha(0.85), Align::Center);
    c.text(FontId::Digits, &format!("{:02}:{:02}", s.hour, s.minute), 160.0, 152.0, 92.0, palette::WHITE, Align::Center);

    // Rings: memory, network, monthly data.
    let cy = 214.0;
    let r = 25.0;
    c.ring(58.0, cy, r, 4.0, s.memory_pct / 100.0, palette::PURPLE, Rgba::rgb(120, 140, 255));
    c.text(FontId::Digits, &format!("{:.0}", s.memory_pct), 58.0, cy + 7.0, 19.0, palette::WHITE, Align::Center);
    c.text(FontId::Sans, "内存", 58.0, cy + r + 22.0, 12.0, palette::DIM, Align::Center);

    let sig = (s.bars as f32 / 4.0).max(0.08);
    c.ring(160.0, cy, r, 4.0, sig, palette::MINT, Rgba::rgb(40, 170, 220));
    c.text(FontId::Digits, &s.net_type, 160.0, cy + 7.0, 19.0, palette::WHITE, Align::Center);
    c.text(FontId::Sans, &format!("{} {}", short_carrier(&s.carrier), s.band), 160.0, cy + r + 22.0, 12.0, palette::DIM, Align::Center);

    let month_frac = s.plan_bytes.map(|p| (s.month_bytes / p) as f32).unwrap_or(0.3);
    c.ring(262.0, cy, r, 4.0, month_frac, palette::SKY, palette::BLUE);
    let (mv, mu) = bytes(s.month_bytes);
    c.text(FontId::Digits, &mv, 262.0, cy + 4.0, 16.0, palette::WHITE, Align::Center);
    c.text(FontId::Sans, mu, 262.0, cy + 15.0, 9.0, palette::DIM, Align::Center);
    c.text(FontId::Sans, "本月流量", 262.0, cy + r + 22.0, 12.0, palette::DIM, Align::Center);

    // Speed card.
    let (x, y, w, h) = (14.0, 276.0, 292.0, 104.0);
    c.glass(x, y, w, h, 18.0);
    c.text(FontId::Sans, "↓ 下行", x + 14.0, y + 22.0, 12.0, palette::DIM, Align::Left);
    let (dv, du) = speed(s.down_bps, s.speed_bytes);
    let adv = c.text(FontId::Digits, &dv, x + 14.0, y + 62.0, 40.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, du, x + 18.0 + adv, y + 62.0, 11.0, palette::DIM, Align::Left);
    c.text(FontId::Sans, "↑ 上行", x + w - 14.0, y + 30.0, 12.0, palette::DIM, Align::Right);
    let (uv, uu) = speed(s.up_bps, s.speed_bytes);
    let uw = c.fonts.measure(FontId::Sans, uu, 11.0);
    c.text(FontId::Sans, uu, x + w - 14.0, y + 62.0, 11.0, palette::DIM, Align::Right);
    c.text(FontId::Digits, &uv, x + w - 18.0 - uw, y + 62.0, 28.0, palette::AMBER, Align::Right);
    c.sparkline(x + 14.0, y + 70.0, w - 28.0, 24.0, &s.down_hist, 0.0, palette::SKY, true);

    // Alerts from the phone line (new): unread SMS and missed calls.
    let mut chips: Vec<(String, Rgba)> = Vec::new();
    if s.unread_sms > 0 {
        chips.push((format!("{} 条新短信", s.unread_sms), palette::SKY));
    }
    if !s.missed.is_empty() {
        chips.push((format!("{} 个未接来电", s.missed.len()), palette::RED));
    }
    if !chips.is_empty() {
        let gap = 8.0;
        let widths: Vec<f32> = chips.iter().map(|(t, _)| c.fonts.measure(FontId::Sans, t, 12.0) + 22.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + gap * (chips.len() as f32 - 1.0);
        let mut cx = 160.0 - total / 2.0;
        for ((t, col), w) in chips.iter().zip(widths) {
            c.glass(cx, 392.0, w, 26.0, 13.0);
            c.circle(cx + 10.0, 405.0, 3.0, *col);
            c.text(FontId::Sans, t, cx + 16.0, 409.0, 12.0, palette::TEXT, Align::Left);
            cx += w + gap;
        }
    }

    // "滑动进入" pill.
    let pw = 96.0;
    c.rrect(160.0 - pw / 2.0, 438.0, pw, 26.0, 13.0, Rgba(0, 0, 0, 90));
    c.stroke_path(&crate::gfx::rounded_rect(160.0 - pw / 2.0 + 0.5, 438.5, pw - 1.0, 25.0, 13.0), Rgba(255, 255, 255, 40), 1.0);
    // Chevron drawn, not a glyph: the system font has no "⌃".
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(129.0, 455.0);
    pb.line_to(134.0, 449.0);
    pb.line_to(139.0, 455.0);
    if let Some(p) = pb.finish() {
        c.stroke_path(&p, palette::TEXT, 1.8);
    }
    c.text(FontId::Sans, "滑动进入", 146.0, 456.0, 12.0, palette::TEXT, Align::Left);
}

fn short_carrier(c: &str) -> &str {
    c.strip_prefix("中国").unwrap_or(c)
}
