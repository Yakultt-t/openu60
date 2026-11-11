//! Internet mode: SIM hotspot, Wi-Fi repeater (home Wi-Fi + own hotspot) or plain Wi-Fi
//! connection (own hotspot off), plus the nearby networks to join.

use super::{button, header, wifi_bars, Action, Hits};
use crate::gfx::{palette, rounded_rect, Align, Canvas, FontId, Rgba};
use crate::model::{mode_name, Status};

fn band(freq: u32) -> &'static str {
    if freq >= 4900 { "5G" } else if freq > 0 { "2.4G" } else { "" }
}

pub fn draw(c: &mut Canvas, s: &Status, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    header(c, "上网方式");
    let mut hits = Hits::new();
    let (x, w) = (14.0, 292.0);
    let mode = if s.uplink_mode.is_empty() { "hotspot" } else { s.uplink_mode.as_str() };

    // Three mode cards.
    let y = 80.0;
    let cw = (w - 12.0) / 3.0;
    let modes = [("hotspot", "SIM 卡上网"), ("repeater", "家里 Wi-Fi + 热点"), ("client", "家里 Wi-Fi，关热点")];
    for (i, (key, note)) in modes.iter().enumerate() {
        let cx = x + i as f32 * (cw + 6.0);
        let sel = mode == *key;
        if sel {
            c.fill_path(&rounded_rect(cx, y, cw, 66.0, 14.0), Rgba(64, 132, 255, 70));
            c.stroke_path(&rounded_rect(cx + 0.75, y + 0.75, cw - 1.5, 64.5, 14.0), palette::BLUE, 1.5);
        } else {
            c.glass(cx, y, cw, 66.0, 14.0);
        }
        c.text(FontId::Sans, mode_name(key), cx + cw / 2.0, y + 28.0, 13.0, if sel { palette::WHITE } else { palette::TEXT }, Align::Center);
        c.text(FontId::Sans, note, cx + cw / 2.0, y + 48.0, 9.0, palette::DIM, Align::Center);
        if !sel {
            hits.push((Action::SetMode(key.to_string()), [cx, y, cw, 66.0]));
        }
    }

    // What is happening now.
    let y = 154.0;
    c.glass(x, y, w, 50.0, 16.0);
    let (line, col) = if s.relay_connecting {
        ("正在切换…".to_string(), palette::AMBER)
    } else if s.wifi_uplink {
        (format!("已连上 {} · {}", s.relay_ssid, band(s.relay_freq)), palette::GREEN)
    } else if mode == "hotspot" {
        (format!("用 SIM 卡上网 · {}", s.net_type), palette::TEXT)
    } else {
        ("没连上 Wi-Fi，选一个网络".to_string(), palette::AMBER)
    };
    c.circle(x + 18.0, y + 25.0, 4.0, col);
    c.text(FontId::Sans, &line, x + 30.0, y + 30.0, 12.0, palette::TEXT, Align::Left);
    if mode == "client" {
        c.text(FontId::Sans, "热点已关，手机请连同一个 Wi-Fi", x + 30.0, y + 44.0, 9.0, palette::DIM, Align::Left);
    }
    // Once on home Wi-Fi, the router's own hotspot can be switched off (and back on).
    if s.wifi_uplink && !s.relay_connecting {
        let on = mode != "client";
        c.text(FontId::Sans, "自身热点", x + w - 58.0, y + 20.0, 9.0, palette::DIM, Align::Center);
        let tx = x + w - 76.0;
        c.rrect(tx, y + 25.0, 36.0, 20.0, 10.0, if on { palette::BLUE } else { Rgba(255, 255, 255, 50) });
        c.circle(if on { tx + 26.0 } else { tx + 10.0 }, y + 35.0, 8.0, palette::WHITE);
        let target = if on { "client" } else { "repeater" };
        hits.push((Action::SetMode(target.to_string()), [x + w - 100.0, y, 100.0, 50.0]));
    }

    // Nearby networks (joining uses the Wi-Fi mode picked above; repeater if in hotspot mode).
    let y = 212.0;
    let rows = s.scan.len().min(4);
    let h = 48.0 + rows.max(1) as f32 * 42.0;
    c.glass(x, y, w, h, 18.0);
    c.text(FontId::Sans, "附近的 Wi-Fi", x + 14.0, y + 26.0, 13.0, palette::TEXT, Align::Left);
    let label = if s.scanning { "扫描中…" } else { "扫描" };
    hits.push((Action::RelayScan, button(c, x + w - 80.0, y + 9.0, 66.0, 28.0, label, Rgba(255, 255, 255, 30))));
    if s.scan.is_empty() {
        c.text(FontId::Sans, if s.scanning { "正在找附近的网络…" } else { "点「扫描」找附近的网络" }, x + w / 2.0, y + 74.0, 12.0, palette::DIM, Align::Center);
    }
    for (i, n) in s.scan.iter().take(4).enumerate() {
        let ry = y + 44.0 + i as f32 * 42.0;
        if i > 0 {
            c.rect(x + 14.0, ry, w - 28.0, 1.0, Rgba(255, 255, 255, 18));
        }
        wifi_bars(c, x + 24.0, ry + 28.0, n.signal);
        let current = s.wifi_uplink && n.ssid == s.relay_ssid;
        let mut name = n.ssid.clone();
        while c.fonts.measure(FontId::Sans, &name, 14.0) > 170.0 && name.chars().count() > 4 {
            name.pop();
            name.pop();
            name.push('…');
        }
        c.text(FontId::Sans, &name, x + 44.0, ry + 25.0, 14.0, if current { palette::SKY } else { palette::TEXT }, Align::Left);
        let sub = format!("{}{}", band(n.freq), if n.secure { " · 加密" } else { " · 开放" });
        c.text(FontId::Sans, &sub, x + 44.0, ry + 38.0, 10.0, palette::DIM, Align::Left);
        let known = n.ssid == s.relay_last_ssid || !n.secure;
        let right = if current { "已连接" } else if known { "连接" } else { "手机输密码" };
        c.text(FontId::Sans, right, x + w - 14.0, ry + 28.0, 11.0, if current { palette::GREEN } else { palette::DIM }, Align::Right);
        if !current {
            hits.push((Action::RelayJoin(n.ssid.clone(), known), [x, ry, w, 42.0]));
        }
    }
    c.text(FontId::Sans, "新的加密网络请在手机 SIM 随行 app 里输入密码", 160.0, 450.0, 10.0, palette::FAINT, Align::Center);
    c.page_dots(160.0, 466.0, pages, page);
    hits
}
