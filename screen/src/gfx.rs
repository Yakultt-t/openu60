//! Drawing on a 320x480 canvas: anti-aliased shapes (tiny-skia), glyphs (ab_glyph) and the
//! few composite widgets the pages share (glass cards, rings, sparklines, toggles).

use std::cell::RefCell;
use std::collections::HashMap;

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Rect, Shader,
    SpreadMode, Stroke, Transform,
};

pub const W: u32 = 320;
pub const H: u32 = 480;

#[derive(Clone, Copy, PartialEq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b, 255)
    }
    pub fn alpha(self, a: f32) -> Self {
        Self(self.0, self.1, self.2, (self.3 as f32 * a).round().clamp(0.0, 255.0) as u8)
    }
    fn color(self) -> Color {
        Color::from_rgba8(self.0, self.1, self.2, self.3)
    }
}

pub mod palette {
    use super::Rgba;
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    pub const TEXT: Rgba = Rgba::rgb(242, 245, 250);
    pub const DIM: Rgba = Rgba::rgb(160, 168, 184);
    pub const FAINT: Rgba = Rgba::rgb(110, 118, 134);
    pub const BLUE: Rgba = Rgba::rgb(64, 132, 255);
    pub const SKY: Rgba = Rgba::rgb(96, 170, 255);
    pub const GREEN: Rgba = Rgba::rgb(52, 211, 120);
    pub const MINT: Rgba = Rgba::rgb(64, 220, 190);
    pub const AMBER: Rgba = Rgba::rgb(255, 190, 70);
    pub const ORANGE: Rgba = Rgba::rgb(255, 150, 60);
    pub const RED: Rgba = Rgba::rgb(255, 95, 90);
    pub const PURPLE: Rgba = Rgba::rgb(170, 120, 255);
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontId {
    /// CJK + Latin text.
    Sans,
    /// Condensed digits for big numbers (clock, speeds).
    Digits,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Glyph bitmap placement in fontdue's convention: `ymin` is the bottom of the bitmap above
/// the baseline (negative when it hangs below), `xmin` the left bearing.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

/// Fonts are parsed lazily (ab_glyph), so even the 15 MB system CJK font opens instantly.
pub struct Fonts {
    sans: ab_glyph::FontVec,
    digits: ab_glyph::FontVec,
    cache: RefCell<HashMap<(FontId, char, u32), (Metrics, Vec<u8>)>>,
}

fn em_scale(font: &ab_glyph::FontVec, size: f32) -> ab_glyph::PxScale {
    use ab_glyph::Font;
    let upem = font.units_per_em().unwrap_or(1000.0);
    ab_glyph::PxScale::from(size * font.height_unscaled() / upem)
}

impl Fonts {
    pub fn load(sans: &[u8], digits: &[u8]) -> Result<Self, String> {
        Ok(Self {
            sans: ab_glyph::FontVec::try_from_vec(sans.to_vec()).map_err(|e| e.to_string())?,
            digits: ab_glyph::FontVec::try_from_vec(digits.to_vec()).map_err(|e| e.to_string())?,
            cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn load_owned(sans: Vec<u8>, digits: Vec<u8>) -> Result<Self, String> {
        Ok(Self {
            sans: ab_glyph::FontVec::try_from_vec(sans).map_err(|e| e.to_string())?,
            digits: ab_glyph::FontVec::try_from_vec(digits).map_err(|e| e.to_string())?,
            cache: RefCell::new(HashMap::new()),
        })
    }

    fn font(&self, id: FontId, ch: char) -> (&ab_glyph::FontVec, FontId) {
        use ab_glyph::Font;
        match id {
            FontId::Digits if self.digits.glyph_id(ch).0 != 0 => (&self.digits, FontId::Digits),
            _ => (&self.sans, FontId::Sans),
        }
    }

    fn advance(&self, id: FontId, ch: char, size: f32) -> f32 {
        use ab_glyph::{Font, ScaleFont};
        let (font, _) = self.font(id, ch);
        font.as_scaled(em_scale(font, size)).h_advance(font.glyph_id(ch))
    }

    fn glyph(&self, id: FontId, ch: char, size: f32) -> (Metrics, Vec<u8>) {
        use ab_glyph::Font;
        let (font, real) = self.font(id, ch);
        let key = (real, ch, (size * 4.0) as u32);
        if let Some(g) = self.cache.borrow().get(&key) {
            return g.clone();
        }
        let advance = self.advance(id, ch, size);
        let glyph = font.glyph_id(ch).with_scale_and_position(em_scale(font, size), ab_glyph::point(0.0, 0.0));
        let out = match font.outline_glyph(glyph) {
            Some(og) => {
                let b = og.px_bounds();
                let (w, h) = (b.width().ceil() as usize, b.height().ceil() as usize);
                let mut mask = vec![0u8; w * h];
                og.draw(|x, y, c| {
                    let i = y as usize * w + x as usize;
                    if i < mask.len() {
                        mask[i] = (c * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                });
                (Metrics { xmin: b.min.x.floor() as i32, ymin: -(b.max.y.ceil() as i32), width: w, height: h, advance_width: advance }, mask)
            }
            None => (Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: advance }, Vec::new()),
        };
        let mut cache = self.cache.borrow_mut();
        if cache.len() > 4000 {
            cache.clear();
        }
        cache.insert(key, out.clone());
        out
    }

    pub fn measure(&self, id: FontId, s: &str, size: f32) -> f32 {
        s.chars().map(|c| self.advance(id, c, size)).sum()
    }
}

pub struct Canvas<'f> {
    pub pm: Pixmap,
    pub fonts: &'f Fonts,
}

pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.5523 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn solid(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c.color());
    p.anti_alias = true;
    p
}

pub fn vgradient(y0: f32, y1: f32, stops: &[(f32, Rgba)]) -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(0.0, y0),
        Point::from_xy(0.0, y1),
        stops.iter().map(|(p, c)| GradientStop::new(*p, c.color())).collect(),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(stops[0].1.color()))
}

pub fn hgradient(x0: f32, x1: f32, stops: &[(f32, Rgba)]) -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(x0, 0.0),
        Point::from_xy(x1, 0.0),
        stops.iter().map(|(p, c)| GradientStop::new(*p, c.color())).collect(),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(Shader::SolidColor(stops[0].1.color()))
}

impl<'f> Canvas<'f> {
    pub fn new(fonts: &'f Fonts) -> Self {
        Self { pm: Pixmap::new(W, H).unwrap(), fonts }
    }

    pub fn fill_path(&mut self, path: &Path, c: Rgba) {
        self.pm.fill_path(path, &solid(c), FillRule::Winding, Transform::identity(), None);
    }

    pub fn fill_path_shader(&mut self, path: &Path, shader: Shader) {
        let mut p = Paint::default();
        p.shader = shader;
        p.anti_alias = true;
        self.pm.fill_path(path, &p, FillRule::Winding, Transform::identity(), None);
    }

    pub fn stroke_path(&mut self, path: &Path, c: Rgba, width: f32) {
        let stroke = Stroke { width, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
        self.pm.stroke_path(path, &solid(c), &stroke, Transform::identity(), None);
    }

    pub fn stroke_path_shader(&mut self, path: &Path, shader: Shader, width: f32) {
        let mut p = Paint::default();
        p.shader = shader;
        p.anti_alias = true;
        let stroke = Stroke { width, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
        self.pm.stroke_path(path, &p, &stroke, Transform::identity(), None);
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
        if let Some(r) = Rect::from_xywh(x, y, w, h) {
            self.pm.fill_rect(r, &solid(c), Transform::identity(), None);
        }
    }

    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        self.fill_path(&rounded_rect(x, y, w, h, r), c);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        if let Some(p) = PathBuilder::from_circle(cx, cy, r) {
            self.fill_path(&p, c);
        }
    }

    /// Frosted-glass card: dark translucent body, a faint light rim and a soft top sheen.
    pub fn glass(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let body = rounded_rect(x, y, w, h, r);
        self.fill_path_shader(
            &body,
            vgradient(y, y + h, &[(0.0, Rgba(30, 36, 52, 150)), (1.0, Rgba(14, 18, 30, 170))]),
        );
        self.stroke_path_shader(
            &rounded_rect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r),
            vgradient(y, y + h, &[(0.0, Rgba(255, 255, 255, 46)), (0.5, Rgba(255, 255, 255, 14)), (1.0, Rgba(255, 255, 255, 26))]),
            1.0,
        );
    }

    /// Text with its baseline at `y`; returns the advance width.
    pub fn text(&mut self, font: FontId, s: &str, x: f32, y: f32, size: f32, c: Rgba, align: Align) -> f32 {
        let width = self.fonts.measure(font, s, size);
        let mut pen = match align {
            Align::Left => x,
            Align::Center => x - width / 2.0,
            Align::Right => x - width,
        };
        for ch in s.chars() {
            let (m, bmp) = self.fonts.glyph(font, ch, size);
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (y - m.height as f32 - m.ymin as f32).round() as i32;
            self.blit_mask(gx, gy, m.width, m.height, &bmp, c);
            pen += m.advance_width;
        }
        width
    }

    fn blit_mask(&mut self, x0: i32, y0: i32, w: usize, h: usize, mask: &[u8], c: Rgba) {
        let data = self.pm.data_mut();
        for row in 0..h {
            let y = y0 + row as i32;
            if y < 0 || y >= H as i32 {
                continue;
            }
            for col in 0..w {
                let x = x0 + col as i32;
                if x < 0 || x >= W as i32 {
                    continue;
                }
                let a = mask[row * w + col] as u32 * c.3 as u32 / 255;
                if a == 0 {
                    continue;
                }
                let i = (y as usize * W as usize + x as usize) * 4;
                let inv = 255 - a;
                // Premultiplied source-over.
                data[i] = ((c.0 as u32 * a + data[i] as u32 * inv) / 255) as u8;
                data[i + 1] = ((c.1 as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
                data[i + 2] = ((c.2 as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
                data[i + 3] = (a + data[i + 3] as u32 * inv / 255) as u8;
            }
        }
    }

    /// Ring gauge: a faint full track and a coloured arc from 12 o'clock, clockwise.
    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, frac: f32, from: Rgba, to: Rgba) {
        if let Some(track) = PathBuilder::from_circle(cx, cy, r) {
            self.stroke_path(&track, Rgba(255, 255, 255, 34), width);
        }
        let frac = frac.clamp(0.0, 1.0);
        if frac <= 0.001 {
            return;
        }
        let steps = (96.0 * frac).max(4.0) as usize;
        let mut pb = PathBuilder::new();
        for i in 0..=steps {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * i as f32 / steps as f32;
            let (px, py) = (cx + r * a.cos(), cy + r * a.sin());
            if i == 0 { pb.move_to(px, py) } else { pb.line_to(px, py) }
        }
        if let Some(arc) = pb.finish() {
            self.stroke_path_shader(&arc, vgradient(cy - r, cy + r, &[(0.0, from), (1.0, to)]), width);
        }
        // Bright head dot at the end of the arc.
        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac;
        let (hx, hy) = (cx + r * a.cos(), cy + r * a.sin());
        self.circle(hx, hy, width * 0.9, Rgba(255, 255, 255, 60));
        self.circle(hx, hy, width * 0.55, palette::WHITE);
    }

    /// Area chart of `values` (oldest first) inside the box, scaled to `max` (auto if 0).
    pub fn sparkline(&mut self, x: f32, y: f32, w: f32, h: f32, values: &[f32], max: f32, line: Rgba, glow: bool) {
        if values.len() < 2 {
            return;
        }
        let top = if max > 0.0 { max } else { values.iter().cloned().fold(0.0, f32::max).max(1.0) * 1.15 };
        let pts: Vec<(f32, f32)> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (x + w * i as f32 / (values.len() - 1) as f32, y + h - h * (v / top).clamp(0.0, 1.0)))
            .collect();
        let mut area = PathBuilder::new();
        area.move_to(pts[0].0, y + h);
        for p in &pts {
            area.line_to(p.0, p.1);
        }
        area.line_to(pts[pts.len() - 1].0, y + h);
        area.close();
        if let Some(a) = area.finish() {
            self.fill_path_shader(&a, vgradient(y, y + h, &[(0.0, line.alpha(0.45)), (1.0, line.alpha(0.0))]));
        }
        let mut pb = PathBuilder::new();
        for (i, p) in pts.iter().enumerate() {
            if i == 0 { pb.move_to(p.0, p.1) } else { pb.line_to(p.0, p.1) }
        }
        if let Some(l) = pb.finish() {
            if glow {
                self.stroke_path(&l, line.alpha(0.25), 5.0);
            }
            self.stroke_path(&l, line, 1.6);
        }
        if glow {
            let last = pts[pts.len() - 1];
            self.circle(last.0 - 2.0, last.1, 4.5, line.alpha(0.35));
            self.circle(last.0 - 2.0, last.1, 2.5, palette::WHITE);
        }
    }

    /// iOS-style switch, 44x26 at (x, y).
    pub fn toggle(&mut self, x: f32, y: f32, on: bool) {
        let track = if on { palette::BLUE } else { Rgba(255, 255, 255, 50) };
        self.rrect(x, y, 44.0, 26.0, 13.0, track);
        let kx = if on { x + 31.0 } else { x + 13.0 };
        self.circle(kx, y + 13.0, 10.5, palette::WHITE);
    }

    /// Segmented level bar like the battery meter in the references.
    pub fn level_bar(&mut self, x: f32, y: f32, w: f32, h: f32, frac: f32, on: Rgba) {
        let n = 26;
        let gap = 2.0;
        let seg = (w - gap * (n as f32 - 1.0)) / n as f32;
        let lit = (frac.clamp(0.0, 1.0) * n as f32).round() as usize;
        for i in 0..n {
            let c = if i < lit { on } else { Rgba(255, 255, 255, 40) };
            self.rrect(x + i as f32 * (seg + gap), y, seg, h, 1.5, c);
        }
    }

    /// Page dots: the current one is a pill.
    pub fn page_dots(&mut self, cx: f32, y: f32, count: usize, current: usize) {
        let total: f32 = (0..count).map(|i| if i == current { 16.0 } else { 5.0 }).sum::<f32>() + 5.0 * (count as f32 - 1.0);
        let mut x = cx - total / 2.0;
        for i in 0..count {
            let w = if i == current { 16.0 } else { 5.0 };
            let c = if i == current { palette::WHITE } else { Rgba(255, 255, 255, 90) };
            self.rrect(x, y, w, 5.0, 2.5, c);
            x += w + 5.0;
        }
    }

    /// Copies the canvas into an RGB565 scanout buffer, rotated 180° (the panel is mounted
    /// upside down).
    pub fn to_rgb565(&self, out: &mut [u8], pitch: usize, rotate: bool) {
        let src = self.pm.data();
        for y in 0..H as usize {
            for x in 0..W as usize {
                let (sx, sy) = if rotate { (W as usize - 1 - x, H as usize - 1 - y) } else { (x, y) };
                let i = (sy * W as usize + sx) * 4;
                let (r, g, b) = (src[i] as u16, src[i + 1] as u16, src[i + 2] as u16);
                let v = ((r & 0xf8) << 8) | ((g & 0xfc) << 3) | (b >> 3);
                let o = y * pitch + x * 2;
                out[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}
