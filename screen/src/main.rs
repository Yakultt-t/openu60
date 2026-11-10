//! OpenU60 touchscreen UI for the ZTE U60 Pro.
//!
//!   openu60-screen watch   stays in the background next to the stock UI; three quick presses
//!                          of the power key (or a request from the phone app) open our UI.
//!   openu60-screen run     takes the screen now and gives it back to the stock UI on exit
//!
//! The stock UI is only stopped through its init script (never disabled: it is part of the
//! boot sync barrier) and always restarted afterwards.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use openu60_screen::gfx::{palette, Align, Canvas, FontId, Fonts, Rgba};
use openu60_screen::model::Status;
use openu60_screen::pages::{Action, Hits, Sub};
use openu60_screen::settings::{self, Settings, HOME};
use openu60_screen::{actions, data, display, input, pages, wallpaper};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

const STOCK_INIT: &str = "/etc/init.d/zte_topsw_devui";
const WATCH_LOCK: &str = "/tmp/openu60-screen-watch.lock";
/// Created by zte-agent when the phone app asks to open the screen.
const OPEN_REQUEST: &str = "/tmp/openu60-screen-open";
/// Three short presses of the power key, each within this gap of the previous one, switch
/// between the stock UI and ours. A single press sleeps/wakes our screen once it is clear no
/// second press follows; a hold of `MENU_MS` opens the power menu.
const PRESS_GAP_MS: u128 = 600;
const SHORT_PRESS_MS: u64 = 800;
const MENU_MS: u64 = 2000;

/// Counts quick power-key presses.
struct Presses {
    times: Vec<Instant>,
}

impl Presses {
    /// Records a short press; true when it completes a triple press.
    fn push(&mut self) -> bool {
        let now = Instant::now();
        if self.times.last().map(|t| now.duration_since(*t).as_millis() > PRESS_GAP_MS).unwrap_or(false) {
            self.times.clear();
        }
        self.times.push(now);
        if self.times.len() >= 3 {
            self.times.clear();
            return true;
        }
        false
    }

    /// A lone press whose follow-up window has passed (taken once).
    fn settled_single(&mut self) -> bool {
        match self.times.last() {
            Some(t) if t.elapsed().as_millis() > PRESS_GAP_MS => {
                let single = self.times.len() == 1;
                self.times.clear();
                single
            }
            _ => false,
        }
    }
}
const ANIM_MS: f32 = 220.0;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[screen] {}", msg.as_ref());
}

fn stock_pid() -> Option<i32> {
    let out = Command::new("pidof").arg("zte_topsw_devui").output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().next()?.parse().ok()
}

/// Stops the stock UI quickly: procd is told to stop the service (so it will not respawn it)
/// and the process, which takes ~6 s to exit by itself, is then killed.
fn stop_stock() -> bool {
    let pid = stock_pid();
    let child = Command::new(STOCK_INIT).arg("stop").spawn();
    // Give procd a moment to mark the service stopped (so it does not respawn), then end the
    // process instead of waiting out its slow shutdown.
    std::thread::sleep(Duration::from_millis(120));
    for _ in 0..60 {
        let alive = pid.map(|p| std::path::Path::new(&format!("/proc/{p}")).exists()).unwrap_or(false);
        if !alive {
            break;
        }
        if let Some(p) = pid {
            unsafe { libc::kill(p, libc::SIGKILL); }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    if let Ok(mut c) = child {
        std::thread::spawn(move || { let _ = c.wait(); });
    }
    stock_pid().is_none() || pid.is_none()
}

fn start_stock() {
    if stock_pid().is_none() {
        let _ = Command::new(STOCK_INIT).arg("start").status();
    }
}

fn uptime_secs() -> f64 {
    std::fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0)
}

// ---------- watch mode ----------

fn watch() {
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(WATCH_LOCK).expect("lock file");
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        log("another watcher is running");
        return;
    }
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || input::power(tx));
    }
    // The phone app's "open on the router screen" request arrives as a file.
    std::thread::spawn(move || loop {
        if std::path::Path::new(OPEN_REQUEST).exists() {
            let _ = std::fs::remove_file(OPEN_REQUEST);
            let _ = tx.send(input::Event::Power(u64::MAX));
        }
        std::thread::sleep(Duration::from_millis(700));
    });
    log("watching the power key (press three times to open)");
    let exe = std::env::current_exe().unwrap_or_else(|_| format!("{HOME}/openu60-screen").into());
    let mut failures: Vec<Instant> = Vec::new();
    let mut presses = Presses { times: Vec::new() };
    loop {
        let Ok(input::Event::Power(ms)) = rx.recv() else { continue };
        if std::path::Path::new(&format!("{HOME}/disabled")).exists() {
            continue;
        }
        // u64::MAX marks a request from the app; otherwise wait for the third quick press.
        let open = ms == u64::MAX || (ms < SHORT_PRESS_MS && presses.push());
        if !open || uptime_secs() < 60.0 || stock_pid().is_none() {
            continue;
        }
        failures.retain(|t| t.elapsed() < Duration::from_secs(120));
        if failures.len() >= 2 {
            log("crashed twice recently; staying on the stock UI");
            continue;
        }
        log("opening OpenU60 screen");
        let started = Instant::now();
        let status = Command::new(&exe).arg("run").status();
        let ok = matches!(&status, Ok(s) if s.success());
        if !ok && started.elapsed() < Duration::from_secs(30) {
            failures.push(Instant::now());
        }
        log(format!("screen exited ({status:?}); back to the stock UI"));
        start_stock();
        while rx.try_recv().is_ok() {}
        let _ = std::fs::remove_file(OPEN_REQUEST);
    }
}

// ---------- run mode ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum View {
    Cover,
    Pages,
}

#[derive(PartialEq)]
enum Menu {
    None,
    Power,
}

/// Slide between two rendered frames.
struct Anim {
    from: Pixmap,
    to: Pixmap,
    start: Instant,
    /// Unit direction the "from" frame leaves in: (-1,0) left, (1,0) right, (0,-1) up, (0,1) down.
    dir: (f32, f32),
    /// Where "from" starts, as a fraction of the screen (lets a drag continue smoothly).
    offset: f32,
}

struct App<'f> {
    fonts: &'f Fonts,
    wall: Pixmap,
    wall_key: (String, Option<SystemTime>),
    status: Arc<Mutex<Status>>,
    settings: Settings,
    settings_mtime: Option<SystemTime>,
    view: View,
    page: usize,
    sub: Option<Sub>,
    menu: Menu,
    awake: bool,
    last_input: Instant,
    hits: Hits,
    overlay_hits: Hits,
    menu_hits: Vec<(u8, [f32; 4])>,
    touch_start: Option<(f32, f32, Instant)>,
    drag: (f32, f32),
    anim: Option<Anim>,
    last_call: bool,
    last_unread: u32,
    presses: Presses,
    exit: Option<&'static str>,
}

const PAGES: usize = pages::TITLES.len();

fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

impl<'f> App<'f> {
    fn snapshot(&self) -> Status {
        let mut s = self.status.lock().unwrap().clone();
        s.speed_bytes = self.settings.speed_bytes;
        s.battery_pct = self.settings.battery_pct;
        s
    }

    /// Draws one screen (no overlays). Records the hit boxes when it is the visible one.
    fn render(&mut self, view: View, page: usize, sub: Option<Sub>, s: &Status, record: bool) -> Canvas<'f> {
        let mut c = Canvas::new(self.fonts);
        c.pm.draw_pixmap(0, 0, self.wall.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        let hits = match view {
            View::Cover => { pages::cover::draw(&mut c, s); Hits::new() }
            View::Pages => match page {
                0 => { pages::network::draw(&mut c, s, page, PAGES); Hits::new() }
                1 => pages::wifi::draw(&mut c, s, page, PAGES),
                2 => pages::charge::draw(&mut c, s, page, PAGES),
                3 => pages::adblock::draw(&mut c, s, page, PAGES),
                4 => pages::pair::draw(&mut c, s, page, PAGES),
                _ => pages::more::draw(&mut c, s, &self.settings, sub, page, PAGES),
            },
        };
        if record {
            self.hits = hits;
        }
        c
    }

    fn frame(&mut self) -> Canvas<'f> {
        let s = self.snapshot();
        // A running slide animation composites two cached frames.
        if let Some(a) = &self.anim {
            let t = (a.start.elapsed().as_secs_f32() * 1000.0 / ANIM_MS).min(1.0);
            let k = a.offset + (1.0 - a.offset) * ease(t);
            let mut c = Canvas::new(self.fonts);
            let (dx, dy) = (a.dir.0 * 320.0 * k, a.dir.1 * 480.0 * k);
            c.pm.draw_pixmap(dx.round() as i32, dy.round() as i32, a.from.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            let (tx, ty) = (dx - a.dir.0 * 320.0, dy - a.dir.1 * 480.0);
            c.pm.draw_pixmap(tx.round() as i32, ty.round() as i32, a.to.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            if t >= 1.0 {
                self.anim = None;
            }
            return c;
        }
        let mut c = self.render(self.view, self.page, self.sub, &s, true);
        // Finger-following drag between pages (or from the cover upward).
        let (ddx, ddy) = self.drag;
        if self.touch_start.is_some() && s.call.is_none() {
            if self.view == View::Pages && self.sub.is_none() && ddx.abs() > 4.0 && ddx.abs() > ddy.abs() {
                let next = if ddx < 0.0 { self.page + 1 } else { self.page.wrapping_sub(1) };
                if next < PAGES {
                    let n = self.render(View::Pages, next, None, &s, false);
                    let mut out = Canvas::new(self.fonts);
                    let dx = ddx.round() as i32;
                    out.pm.draw_pixmap(dx, 0, c.pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    let nx = if ddx < 0.0 { dx + 320 } else { dx - 320 };
                    out.pm.draw_pixmap(nx, 0, n.pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    c = out;
                }
            } else if self.view == View::Cover && ddy < -4.0 {
                let n = self.render(View::Pages, 0, None, &s, false);
                let mut out = Canvas::new(self.fonts);
                let dy = ddy.round() as i32;
                out.pm.draw_pixmap(0, dy, c.pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                out.pm.draw_pixmap(0, dy + 480, n.pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                c = out;
            }
        }
        self.overlay_hits.clear();
        if let Some(call) = &s.call {
            self.overlay_hits = pages::call::draw(&mut c, call);
        }
        if let Some((text, at)) = &s.toast {
            if at.elapsed() < Duration::from_secs(4) {
                let w = c.fonts.measure(FontId::Sans, text, 12.0) + 32.0;
                c.rrect(160.0 - w / 2.0, 404.0, w, 32.0, 16.0, Rgba(20, 26, 40, 235));
                c.text(FontId::Sans, text, 160.0, 425.0, 12.0, palette::TEXT, Align::Center);
            }
        }
        if self.menu == Menu::Power {
            self.draw_power_menu(&mut c);
        }
        if !s.live {
            c.text(FontId::Sans, "正在连接 U60 助手…", 160.0, 476.0, 10.0, palette::FAINT, Align::Center);
        }
        c
    }

    fn draw_power_menu(&mut self, c: &mut Canvas) {
        c.rect(0.0, 0.0, 320.0, 480.0, Rgba(0, 0, 0, 150));
        let (x, y, w) = (40.0, 150.0, 240.0);
        c.rrect(x, y, w, 190.0, 22.0, Rgba(24, 30, 44, 245));
        self.menu_hits.clear();
        let items: [(u8, &str, Rgba); 3] = [(3, "关机", palette::RED), (2, "重启", palette::AMBER), (0, "取消", palette::DIM)];
        for (i, (id, label, col)) in items.iter().enumerate() {
            let by = y + 14.0 + i as f32 * 56.0;
            c.rrect(x + 14.0, by, w - 28.0, 46.0, 14.0, Rgba(255, 255, 255, 26));
            c.text(FontId::Sans, label, 160.0, by + 29.0, 15.0, *col, Align::Center);
            self.menu_hits.push((*id, [x + 14.0, by, w - 28.0, 46.0]));
        }
    }

    /// Starts a slide from the current screen to `next` (already applied to self).
    fn slide(&mut self, prev: (View, usize, Option<Sub>), dir: (f32, f32), offset: f32) {
        let s = self.snapshot();
        let from = self.render(prev.0, prev.1, prev.2, &s, false).pm;
        let to = self.render(self.view, self.page, self.sub, &s, true).pm;
        self.anim = Some(Anim { from, to, start: Instant::now(), dir, offset: offset.clamp(0.0, 0.95) });
    }

    fn refresh_wallpaper(&mut self) {
        let custom = format!("{HOME}/wallpaper.png");
        let key = (self.settings.theme.clone(), settings::mtime(&custom));
        if key != self.wall_key {
            self.wall = wallpaper::for_theme(&self.settings.theme, &custom);
            self.wall_key = key;
        }
    }

    fn reload_settings(&mut self) {
        let m = settings::mtime(&settings::path());
        if m != self.settings_mtime {
            self.settings_mtime = m;
            self.settings = settings::load();
            if self.awake {
                display::set_backlight(self.settings.brightness);
            }
        }
        self.refresh_wallpaper();
    }

    fn save_settings(&mut self) {
        settings::save(&self.settings);
        self.settings_mtime = settings::mtime(&settings::path());
        self.refresh_wallpaper();
    }

    fn wake(&mut self) {
        self.awake = true;
        if self.settings.wake_cover {
            self.view = View::Cover;
            self.sub = None;
        }
        self.menu = Menu::None;
        self.last_input = Instant::now();
    }

    fn sleep(&mut self) {
        self.awake = false;
        self.menu = Menu::None;
        self.anim = None;
    }

    fn tap(&mut self, x: f32, y: f32) {
        let inside = |r: &[f32; 4]| x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3];
        if self.menu == Menu::Power {
            let id = self.menu_hits.iter().find(|(_, r)| inside(r)).map(|(i, _)| *i);
            self.menu = Menu::None;
            match id {
                Some(2) => self.exit = Some("reboot"),
                Some(3) => self.exit = Some("poweroff"),
                _ => {}
            }
            return;
        }
        if let Some(a) = self.overlay_hits.iter().find(|(_, r)| inside(r)).map(|(a, _)| a.clone()) {
            actions::run(a, self.status.clone());
            return;
        }
        if self.status.lock().unwrap().call.is_some() {
            return; // an overlay covers everything else
        }
        match self.view {
            View::Cover => {
                if y > 380.0 {
                    if y < 430.0 && !self.status.lock().unwrap().missed.is_empty() && x > 160.0 {
                        actions::run(Action::DismissMissed, self.status.clone());
                        return;
                    }
                    let prev = (self.view, self.page, self.sub);
                    self.view = View::Pages;
                    self.page = 0;
                    self.slide(prev, (0.0, -1.0), 0.0);
                }
            }
            View::Pages => {
                let Some(action) = self.hits.iter().find(|(_, r)| inside(r)).map(|(a, _)| a.clone()) else { return };
                self.act(action);
            }
        }
    }

    fn act(&mut self, action: Action) {
        let prev = (self.view, self.page, self.sub);
        match action {
            Action::Brightness(d) => {
                self.settings.brightness = (self.settings.brightness as i32 + d).clamp(10, 255) as u8;
                display::set_backlight(self.settings.brightness);
                self.save_settings();
            }
            Action::Timeout(t) => { self.settings.timeout_secs = t; self.save_settings(); }
            Action::Theme(t) => { self.settings.theme = t; self.save_settings(); }
            Action::ToggleWakeCover => { self.settings.wake_cover = !self.settings.wake_cover; self.save_settings(); }
            Action::ToggleBatteryPct => { self.settings.battery_pct = !self.settings.battery_pct; self.save_settings(); }
            Action::ToggleSpeedUnit => { self.settings.speed_bytes = !self.settings.speed_bytes; self.save_settings(); }
            Action::Open(sub) => { self.sub = Some(sub); self.slide(prev, (-1.0, 0.0), 0.0); }
            Action::Back => { self.sub = None; self.slide(prev, (1.0, 0.0), 0.0); }
            Action::StockUi => self.exit = Some("stock"),
            Action::PowerMenu | Action::Reboot | Action::PowerOff => self.menu = Menu::Power,
            other => actions::run(other, self.status.clone()),
        }
    }

    fn handle(&mut self, ev: input::Event) {
        use input::Event::*;
        if let Power(ms) = ev {
            if ms < SHORT_PRESS_MS {
                if self.presses.push() {
                    self.exit = Some("stock");
                }
                // The single press is acted on later, once no second press followed.
            } else if ms >= MENU_MS {
                if !self.awake { self.wake(); }
                self.menu = Menu::Power;
            }
            return;
        }
        if !self.awake {
            if matches!(ev, Up(..)) {
                self.wake();
            }
            self.touch_start = None;
            return;
        }
        self.last_input = Instant::now();
        match ev {
            Power(_) => {}
            Down(x, y) => {
                self.anim = None;
                self.touch_start = Some((x, y, Instant::now()));
                self.drag = (0.0, 0.0);
            }
            Move(x, y) => {
                if let Some((sx, sy, _)) = self.touch_start {
                    if self.menu == Menu::None {
                        self.drag = (x - sx, y - sy);
                    }
                }
            }
            Up(x, y) => {
                let Some((sx, sy, t0)) = self.touch_start.take() else { return };
                let (dx, dy) = (x - sx, y - sy);
                self.drag = (0.0, 0.0);
                let fast = t0.elapsed() < Duration::from_millis(250);
                let blocked = self.menu != Menu::None || self.status.lock().unwrap().call.is_some();
                if dx.abs() < 14.0 && dy.abs() < 14.0 {
                    self.tap(x, y);
                } else if blocked {
                } else if self.view == View::Pages && dx.abs() > dy.abs() && (dx.abs() > 90.0 || (fast && dx.abs() > 30.0)) {
                    let prev = (self.view, self.page, self.sub);
                    if self.sub.is_some() && dx > 0.0 {
                        self.sub = None;
                        self.slide(prev, (1.0, 0.0), 0.0);
                    } else if self.sub.is_none() && dx < 0.0 && self.page + 1 < PAGES {
                        self.page += 1;
                        self.slide(prev, (-1.0, 0.0), dx.abs() / 320.0);
                    } else if self.sub.is_none() && dx > 0.0 && self.page > 0 {
                        self.page -= 1;
                        self.slide(prev, (1.0, 0.0), dx.abs() / 320.0);
                    }
                } else if self.view == View::Cover && (dy < -120.0 || (fast && dy < -40.0)) {
                    let prev = (self.view, self.page, self.sub);
                    self.view = View::Pages;
                    self.page = 0;
                    self.sub = None;
                    self.slide(prev, (0.0, -1.0), dy.abs() / 480.0);
                } else if self.view == View::Pages && dy > 80.0 && sy < 90.0 {
                    let prev = (self.view, self.page, self.sub);
                    self.view = View::Cover;
                    self.slide(prev, (0.0, 1.0), 0.0);
                }
            }
        }
    }
}

/// Draws `frame` under a black veil of `1 - level`, and sets the backlight to `level` of
/// `bright`, stepping from `from` to `to` over `ms` milliseconds.
fn fade(disp: &mut display::Display, fonts: &Fonts, frame: &Pixmap, from: f32, to: f32, bright: u8, ms: u64) {
    let steps = (ms / 28).max(1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let e = 1.0 - (1.0 - t).powi(2);
        let level = from + (to - from) * e;
        let mut c = Canvas::new(fonts);
        c.pm.draw_pixmap(0, 0, frame.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        if level < 0.999 {
            c.rect(0.0, 0.0, 320.0, 480.0, Rgba(0, 0, 0, ((1.0 - level) * 255.0) as u8));
        }
        let _ = disp.present(&c);
        display::set_backlight(((bright as f32) * level.max(0.0)).round().max(if level > 0.0 { 1.0 } else { 0.0 }) as u8);
        std::thread::sleep(Duration::from_millis(28));
    }
}

/// Opening animation: the wallpaper rises out of black while a ring of light draws around
/// the OpenU60 name, then hands over to the first real frame.
fn splash(disp: &mut display::Display, fonts: &Fonts, wall: &Pixmap, bright: u8, first: &Pixmap) {
    let total = 900u128;
    let start = Instant::now();
    loop {
        let ms = start.elapsed().as_millis();
        let t = (ms as f32 / total as f32).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let mut c = Canvas::new(fonts);
        c.pm.draw_pixmap(0, 0, wall.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        c.rect(0.0, 0.0, 320.0, 480.0, Rgba(0, 0, 0, (255.0 * (1.0 - 0.55 * e)) as u8));
        // Ring sweeping clockwise, with a soft halo.
        let r = 54.0 + 6.0 * e;
        c.circle(160.0, 210.0, r + 18.0, palette::BLUE.alpha(0.10 * e));
        c.ring(160.0, 210.0, r, 4.0, e, palette::SKY, palette::PURPLE);
        let size = 26.0 + 6.0 * e;
        c.text(FontId::Sans, "U60 助手", 160.0, 220.0, size, palette::WHITE.alpha(e), Align::Center);
        c.text(FontId::Sans, "U60 Pro", 160.0, 300.0, 12.0, palette::DIM.alpha(e * 0.9), Align::Center);
        let _ = disp.present(&c);
        display::set_backlight(((bright as f32) * (0.15 + 0.85 * e)).round() as u8);
        if t >= 1.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(24));
    }
    std::thread::sleep(Duration::from_millis(180));
    // Cross-fade from the splash into the cover.
    let mut splash_end = Canvas::new(fonts);
    splash_end.pm.draw_pixmap(0, 0, wall.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    splash_end.rect(0.0, 0.0, 320.0, 480.0, Rgba(0, 0, 0, 115));
    for i in 0..=8 {
        let a = i as f32 / 8.0;
        let mut c = Canvas::new(fonts);
        c.pm.draw_pixmap(0, 0, splash_end.pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        let paint = PixmapPaint { opacity: a, ..Default::default() };
        c.pm.draw_pixmap(0, 0, first.as_ref(), &paint, Transform::identity(), None);
        let _ = disp.present(&c);
        std::thread::sleep(Duration::from_millis(26));
    }
}

fn run() -> i32 {
    let t0 = Instant::now();
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    // Only one UI at a time: a second one would fight over the display.
    let run_lock = std::fs::OpenOptions::new().create(true).write(true).open("/tmp/openu60-screen-run.lock");
    if let Ok(f) = &run_lock {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            log("already running");
            return 0;
        }
    }
    let fonts = match (std::fs::read("/usr/ui/fonts/ZTEZhengYuan.ttf"), std::fs::read("/usr/ui/fonts/Oswald-Regular.ttf")) {
        (Ok(a), Ok(b)) => match Fonts::load_owned(a, b) {
            Ok(f) => f,
            Err(e) => { log(format!("fonts: {e}")); return 2; }
        },
        _ => { log("fonts missing"); return 2; }
    };
    log(format!("fonts loaded in {} ms", t0.elapsed().as_millis()));
    let settings = settings::load();
    let custom = format!("{HOME}/wallpaper.png");
    let wall = wallpaper::for_theme(&settings.theme, &custom);
    let status = Arc::new(Mutex::new(Status { down_hist: vec![0.0; 60], up_hist: vec![0.0; 60], ..Default::default() }));
    data::spawn(status.clone());

    // Everything is loaded before the stock UI goes away, so the switch is quick.
    let mut app = App {
        fonts: &fonts, wall, wall_key: (settings.theme.clone(), settings::mtime(&custom)), status, settings,
        settings_mtime: settings::mtime(&settings::path()), view: View::Cover, page: 0, sub: None, menu: Menu::None,
        awake: true, last_input: Instant::now(), hits: Hits::new(), overlay_hits: Hits::new(),
        menu_hits: Vec::new(), touch_start: None, drag: (0.0, 0.0), anim: None, last_call: false, last_unread: u32::MAX,
        presses: Presses { times: Vec::new() }, exit: None,
    };
    let first = app.frame();
    log(format!("first frame ready at {} ms", t0.elapsed().as_millis()));

    if !stop_stock() {
        log("the stock UI did not stop; giving up");
        start_stock();
        return 3;
    }
    let mut disp = None;
    for _ in 0..10 {
        match display::Display::open() {
            Ok(d) => { disp = Some(d); break; }
            Err(e) => { log(format!("display: {e}")); std::thread::sleep(Duration::from_millis(60)); }
        }
    }
    let Some(mut disp) = disp else { start_stock(); return 4; };
    // A black frame first: the panel shows it while the modeset settles.
    let black = Canvas::new(&fonts);
    if let Err(e) = disp.present(&black) {
        log(format!("first frame: {e}"));
        drop(disp);
        start_stock();
        return 5;
    }
    splash(&mut disp, &fonts, &app.wall, app.settings.brightness, &first.pm);
    display::set_backlight(app.settings.brightness);
    log(format!("screen is ours at {} ms", t0.elapsed().as_millis()));

    let (tx, rx) = mpsc::channel();
    { let tx = tx.clone(); std::thread::spawn(move || input::touch(tx, true)); }
    std::thread::spawn(move || input::power(tx));

    let mut last_draw = Instant::now();
    let mut last_reload = Instant::now();
    let mut was_awake = true;
    let mut last_frame: Option<Pixmap> = None;
    while !STOP.load(Ordering::SeqCst) && app.exit.is_none() {
        let mut dirty = false;
        let busy = app.touch_start.is_some() || app.anim.is_some();
        match rx.recv_timeout(Duration::from_millis(if busy { 12 } else { 200 })) {
            Ok(ev) => { app.handle(ev); dirty = true; while let Ok(ev) = rx.try_recv() { app.handle(ev); } }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        if app.presses.settled_single() {
            if app.awake { app.sleep(); } else { app.wake(); }
            dirty = true;
        }
        if last_reload.elapsed() > Duration::from_secs(2) {
            last_reload = Instant::now();
            app.reload_settings();
        }
        let (calling, unread) = { let s = app.status.lock().unwrap(); (s.call.is_some(), s.unread_sms) };
        if app.last_unread == u32::MAX {
            if app.status.lock().unwrap().live { app.last_unread = unread; }
        } else if unread > app.last_unread {
            if !app.awake { app.wake(); }
            app.status.lock().unwrap().toast = Some((format!("{unread} 条新短信，请在手机上查看"), Instant::now()));
            dirty = true;
            app.last_unread = unread;
        } else {
            app.last_unread = unread;
        }
        if calling && !app.last_call {
            if !app.awake { app.wake(); }
            dirty = true;
        }
        app.last_call = calling;
        if calling {
            app.last_input = Instant::now();
            dirty = true;
        }
        if app.awake && app.last_input.elapsed() > Duration::from_secs(app.settings.timeout_secs as u64) {
            app.sleep();
        }
        // Screen on/off: fade the content and ramp the backlight instead of switching hard.
        if was_awake && !app.awake {
            if let Some(f) = &last_frame {
                fade(&mut disp, &fonts, f, 1.0, 0.0, app.settings.brightness, 260);
            }
            display::set_backlight(0);
        } else if !was_awake && app.awake {
            let c = app.frame();
            fade(&mut disp, &fonts, &c.pm, 0.0, 1.0, app.settings.brightness, 300);
            last_frame = Some(c.pm);
            last_draw = Instant::now();
        }
        was_awake = app.awake;
        let animating = app.anim.is_some() || (app.touch_start.is_some() && (app.drag.0.abs() > 4.0 || app.drag.1.abs() > 4.0));
        if app.awake && (dirty || animating || last_draw.elapsed() >= Duration::from_secs(1)) {
            let c = app.frame();
            if let Err(e) = disp.present(&c) {
                log(format!("present: {e}"));
                break;
            }
            last_frame = Some(c.pm);
            last_draw = Instant::now();
        }
    }
    let action = app.exit.unwrap_or("stock");
    log(format!("leaving ({action})"));
    // A last frame tells the user what is happening while the stock UI starts.
    let mut c = Canvas::new(&fonts);
    c.pm.draw_pixmap(0, 0, app.wall.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    let msg = match action { "reboot" => "正在重启…", "poweroff" => "正在关机…", _ => "正在切换到原厂界面…" };
    c.text(FontId::Sans, msg, 160.0, 250.0, 15.0, palette::TEXT, Align::Center);
    if let Some(f) = &last_frame {
        fade(&mut disp, &fonts, f, 1.0, 0.35, app.settings.brightness, 220);
    }
    let _ = disp.present(&c);
    drop(disp);
    start_stock();
    if action == "reboot" || action == "poweroff" {
        std::thread::sleep(Duration::from_secs(3));
        let _ = Command::new(action).status();
    }
    0
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "watch" => watch(),
        "run" => std::process::exit(run()),
        _ => eprintln!("usage: openu60-screen watch|run"),
    }
}
