//! Charging: battery state, charge limit (direct supply), reverse charging.

use super::{button, header, switch_row, Action, Hits};
use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Status;

pub fn draw(c: &mut Canvas, s: &Status, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    header(c, "充电");
    let mut hits = Hits::new();
    let (x, w) = (14.0, 292.0);

    // Battery summary.
    let y = 82.0;
    c.glass(x, y, w, 96.0, 18.0);
    let a = c.text(FontId::Digits, &s.battery.to_string(), x + 14.0, y + 52.0, 40.0, palette::WHITE, Align::Left);
    c.text(FontId::Sans, "%", x + 18.0 + a, y + 52.0, 12.0, palette::DIM, Align::Left);
    let state = if s.charging {
        "正在充电"
    } else if s.powerbank {
        // The USB-C port is giving power out, so a cable to a computer does not charge.
        "反向供电中 · 不会充电"
    } else if s.charger_connected && s.charge_limit_enabled && s.battery as u32 >= s.charge_limit.saturating_sub(5) {
        "已到上限 · 用充电器的电"
    } else if s.charger_connected {
        "已接电源"
    } else {
        "未接电源"
    };
    c.text(FontId::Sans, state, x + w - 14.0, y + 28.0, 12.0, if s.charging { palette::GREEN } else { palette::DIM }, Align::Right);
    let detail = format!("{:.1} W · {:.0}°C", s.battery_watts, s.battery_temp_c);
    c.text(FontId::Sans, &detail, x + w - 14.0, y + 50.0, 11.0, palette::DIM, Align::Right);
    let col = if s.battery <= 15 { palette::RED } else if s.battery <= 30 { palette::AMBER } else { palette::GREEN };
    c.level_bar(x + 14.0, y + 68.0, w - 28.0, 12.0, s.battery as f32 / 100.0, col);

    // Charge limit = "direct supply": stop at the limit and run from the charger.
    hits.push((Action::ChargeLimitToggle, switch_row(c, x, 188.0, w, "直供电", "充到上限就停，直接用充电器的电，护电池", s.charge_limit_enabled)));
    let y = 260.0;
    c.glass(x, y, w, 78.0, 18.0);
    c.text(FontId::Sans, "充电上限", x + 14.0, y + 26.0, 13.0, palette::TEXT, Align::Left);
    c.text(FontId::Digits, &format!("{}%", s.charge_limit), x + w - 14.0, y + 28.0, 22.0, palette::WHITE, Align::Right);
    let (bx, by, bw) = (x + 60.0, y + 44.0, w - 120.0);
    c.rrect(bx, by, bw, 18.0, 9.0, Rgba(255, 255, 255, 30));
    let frac = (s.charge_limit as f32 - 50.0) / 50.0;
    c.rrect(bx, by, (bw * frac.clamp(0.0, 1.0)).max(18.0), 18.0, 9.0, if s.charge_limit_enabled { palette::GREEN } else { palette::FAINT });
    hits.push((Action::ChargeLimit(-5), button(c, x + 12.0, y + 38.0, 40.0, 30.0, "-", Rgba(255, 255, 255, 28))));
    hits.push((Action::ChargeLimit(5), button(c, x + w - 52.0, y + 38.0, 40.0, 30.0, "+", Rgba(255, 255, 255, 28))));

    hits.push((Action::PowerbankToggle, switch_row(c, x, 348.0, w, "反向供电", "用 USB 口给手机等设备充电", s.powerbank)));
    c.page_dots(160.0, 462.0, pages, page);
    hits
}
