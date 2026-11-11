//! Messages: conversations (newest first) and one conversation's messages, read-only.
//! Typing happens in the phone app; the screen is for a quick look.

use super::{back_header, button, header, Action, Hits, Sub};
use crate::gfx::{palette, Align, Canvas, FontId, Fonts, Rgba};
use crate::model::{Sms, Status};

/// One conversation: messages with one number, oldest first.
pub struct Thread<'a> {
    pub key: u64,
    pub number: &'a str,
    pub msgs: Vec<&'a Sms>,
    pub unread: usize,
}

/// Stable id of a conversation (survives new messages reordering the list).
pub fn thread_key(number: &str) -> u64 {
    number.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

pub fn threads(s: &Status) -> Vec<Thread<'_>> {
    let mut out: Vec<Thread> = Vec::new();
    // `s.sms` is newest first, so threads come out ordered by their latest message.
    for m in &s.sms {
        let key = thread_key(&m.number);
        match out.iter_mut().find(|t| t.key == key) {
            Some(t) => t.msgs.push(m),
            None => out.push(Thread { key, number: &m.number, msgs: vec![m], unread: 0 }),
        }
    }
    for t in &mut out {
        t.msgs.reverse();
        t.unread = t.msgs.iter().filter(|m| m.unread).count();
    }
    out
}

/// Breaks text into lines no wider than `max` (by character; CJK has no spaces).
pub fn wrap(fonts: &Fonts, text: &str, size: f32, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut w = 0.0;
        for ch in para.chars() {
            let cw = fonts.measure(FontId::Sans, &ch.to_string(), size);
            if w + cw > max && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                w = 0.0;
            }
            line.push(ch);
            w += cw;
        }
        lines.push(line);
    }
    lines
}

fn ellipsize(fonts: &Fonts, text: &str, size: f32, max: f32) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if fonts.measure(FontId::Sans, &flat, size) <= max {
        return flat;
    }
    let mut out = String::new();
    for ch in flat.chars() {
        if fonts.measure(FontId::Sans, &format!("{out}{ch}…"), size) > max {
            break;
        }
        out.push(ch);
    }
    out + "…"
}

const LIST_ROWS: usize = 6;

pub fn draw(c: &mut Canvas, s: &Status, sub: Option<Sub>, scroll: usize, page: usize, pages: usize) -> Hits {
    super::status_bar(c, s, true);
    let hits = match sub {
        Some(Sub::Sms(key)) => thread(c, s, key, scroll),
        _ => list(c, s, scroll),
    };
    if !matches!(sub, Some(Sub::Sms(_))) {
        c.page_dots(160.0, 466.0, pages, page);
    }
    hits
}

fn pager(c: &mut Canvas, hits: &mut Hits, can_up: bool, can_down: bool) {
    if can_up {
        hits.push((Action::SmsScroll(-1), button(c, 196.0, 44.0, 52.0, 30.0, "上页", Rgba(255, 255, 255, 28))));
    }
    if can_down {
        hits.push((Action::SmsScroll(1), button(c, 254.0, 44.0, 52.0, 30.0, "下页", Rgba(255, 255, 255, 28))));
    }
}

fn list(c: &mut Canvas, s: &Status, scroll: usize) -> Hits {
    let mut hits = Hits::new();
    header(c, "短信");
    let all = threads(s);
    let (x, w) = (14.0, 292.0);
    if all.is_empty() {
        c.glass(x, 84.0, w, 70.0, 16.0);
        let note = if s.sms_loaded { "还没有短信" } else { "正在读取短信…" };
        c.text(FontId::Sans, note, 160.0, 124.0, 13.0, palette::DIM, Align::Center);
        return hits;
    }
    let pages = all.len().div_ceil(LIST_ROWS);
    let page = scroll.min(pages - 1);
    pager(c, &mut hits, page > 0, page + 1 < pages);
    for (i, t) in all.iter().skip(page * LIST_ROWS).take(LIST_ROWS).enumerate() {
        let y = 84.0 + i as f32 * 62.0;
        c.glass(x, y, w, 56.0, 14.0);
        let last = t.msgs.last().unwrap();
        if t.unread > 0 {
            c.circle(x + 12.0, y + 20.0, 4.0, palette::SKY);
        }
        let name_x = x + 22.0;
        let time_w = c.text(FontId::Sans, &last.time, x + w - 12.0, y + 22.0, 10.0, palette::DIM, Align::Right);
        let number = ellipsize(c.fonts, t.number, 14.0, w - 44.0 - time_w);
        c.text(FontId::Sans, &number, name_x, y + 23.0, 14.0, if t.unread > 0 { palette::WHITE } else { palette::TEXT }, Align::Left);
        let prefix = if last.incoming { "" } else { "我：" };
        let preview = ellipsize(c.fonts, &format!("{prefix}{}", last.text), 11.0, w - 34.0);
        c.text(FontId::Sans, &preview, name_x, y + 43.0, 11.0, palette::DIM, Align::Left);
        hits.push((Action::SmsOpen(t.key), [x, y, w, 56.0]));
    }
    hits
}

const TOP: f32 = 84.0;
const BOTTOM: f32 = 470.0;
const SIZE: f32 = 13.0;
const LINE: f32 = 18.0;
const MAX_TEXT_W: f32 = 210.0;

fn thread(c: &mut Canvas, s: &Status, key: u64, scroll: usize) -> Hits {
    let mut hits = Hits::new();
    let all = threads(s);
    let Some(t) = all.iter().find(|t| t.key == key) else {
        back_header(c, "短信", &mut hits);
        return hits;
    };
    let title = ellipsize(c.fonts, t.number, 20.0, 150.0);
    back_header(c, &title, &mut hits);

    // Lay out bottom-up from the newest message, skipping `scroll` messages from the end.
    let skip = scroll.min(t.msgs.len().saturating_sub(1));
    let mut y = BOTTOM;
    let mut shown = 0;
    for m in t.msgs.iter().rev().skip(skip) {
        let lines = wrap(c.fonts, &m.text, SIZE, MAX_TEXT_W);
        let text_w = lines.iter().map(|l| c.fonts.measure(FontId::Sans, l, SIZE)).fold(0.0f32, f32::max);
        let bw = text_w + 20.0;
        let bh = lines.len() as f32 * LINE + 12.0;
        let block = bh + 16.0;
        if y - block < TOP && shown > 0 {
            break;
        }
        y -= block;
        let bx = if m.incoming { 14.0 } else { 306.0 - bw };
        let fill = if m.incoming { Rgba(255, 255, 255, 34) } else { Rgba(60, 130, 255, 170) };
        c.rrect(bx, y, bw, bh, 12.0, fill);
        for (i, l) in lines.iter().enumerate() {
            c.text(FontId::Sans, l, bx + 10.0, y + 19.0 + i as f32 * LINE, SIZE, palette::WHITE, Align::Left);
        }
        let (tx, align) = if m.incoming { (bx + 4.0, Align::Left) } else { (bx + bw - 4.0, Align::Right) };
        c.text(FontId::Sans, &m.time, tx, y + bh + 12.0, 9.0, palette::FAINT, align);
        shown += 1;
    }
    pager(c, &mut hits, skip + shown < t.msgs.len(), skip > 0);
    hits
}
