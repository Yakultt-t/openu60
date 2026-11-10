//! Renders the pages with mock data to PNG files (2x) for design review on a Mac.
//! Usage: screen-preview <sans.ttf> <digits.ttf> <out dir>

use openu60_screen::gfx::{Canvas, Fonts};
use openu60_screen::{model::Status, pages, wallpaper};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let fonts = Fonts::load(&std::fs::read(&a[1]).unwrap(), &std::fs::read(&a[2]).unwrap()).unwrap();
    let out = std::path::PathBuf::from(&a[3]);
    std::fs::create_dir_all(&out).unwrap();
    let wall = match std::env::var("THEME").as_deref() { Ok("space") => wallpaper::space(), Ok("aurora") => wallpaper::aurora(), _ => wallpaper::earth() };
    let s = Status::mock();
    let pages: Vec<(&str, Box<dyn Fn(&mut Canvas)>)> = vec![
        ("1-cover", Box::new(|c: &mut Canvas| pages::cover::draw(c, &s))),
        ("2-network", Box::new(|c: &mut Canvas| pages::network::draw(c, &s, 0, 5))),
        ("3-wifi", Box::new(|c: &mut Canvas| { pages::wifi::draw(c, &s, 1, 5); })),
        ("4-charge", Box::new(|c: &mut Canvas| { pages::charge::draw(c, &s, 2, 5); })),
        ("5-adblock", Box::new(|c: &mut Canvas| { pages::adblock::draw(c, &s, 3, 6); })),
        ("5-pair", Box::new(|c: &mut Canvas| { pages::pair::draw(c, &s, 4, 6); })),
        ("6-settings", Box::new(|c: &mut Canvas| { pages::more::draw(c, &s, &openu60_screen::settings::Settings::default(), None, 5, 6); })),
        ("6b-appearance", Box::new(|c: &mut Canvas| { pages::more::draw(c, &s, &openu60_screen::settings::Settings::default(), Some(pages::Sub::Appearance), 5, 6); })),
        ("6c-display", Box::new(|c: &mut Canvas| { pages::more::draw(c, &s, &openu60_screen::settings::Settings::default(), Some(pages::Sub::Display), 5, 6); })),
        ("6d-advanced", Box::new(|c: &mut Canvas| { pages::more::draw(c, &s, &openu60_screen::settings::Settings::default(), Some(pages::Sub::Advanced), 5, 6); })),
        ("7-call", Box::new(|c: &mut Canvas| { pages::network::draw(c, &s, 0, 5); pages::call::draw(c, &openu60_screen::model::Call { number: "138 0013 8000".into(), state: "来电", since: std::time::Instant::now() }); })),
    ];
    for (name, draw) in pages {
        let mut c = Canvas::new(&fonts);
        c.pm.draw_pixmap(0, 0, wall.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
        draw(&mut c);
        // 2x nearest-neighbour upscale so the preview shows real pixels.
        let mut big = tiny_skia::Pixmap::new(640, 960).unwrap();
        big.draw_pixmap(0, 0, c.pm.as_ref(), &tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Nearest, ..Default::default() }, tiny_skia::Transform::from_scale(2.0, 2.0), None);
        big.save_png(out.join(format!("{name}.png"))).unwrap();
    }
    println!("ok");
}
