//! Settings: a tile grid like the reference design, with sub-pages for appearance, display,
//! screen lock and advanced options.

use super::{back_header, button, chevron, header, tile, Action, Hits, Sub};
use crate::gfx::{hgradient, palette, rounded_rect, Align, Canvas, FontId, Rgba};
use crate::model::Status;
use crate::settings::{Settings, THEMES, TIMEOUTS};

fn theme_name(key: &str) -> &'static str {
    THEMES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n).unwrap_or("地球")
}

fn timeout_label(t: u32) -> String {
    if t < 60 { format!("{t} 秒") } else { format!("{} 分钟", t / 60) }
}

pub fn draw(c: &mut Canvas, s: &Status, set: &Settings, sub: Option<Sub>, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    let hits = match sub {
        None => home(c, s, set),
        Some(Sub::Appearance) => appearance(c, set),
        Some(Sub::Display) => display(c, set),
        Some(Sub::Advanced) => advanced(c, s),
    };
    if sub.is_none() {
        c.page_dots(160.0, 466.0, pages, page);
    }
    hits
}

fn home(c: &mut Canvas, s: &Status, set: &Settings) -> Hits {
    header(c, "设置");
    let mut hits = Hits::new();
    let (x, w) = (14.0, 292.0);
    let half = (w - 8.0) / 2.0;
    let x2 = x + half + 8.0;

    // Appearance row with a tiny preview.
    let y = 80.0;
    c.glass(x, y, w, 62.0, 16.0);
    c.rrect(x + 12.0, y + 11.0, 26.0, 40.0, 6.0, Rgba(30, 60, 120, 255));
    c.rrect(x + 42.0, y + 11.0, 26.0, 40.0, 6.0, Rgba(20, 40, 80, 255));
    c.text(FontId::Digits, &format!("{:02}:{:02}", s.hour, s.minute), x + 25.0, y + 30.0, 9.0, palette::WHITE, Align::Center);
    c.text(FontId::Sans, "外观", x + 80.0, y + 28.0, 14.0, palette::TEXT, Align::Left);
    c.text(FontId::Sans, &format!("主题 {} · 唤醒{}", theme_name(&set.theme), if set.wake_cover { "显示封面" } else { "回到原页面" }), x + 80.0, y + 46.0, 10.0, palette::DIM, Align::Left);
    chevron(c, x + w - 18.0, y + 31.0);
    hits.push((Action::Open(Sub::Appearance), [x, y, w, 62.0]));

    let row = |i: f32| 148.0 + i * 52.0;
    let pct = (set.brightness as u32 * 100 / 255).max(1);
    hits.push((Action::Open(Sub::Display), tile(c, x, row(0.0), half, 46.0, ("亮", palette::AMBER), "屏幕", &format!("{pct}% · {}", timeout_label(set.timeout_secs)), None)));
    let ad = if s.adblock.enabled { if s.adblock.paused_until > 0 { "已暂停" } else { "拦截中" } } else { "已关闭" };
    hits.push((Action::AdblockToggle, tile(c, x2, row(0.0), half, 46.0, ("", palette::GREEN), "广告拦截", ad, Some(s.adblock.enabled))));
    hits.push((Action::ToggleWakeCover, tile(c, x, row(1.0), half, 46.0, ("", palette::BLUE), "唤醒封面", "", Some(set.wake_cover))));
    hits.push((Action::ToggleBatteryPct, tile(c, x2, row(1.0), half, 46.0, ("", palette::BLUE), "电量百分比", "", Some(set.battery_pct))));
    hits.push((Action::PowerbankToggle, tile(c, x, row(2.0), half, 46.0, ("", palette::BLUE), "反向供电", "", Some(s.powerbank))));
    let unit = if set.speed_bytes { "MB/s" } else { "Mbps" };
    let r = tile(c, x2, row(2.0), half, 46.0, ("", palette::BLUE), "速率单位", "", None);
    c.text(FontId::Sans, unit, x2 + half - 12.0, row(2.0) + 28.0, 12.0, palette::SKY, Align::Right);
    hits.push((Action::ToggleSpeedUnit, r));
    let r = tile(c, x, row(3.0), w, 46.0, ("", palette::BLUE), "高级设置", "", None);
    chevron(c, x + w - 16.0, row(3.0) + 23.0);
    hits.push((Action::Open(Sub::Advanced), r));
    let push = if s.push_enabled { format!("U60 助手 · {} 台手机", s.push_devices) } else { "未开启".into() };
    tile(c, x, row(4.0), w, 46.0, ("推", palette::PURPLE), "消息推送", &push, None);

    // Device line.
    let y = row(5.0);
    c.glass(x, y, w, 32.0, 14.0);
    c.text(FontId::Sans, "本机", x + 12.0, y + 21.0, 11.0, palette::TEXT, Align::Left);
    let info = format!("CPU {:.0}% · {:.0}°C · 内存 {:.0}%", s.cpu_pct, s.temp_c, s.memory_pct);
    c.text(FontId::Sans, &info, x + w - 12.0, y + 21.0, 11.0, palette::DIM, Align::Right);
    hits
}

fn appearance(c: &mut Canvas, set: &Settings) -> Hits {
    let mut hits = Hits::new();
    back_header(c, "外观", &mut hits);
    let (x, w) = (14.0, 292.0);
    let tw = (w - 16.0) / 3.0;
    let swatches: [(&str, [Rgba; 2]); 3] = [
        ("earth", [Rgba::rgb(8, 16, 40), Rgba::rgb(60, 140, 255)]),
        ("space", [Rgba::rgb(6, 6, 16), Rgba::rgb(90, 50, 140)]),
        ("aurora", [Rgba::rgb(4, 14, 24), Rgba::rgb(40, 220, 150)]),
    ];
    for (i, (key, cols)) in swatches.iter().enumerate() {
        let tx = x + i as f32 * (tw + 8.0);
        let y = 84.0;
        let sel = set.theme == *key;
        c.fill_path_shader(&rounded_rect(tx, y, tw, 150.0, 14.0), crate::gfx::vgradient(y, y + 150.0, &[(0.0, cols[0]), (0.6, cols[0]), (1.0, cols[1])]));
        if sel {
            c.stroke_path(&rounded_rect(tx + 1.0, y + 1.0, tw - 2.0, 148.0, 14.0), palette::SKY, 2.5);
        }
        c.text(FontId::Sans, theme_name(key), tx + tw / 2.0, y + 172.0, 13.0, if sel { palette::SKY } else { palette::TEXT }, Align::Center);
        hits.push((Action::Theme(key.to_string()), [tx, y, tw, 180.0]));
    }
    let y = 280.0;
    let custom = std::path::Path::new(&format!("{}/wallpaper.png", crate::settings::HOME)).exists();
    let note = if custom { "用手机 app 里选的照片" } else { "在手机 U60 助手 app 里选一张照片" };
    let r = tile(c, x, y, w, 56.0, ("", palette::PURPLE), "自定义壁纸", note, None);
    if custom {
        if set.theme == "custom" {
            c.text(FontId::Sans, "使用中", x + w - 14.0, y + 33.0, 11.0, palette::SKY, Align::Right);
        }
        hits.push((Action::Theme("custom".into()), r));
    }
    hits.push((Action::ToggleWakeCover, tile(c, x, 344.0, w, 56.0, ("", palette::BLUE), "唤醒封面", "亮屏时先显示时间封面", Some(set.wake_cover))));
    hits
}

fn display(c: &mut Canvas, set: &Settings) -> Hits {
    let mut hits = Hits::new();
    back_header(c, "屏幕", &mut hits);
    let (x, w) = (14.0, 292.0);
    let y = 84.0;
    c.glass(x, y, w, 86.0, 18.0);
    c.text(FontId::Sans, "亮度", x + 14.0, y + 24.0, 13.0, palette::TEXT, Align::Left);
    c.text(FontId::Digits, &format!("{}%", (set.brightness as u32 * 100 / 255).max(1)), x + w - 14.0, y + 24.0, 15.0, palette::TEXT, Align::Right);
    let (bx, by, bw) = (x + 60.0, y + 46.0, w - 120.0);
    c.rrect(bx, by, bw, 22.0, 11.0, Rgba(255, 255, 255, 30));
    let frac = set.brightness as f32 / 255.0;
    c.fill_path_shader(&rounded_rect(bx, by, (bw * frac).max(22.0), 22.0, 11.0), hgradient(bx, bx + bw, &[(0.0, palette::AMBER), (1.0, Rgba::rgb(255, 230, 120))]));
    hits.push((Action::Brightness(-25), button(c, x + 12.0, y + 40.0, 40.0, 34.0, "-", Rgba(255, 255, 255, 28))));
    hits.push((Action::Brightness(25), button(c, x + w - 52.0, y + 40.0, 40.0, 34.0, "+", Rgba(255, 255, 255, 28))));

    let y = 180.0;
    c.glass(x, y, w, 84.0, 18.0);
    c.text(FontId::Sans, "自动熄屏", x + 14.0, y + 24.0, 13.0, palette::TEXT, Align::Left);
    let tw = (w - 28.0 - 18.0) / 4.0;
    for (i, t) in TIMEOUTS.iter().enumerate() {
        let tx = x + 14.0 + i as f32 * (tw + 6.0);
        let fill = if *t == set.timeout_secs { palette::BLUE } else { Rgba(255, 255, 255, 24) };
        hits.push((Action::Timeout(*t), button(c, tx, y + 40.0, tw, 32.0, &timeout_label(*t), fill)));
    }
    hits.push((Action::ToggleBatteryPct, tile(c, x, 274.0, w, 52.0, ("", palette::BLUE), "电量百分比", "状态栏显示电量数字", Some(set.battery_pct))));
    let r = tile(c, x, 334.0, w, 52.0, ("", palette::BLUE), "速率单位", "网速显示成 Mbps 或 MB/s", None);
    c.text(FontId::Sans, if set.speed_bytes { "MB/s" } else { "Mbps" }, x + w - 14.0, 365.0, 13.0, palette::SKY, Align::Right);
    hits.push((Action::ToggleSpeedUnit, r));
    hits
}

fn advanced(c: &mut Canvas, s: &Status) -> Hits {
    let mut hits = Hits::new();
    back_header(c, "高级设置", &mut hits);
    let (x, w) = (14.0, 292.0);
    hits.push((Action::StockUi, tile(c, x, 84.0, w, 56.0, ("", palette::BLUE), "回到原厂界面", "在原厂界面连按三下电源键回来", None)));
    let rules = if s.adblock.domains > 0 { format!("{:.1} 万条规则", s.adblock.domains as f32 / 10_000.0) } else { "还没有规则".to_string() };
    hits.push((Action::AdblockUpdate, tile(c, x, 148.0, w, 56.0, ("", palette::GREEN), "更新广告规则", &rules, None)));
    let half = (w - 8.0) / 2.0;
    hits.push((Action::Reboot, button(c, x, 222.0, half, 46.0, "重启", Rgba(255, 190, 70, 80))));
    hits.push((Action::PowerOff, button(c, x + half + 8.0, 222.0, half, 46.0, "关机", Rgba(255, 95, 90, 80))));
    c.glass(x, 286.0, w, 92.0, 16.0);
    let lines = [
        format!("CPU {:.0}% · 温度 {:.0}°C · 内存 {:.0}%", s.cpu_pct, s.temp_c, s.memory_pct),
        format!("电池 {}% · {:.1} W", s.battery, s.battery_watts),
        format!("屏幕程序 v{}", env!("CARGO_PKG_VERSION")),
    ];
    for (i, l) in lines.iter().enumerate() {
        c.text(FontId::Sans, l, x + 14.0, 312.0 + i as f32 * 24.0, 11.0, palette::DIM, Align::Left);
    }
    hits
}
