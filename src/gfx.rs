//! Tiny anti-aliased software rasteriser based on signed distance functions.
//! Used for both the popup and the tray icon, so we need no GDI+/Direct2D.

use std::f32::consts::FRAC_PI_4;

/// A 0x00RRGGBB pixel buffer (the memory layout of a 32-bit top-down DIB).
pub struct Canvas<'a> {
    pub w: i32,
    pub h: i32,
    pub px: &'a mut [u32],
}

/// Pixel coverage for a signed distance measured from the pixel centre.
fn cov(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

impl Canvas<'_> {
    pub fn clear(&mut self, c: u32) {
        self.px.fill(c);
    }

    fn blend(&mut self, i: usize, c: u32, a: f32) {
        if a <= 0.0 {
            return;
        }
        if a >= 1.0 {
            self.px[i] = c;
            return;
        }
        let a = (a * 256.0) as u32;
        let d = self.px[i];
        let mix = |s: u32| ((((c >> s) & 255) * a + ((d >> s) & 255) * (256 - a)) >> 8) << s;
        self.px[i] = mix(16) | mix(8) | mix(0);
    }

    /// Fills the shape described by `sd` (coordinates relative to `cx`,`cy`)
    /// inside the box of half-extents `hx`,`hy`.
    fn shade(&mut self, cx: f32, cy: f32, hx: f32, hy: f32, c: u32, sd: impl Fn(f32, f32) -> f32) {
        let x0 = ((cx - hx - 1.0).floor() as i32).max(0);
        let x1 = ((cx + hx + 1.0).ceil() as i32).min(self.w);
        let y0 = ((cy - hy - 1.0).floor() as i32).max(0);
        let y1 = ((cy + hy + 1.0).ceil() as i32).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let a = cov(sd(x as f32 + 0.5 - cx, y as f32 + 0.5 - cy));
                self.blend((y * self.w + x) as usize, c, a);
            }
        }
    }

    pub fn rrect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: u32) {
        let (hx, hy) = ((x1 - x0) / 2.0, (y1 - y0) / 2.0);
        if hx <= 0.0 || hy <= 0.0 {
            return;
        }
        let r = r.min(hx).min(hy);
        self.shade(x0 + hx, y0 + hy, hx, hy, c, |x, y| sd_rbox(x, y, hx, hy, r));
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: u32) {
        self.shade(cx, cy, r, r, c, |x, y| x.hypot(y) - r);
    }

    pub fn sun(&mut self, cx: f32, cy: f32, size: f32, rays: f32, c: u32) {
        let h = size / 2.0;
        self.shade(cx, cy, h, h, c, |x, y| sun_sd(x, y, size, rays));
    }
}

fn sd_rbox(x: f32, y: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = x.abs() - hx + r;
    let qy = y.abs() - hy + r;
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

fn sd_segment(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let (px, py, dx, dy) = (x - ax, y - ay, bx - ax, by - ay);
    let t = ((px * dx + py * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (px - dx * t).hypot(py - dy * t) - r
}

/// Sun glyph of overall `size`, centred on the origin. `rays` (0..=1) sets
/// the ray length, so the glyph can mirror the current brightness.
fn sun_sd(x: f32, y: f32, size: f32, rays: f32) -> f32 {
    let s = size / 2.0;
    let mut d = x.hypot(y) - s * 0.38;
    let t = s * 0.095;
    let r0 = s * 0.6;
    let r1 = r0 + s * (0.06 + 0.24 * rays.clamp(0.0, 1.0));
    for k in 0..8 {
        let (sn, cs) = (k as f32 * FRAC_PI_4).sin_cos();
        d = d.min(sd_segment(x, y, cs * r0, sn * r0, cs * r1, sn * r1, t));
    }
    d
}

/// Straight-alpha ARGB pixels for a `size`×`size` sun icon in colour `rgb`.
pub fn sun_icon(size: i32, rgb: u32) -> Vec<u32> {
    let c = size as f32 / 2.0;
    (0..size * size)
        .map(|i| {
            let (x, y) = ((i % size) as f32 + 0.5 - c, (i / size) as f32 + 0.5 - c);
            let a = cov(sun_sd(x, y, size as f32, 1.0));
            (((a * 255.0 + 0.5) as u32) << 24) | rgb
        })
        .collect()
}
