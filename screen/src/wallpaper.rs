//! The default wallpaper: deep space with stars above the curved limb of the Earth, a blue
//! atmosphere glow along the horizon brightening toward a sunrise on the right, and faint
//! city lights on the night side. Generated, so no image rights are involved.

use crate::gfx::{H, W};
use tiny_skia::Pixmap;

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn noise(x: i32, y: i32, seed: u32) -> f32 {
    (hash((x as u32).wrapping_mul(73856093) ^ (y as u32).wrapping_mul(19349663) ^ seed) & 0xffff) as f32 / 65535.0
}

/// Smooth value noise in [0, 1] at a given cell size.
fn smooth(x: f32, y: f32, cell: f32, seed: u32) -> f32 {
    let (fx, fy) = (x / cell, y / cell);
    let (ix, iy) = (fx.floor() as i32, fy.floor() as i32);
    let (tx, ty) = (fx - ix as f32, fy - iy as f32);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let a = noise(ix, iy, seed);
    let b = noise(ix + 1, iy, seed);
    let c = noise(ix, iy + 1, seed);
    let d = noise(ix + 1, iy + 1, seed);
    let top = a + (b - a) * s(tx);
    let bot = c + (d - c) * s(tx);
    top + (bot - top) * s(ty)
}

pub fn earth() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    // The Earth is a big circle below and to the right; its limb rises from the left edge
    // (~y 370) to the right edge (~y 205).
    let (ecx, ecy, er) = (400.0f32, 760.0f32, 560.0f32);
    let (sun_x, sun_y) = (338.0f32, 196.0f32);
    let data = pm.data_mut();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dist = ((fx - ecx).powi(2) + (fy - ecy).powi(2)).sqrt();
            let d = dist - er; // >0 above the limb (space), <0 on the planet
            // How far along the limb we are, 0 at the left edge, 1 at the right.
            let along = (fx / W as f32).clamp(0.0, 1.0);
            let sun = (-(((fx - sun_x).powi(2) + (fy - sun_y).powi(2)).sqrt()) / 70.0).exp();
            let (mut r, mut g, mut b);
            if d > 0.0 {
                // Space: near-black navy, a little lighter toward the horizon.
                let t = fy / H as f32;
                r = 3.0 + 6.0 * t;
                g = 5.0 + 9.0 * t;
                b = 12.0 + 24.0 * t;
                // Stars, thinning out close to the bright horizon.
                let n = noise(x, y, 7);
                if n > 0.9965 {
                    let bright = (n - 0.9965) / 0.0035;
                    let k = 120.0 + 135.0 * bright * (d / 60.0).clamp(0.2, 1.0);
                    r += k * 0.9;
                    g += k * 0.95;
                    b += k;
                } else if n > 0.990 {
                    let k = 40.0 * (d / 80.0).clamp(0.0, 1.0);
                    r += k;
                    g += k;
                    b += k * 1.1;
                }
                // Atmosphere: a thin bright rim and a wide soft glow, stronger to the right.
                let rim = (-d / 3.2).exp();
                let haze = (-d / 30.0).exp();
                let strength = 0.45 + 0.75 * along.powf(1.3);
                r += (90.0 * rim + 22.0 * haze) * strength;
                g += (175.0 * rim + 80.0 * haze) * strength;
                b += (255.0 * rim + 190.0 * haze) * strength;
            } else {
                // Night side of the planet: very dark blue, a faint cyan band under the limb.
                let depth = -d;
                r = 2.0;
                g = 6.0 + 10.0 * (-depth / 40.0).exp();
                b = 14.0 + 40.0 * (-depth / 30.0).exp();
                let under = ((-depth / 4.0).exp() + 0.35 * (-depth / 18.0).exp()) * (0.45 + 0.75 * along);
                r += 40.0 * under;
                g += 110.0 * under;
                b += 220.0 * under;
                // City lights: clusters of tiny dots, denser near the limb, foreshortened.
                if depth > 8.0 && depth < 260.0 {
                    let region = smooth(fx, fy * 1.6, 38.0, 11) * smooth(fx, fy * 1.6, 13.0, 5);
                    let dot = noise(x, y, 23);
                    if region > 0.40 && dot > 0.955 - (region - 0.40) * 0.2 {
                        let k = 50.0 + 90.0 * noise(x, y, 41);
                        let fade = (1.0 - depth / 260.0).clamp(0.0, 1.0);
                        r += k * fade;
                        g += k * 0.95 * fade;
                        b += k * 0.9 * fade;
                    }
                }
            }
            // Sunrise glow on the right end of the horizon.
            r += 255.0 * sun * 0.85;
            g += 215.0 * sun * 0.7;
            b += 170.0 * sun * 0.6;
            let i = (y as usize * W as usize + x as usize) * 4;
            data[i] = r.clamp(0.0, 255.0) as u8;
            data[i + 1] = g.clamp(0.0, 255.0) as u8;
            data[i + 2] = b.clamp(0.0, 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    pm
}

/// Stars only, with a faint violet nebula: the calmest theme.
pub fn space() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    let data = pm.data_mut();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let (fx, fy) = (x as f32, y as f32);
            let t = fy / H as f32;
            let mut r = 4.0 + 4.0 * t;
            let mut g = 4.0 + 5.0 * t;
            let mut b = 10.0 + 14.0 * t;
            let neb = smooth(fx, fy, 90.0, 3) * smooth(fx, fy, 37.0, 9);
            r += 60.0 * neb * neb;
            g += 20.0 * neb * neb;
            b += 90.0 * neb * neb;
            let n = noise(x, y, 17);
            if n > 0.997 {
                let k = 140.0 + 115.0 * (n - 0.997) / 0.003;
                r += k;
                g += k;
                b += k;
            } else if n > 0.991 {
                r += 35.0;
                g += 35.0;
                b += 45.0;
            }
            let i = (y as usize * W as usize + x as usize) * 4;
            data[i] = r.clamp(0.0, 255.0) as u8;
            data[i + 1] = g.clamp(0.0, 255.0) as u8;
            data[i + 2] = b.clamp(0.0, 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    pm
}

/// Green and violet curtains over a dark sky.
pub fn aurora() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    let data = pm.data_mut();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let (fx, fy) = (x as f32, y as f32);
            let t = fy / H as f32;
            let mut r = 3.0 + 6.0 * t;
            let mut g = 6.0 + 10.0 * t;
            let mut b = 14.0 + 20.0 * t;
            // Two wavy bands, brightest along their lower edge, with vertical streaks.
            for (k, (base, amp, col)) in [(170.0f32, 40.0f32, [40.0f32, 230.0, 150.0]), (250.0, 30.0, [150.0, 90.0, 255.0])].iter().enumerate() {
                let centre = base + amp * (fx / 70.0 + k as f32 * 1.7).sin() + 14.0 * (fx / 23.0).sin();
                let d = fy - centre;
                let curtain = if d < 0.0 { (d / 60.0).exp() } else { (-d / 8.0).exp() };
                let streak = 0.55 + 0.45 * smooth(fx * 3.0, 0.0, 9.0, 31 + k as u32);
                let a = curtain * streak * 0.55;
                r += col[0] * a;
                g += col[1] * a;
                b += col[2] * a;
            }
            let n = noise(x, y, 29);
            if n > 0.997 && fy < 330.0 {
                r += 150.0;
                g += 150.0;
                b += 160.0;
            }
            // Dark hills at the bottom.
            let hill = 400.0 + 18.0 * (fx / 55.0).sin() + 10.0 * (fx / 17.0).cos();
            if fy > hill {
                r = 2.0;
                g = 4.0;
                b = 8.0;
            }
            let i = (y as usize * W as usize + x as usize) * 4;
            data[i] = r.clamp(0.0, 255.0) as u8;
            data[i + 1] = g.clamp(0.0, 255.0) as u8;
            data[i + 2] = b.clamp(0.0, 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    pm
}

/// The wallpaper for a theme; "custom" loads the picture sent from the app (darkened a
/// little so the cards stay readable) and falls back to the Earth.
pub fn for_theme(theme: &str, custom_path: &str) -> Pixmap {
    match theme {
        "space" => space(),
        "aurora" => aurora(),
        "custom" => match Pixmap::load_png(custom_path) {
            Ok(mut p) if p.width() == W && p.height() == H => {
                for px in p.data_mut().chunks_exact_mut(4) {
                    for c in &mut px[..3] {
                        *c = (*c as f32 * 0.72) as u8;
                    }
                }
                p
            }
            _ => earth(),
        },
        _ => earth(),
    }
}
