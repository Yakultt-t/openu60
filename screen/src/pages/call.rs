//! Full-screen call card shown over everything while the router's line rings or talks.
//! Answering happens on the phone (that is where the audio is); here you can only decline.

use super::{Action, Hits};
use crate::gfx::{palette, Align, Canvas, FontId, Rgba};
use crate::model::Call;

pub fn draw(c: &mut Canvas, call: &Call) -> Hits {
    c.rect(0.0, 0.0, 320.0, 480.0, Rgba(4, 6, 12, 236));
    let pulse = (call.since.elapsed().as_millis() % 1600) as f32 / 1600.0;
    if call.state == "来电" {
        for k in 0..3 {
            let t = (pulse + k as f32 / 3.0) % 1.0;
            c.circle(160.0, 170.0, 44.0 + 50.0 * t, palette::GREEN.alpha(0.25 * (1.0 - t)));
        }
    }
    c.circle(160.0, 170.0, 44.0, if call.state == "来电" { palette::GREEN } else { palette::BLUE });
    c.text(FontId::Sans, if call.state == "来电" { "来电" } else { "通话" }, 160.0, 178.0, 20.0, palette::WHITE, Align::Center);
    let number = if call.number.is_empty() { "未知号码".to_string() } else { call.number.clone() };
    c.text(FontId::Digits, &number, 160.0, 268.0, 30.0, palette::WHITE, Align::Center);
    let sub = match call.state {
        "来电" => "来电 · 在手机 U60 助手 上接听".to_string(),
        "呼出" => "正在呼叫…".to_string(),
        _ => {
            let s = call.since.elapsed().as_secs();
            format!("通话中 {:02}:{:02}", s / 60, s % 60)
        }
    };
    c.text(FontId::Sans, &sub, 160.0, 296.0, 13.0, palette::DIM, Align::Center);
    let mut hits = Hits::new();
    let label = if call.state == "来电" { "拒接" } else { "挂断" };
    c.circle(160.0, 388.0, 32.0, palette::RED);
    c.text(FontId::Sans, label, 160.0, 394.0, 15.0, palette::WHITE, Align::Center);
    hits.push((Action::Hangup, [120.0, 348.0, 80.0, 80.0]));
    hits
}
