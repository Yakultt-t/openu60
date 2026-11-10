//! Raw evdev readers: the Sitronix touchscreen (event3) and the PMIC power key (event0).
//! Neither is grabbed, so the stock touch bridge and key daemon keep working.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::gfx::{H, W};

pub const TOUCH_DEV: &str = "/dev/input/event3";
pub const POWER_DEV: &str = "/dev/input/event0";
const KEY_POWER: u16 = 116;

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    /// Power key released after `ms` milliseconds.
    Power(u64),
}

fn records(dev: &str, mut on: impl FnMut(u16, u16, i32)) {
    let Ok(mut f) = OpenOptions::new().read(true).custom_flags(0).open(dev) else {
        eprintln!("[screen] cannot open {dev}");
        return;
    };
    let mut buf = [0u8; 24 * 64];
    loop {
        let n = match f.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => return,
        };
        for ev in buf[..n].chunks_exact(24) {
            on(
                u16::from_le_bytes([ev[16], ev[17]]),
                u16::from_le_bytes([ev[18], ev[19]]),
                i32::from_le_bytes([ev[20], ev[21], ev[22], ev[23]]),
            );
        }
    }
}

/// Touch thread: one finger, coordinates rotated to match the upside-down panel.
pub fn touch(tx: Sender<Event>, rotate: bool) {
    let (mut x, mut y, mut down, mut was_down) = (0i32, 0i32, false, false);
    records(TOUCH_DEV, |typ, code, val| match (typ, code) {
        (3, 0x35) => x = val,
        (3, 0x36) => y = val,
        (1, 0x14a) => down = val != 0,
        (0, 0) => {
            let (fx, fy) = if rotate { ((W as i32 - 1 - x) as f32, (H as i32 - 1 - y) as f32) } else { (x as f32, y as f32) };
            let ev = match (was_down, down) {
                (false, true) => Some(Event::Down(fx, fy)),
                (true, true) => Some(Event::Move(fx, fy)),
                (true, false) => Some(Event::Up(fx, fy)),
                _ => None,
            };
            was_down = down;
            if let Some(e) = ev {
                let _ = tx.send(e);
            }
        }
        _ => {}
    });
}

/// Power key thread: reports each press with its duration.
pub fn power(tx: Sender<Event>) {
    let mut pressed: Option<Instant> = None;
    records(POWER_DEV, |typ, code, val| {
        if typ == 1 && code == KEY_POWER {
            if val == 1 {
                pressed = Some(Instant::now());
            } else if val == 0 {
                if let Some(t) = pressed.take() {
                    let _ = tx.send(Event::Power(t.elapsed().as_millis() as u64));
                }
            }
        }
    });
}
