//! Shown for a while after a USB cable goes in, like the stock screen: pick what the port does.

use super::{button, Action, Hits};
use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Status;

/// How long the prompt stays up without a tap.
pub const PROMPT_FOR: std::time::Duration = std::time::Duration::from_secs(30);

pub fn draw(c: &mut Canvas, s: &Status) -> Hits {
    let mut hits = Hits::new();
    c.rect(0.0, 0.0, 320.0, 480.0, Rgba(4, 6, 12, 200));
    let (x, w) = (18.0, 284.0);
    let (y, h) = (70.0, 352.0);
    c.rrect(x, y, w, h, 22.0, Rgba(24, 30, 46, 250));
    c.text(FontId::Sans, "已连接 USB", 160.0, y + 36.0, 17.0, palette::WHITE, Align::Center);
    c.text(FontId::Sans, "选择 USB 口的用途", 160.0, y + 58.0, 11.0, palette::DIM, Align::Center);
    let options: [(&str, &str, Action); 4] = [
        ("网络共享（正常）", "user", Action::UsbMode("user".into())),
        ("调试（ADB）", "debug", Action::UsbMode("debug".into())),
        ("仅充电", "charging", Action::UsbMode("charging".into())),
        ("给对方供电", "", Action::PowerbankToggle),
    ];
    for (i, (label, mode, action)) in options.into_iter().enumerate() {
        let by = y + 76.0 + i as f32 * 56.0;
        let current = (!mode.is_empty() && s.usb_mode == mode) || (mode.is_empty() && s.powerbank);
        let fill = if current { Rgba(70, 150, 255, 110) } else { Rgba(255, 255, 255, 26) };
        let r = button(c, x + 16.0, by, w - 32.0, 46.0, label, fill);
        if current {
            c.text(FontId::Sans, if mode.is_empty() { "已打开" } else { "使用中" }, x + w - 28.0, by + 28.0, 10.0, palette::SKY, Align::Right);
        }
        hits.push((action, r));
    }
    hits.push((Action::UsbPromptClose, button(c, x + 16.0, y + h - 54.0, w - 32.0, 40.0, "不用改", Rgba(255, 255, 255, 14))));
    hits
}
