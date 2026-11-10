//! Ad blocking: 24-hour counters and chart, the switch, pause buttons, top blocked domains.

use super::{button, header, switch_row, Action, Hits};
use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Status;

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn draw(c: &mut Canvas, s: &Status, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    header(c, "广告拦截");
    let a = &s.adblock;
    let mut hits = Hits::new();
    let (x, w) = (14.0, 292.0);

    // Summary card with the 24 h chart.
    let y = 80.0;
    c.glass(x, y, w, 142.0, 18.0);
    c.text(FontId::Sans, "24 小时拦截", x + 14.0, y + 20.0, 11.0, palette::DIM, Align::Left);
    let adv = c.text(FontId::Digits, &thousands(a.blocked), x + 14.0, y + 58.0, 38.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, "次", x + 18.0 + adv, y + 58.0, 12.0, palette::DIM, Align::Left);
    let (state, col) = if !a.loaded {
        ("读取中", palette::FAINT)
    } else if !a.enabled {
        ("已关闭", palette::FAINT)
    } else if a.paused_until > 0 {
        ("已暂停", palette::AMBER)
    } else if a.active {
        ("拦截中", palette::GREEN)
    } else if a.updating || a.domains == 0 {
        ("下载规则中", palette::AMBER)
    } else {
        ("准备中", palette::AMBER)
    };
    let sw = c.fonts.measure(FontId::Sans, state, 12.0);
    c.circle(x + w - 22.0 - sw, y + 18.0, 3.0, col);
    c.text(FontId::Sans, state, x + w - 14.0, y + 22.0, 12.0, col, Align::Right);
    c.text(FontId::Sans, &format!("{} 台设备在保护中", a.devices), x + w - 14.0, y + 40.0, 10.0, palette::DIM, Align::Right);
    // Legend + chart.
    c.rrect(x + 14.0, y + 70.0, 10.0, 3.0, 1.5, palette::SKY);
    c.text(FontId::Sans, "查询", x + 28.0, y + 75.0, 9.0, palette::DIM, Align::Left);
    c.rrect(x + 58.0, y + 70.0, 10.0, 3.0, 1.5, palette::GREEN);
    c.text(FontId::Sans, "拦截", x + 72.0, y + 75.0, 9.0, palette::DIM, Align::Left);
    let top = a.hourly_queries.iter().cloned().fold(1.0, f32::max) * 1.1;
    c.sparkline(x + 12.0, y + 80.0, w - 24.0, 30.0, &a.hourly_queries, top, palette::SKY, false);
    c.sparkline(x + 12.0, y + 80.0, w - 24.0, 30.0, &a.hourly_blocked, top, palette::GREEN, false);
    let col3 = (w - 28.0) / 3.0;
    let stats = [
        ("查询", thousands(a.queries)),
        ("拦截率", format!("{:.1}%", a.rate * 100.0)),
        ("规则", if a.domains >= 10_000 { format!("{:.1} 万", a.domains as f32 / 10_000.0) } else { a.domains.to_string() }),
    ];
    for (i, (k, v)) in stats.iter().enumerate() {
        let sx = x + 14.0 + col3 * i as f32;
        c.text(FontId::Sans, k, sx, y + 122.0, 9.0, palette::DIM, Align::Left);
        c.text(FontId::Digits, v, sx, y + 138.0, 15.0, palette::TEXT, Align::Left);
    }

    hits.push((Action::AdblockToggle, switch_row(c, x, 230.0, w, "广告拦截", "连着 U60 的手机、电脑都生效", a.enabled)));

    // Pause.
    let y = 300.0;
    c.glass(x, y, w, 76.0, 16.0);
    c.text(FontId::Sans, "暂停拦截", x + 14.0, y + 24.0, 13.0, palette::TEXT, Align::Left);
    if a.paused_until > 0 {
        c.text(FontId::Sans, "已暂停，到时自动恢复", x + w - 14.0, y + 24.0, 10.0, palette::AMBER, Align::Right);
        hits.push((Action::AdblockResume, button(c, x + 14.0, y + 36.0, w - 28.0, 30.0, "现在恢复拦截", palette::BLUE)));
    } else {
        c.text(FontId::Sans, "某个网站打不开时先停一下", x + w - 14.0, y + 24.0, 10.0, palette::DIM, Align::Right);
        let bw = (w - 28.0 - 12.0) / 3.0;
        for (i, (label, mins)) in [("10 分钟", 10u32), ("1 小时", 60), ("到明早", 0)].iter().enumerate() {
            let bx = x + 14.0 + i as f32 * (bw + 6.0);
            let r = button(c, bx, y + 36.0, bw, 30.0, label, Rgba(255, 255, 255, 26));
            if a.enabled {
                hits.push((Action::AdblockPause(*mins), r));
            }
        }
    }

    // Most blocked.
    let y = 384.0;
    c.glass(x, y, w, 66.0, 16.0);
    c.text(FontId::Sans, "拦截最多", x + 14.0, y + 22.0, 12.0, palette::TEXT, Align::Left);
    c.text(FontId::Sans, "24 小时", x + w - 14.0, y + 22.0, 10.0, palette::DIM, Align::Right);
    if let Some((domain, n)) = a.top.first() {
        let max = *n as f32;
        for (i, (d, n)) in a.top.iter().take(1).enumerate() {
            let ry = y + 44.0 + i as f32 * 18.0;
            c.text(FontId::Digits, &format!("{}", i + 1), x + 14.0, ry, 12.0, palette::DIM, Align::Left);
            let mut name = d.clone();
            while c.fonts.measure(FontId::Sans, &name, 12.0) > 200.0 && name.chars().count() > 6 {
                name.remove(0);
            }
            c.text(FontId::Sans, &name, x + 30.0, ry, 12.0, palette::TEXT, Align::Left);
            c.text(FontId::Digits, &n.to_string(), x + w - 14.0, ry, 13.0, palette::TEXT, Align::Right);
            let frac = *n as f32 / max.max(1.0);
            c.rrect(x + 30.0, ry + 6.0, (w - 44.0) * frac, 2.0, 1.0, palette::ORANGE);
        }
        let _ = domain;
    } else {
        c.text(FontId::Sans, "还没有拦截记录", x + w / 2.0, y + 48.0, 11.0, palette::DIM, Align::Center);
    }
    c.page_dots(160.0, 466.0, pages, page);
    hits
}
