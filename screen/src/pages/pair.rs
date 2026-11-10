//! Connect & pair: the hotspot as a Wi-Fi QR code. The OpenU60 app's setup guide scans it,
//! joins the network and pairs, so a new phone needs no typing.

use super::{header, Hits};
use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Status;

fn escape(v: &str) -> String {
    v.chars().flat_map(|c| if "\\;,:\"".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

pub fn draw(c: &mut Canvas, s: &Status, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    header(c, "连接与配对");
    let (x, w) = (14.0, 292.0);
    let y = 82.0;
    c.glass(x, y, w, 262.0, 18.0);
    if s.hotspot_ssid.is_empty() {
        c.text(FontId::Sans, "正在读取 Wi-Fi 信息…", 160.0, y + 130.0, 13.0, palette::DIM, Align::Center);
    } else {
        let kind = if s.hotspot_enc.contains("sae") { "SAE" } else if s.hotspot_enc == "none" { "nopass" } else { "WPA" };
        let payload = format!("WIFI:T:{kind};S:{};P:{};;", escape(&s.hotspot_ssid), escape(&s.hotspot_key));
        if let Ok(code) = qrcode::QrCode::new(payload.as_bytes()) {
            let n = code.width();
            let size = 188.0;
            let cell = (size / (n as f32 + 2.0)).floor();
            let total = cell * (n as f32 + 2.0);
            let (qx, qy) = (160.0 - total / 2.0, y + 14.0);
            c.rrect(qx, qy, total, total, 10.0, palette::WHITE);
            let colors = code.to_colors();
            for (i, col) in colors.iter().enumerate() {
                if *col == qrcode::Color::Dark {
                    let (cx, cy) = ((i % n) as f32, (i / n) as f32);
                    c.rect(qx + (cx + 1.0) * cell, qy + (cy + 1.0) * cell, cell, cell, Rgba::rgb(8, 12, 24));
                }
            }
            c.text(FontId::Sans, &format!("Wi-Fi：{}", s.hotspot_ssid), 160.0, qy + total + 26.0, 13.0, palette::TEXT, Align::Center);
            c.text(FontId::Sans, "用 U60 助手 app 扫码，自动连上并配对", 160.0, qy + total + 46.0, 11.0, palette::DIM, Align::Center);
        }
    }
    let y = 354.0;
    c.glass(x, y, w, 64.0, 16.0);
    c.text(FontId::Sans, "已配对的手机", x + 14.0, y + 26.0, 13.0, palette::TEXT, Align::Left);
    c.text(FontId::Digits, &s.paired_phones.to_string(), x + w - 14.0, y + 28.0, 20.0, palette::WHITE, Align::Right);
    let mode = if s.remote_mode.is_empty() { "手机在外面时会自动走远程连接".to_string() } else { format!("现在有手机通过「{}」连着", s.remote_mode) };
    c.text(FontId::Sans, &mode, x + 14.0, y + 48.0, 10.0, palette::DIM, Align::Left);
    c.page_dots(160.0, 462.0, pages, page);
    Hits::new()
}
