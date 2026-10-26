//! Pixels of the on-screen indicator: the edge glow, the status pill and
//! the click ring. Buffers are premultiplied ARGB8888 in little-endian byte
//! order (B, G, R, A), what wl_shm expects.

/// Claude's accent orange.
const ACCENT: (u8, u8, u8) = (0xD9, 0x77, 0x57);
const PILL_BG: (u8, u8, u8, u8) = (0x1F, 0x1E, 0x1D, 0xE6);

/// Width of the edge glow in buffer px.
pub const GLOW_WIDTH: u32 = 7;

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Image {
    fn new(width: u32, height: u32) -> Self {
        Image {
            width,
            height,
            data: vec![0; (width * height * 4) as usize],
        }
    }

    /// Paint `color` at `coverage` (0..=1) over the pixel (source-over).
    fn blend(&mut self, x: u32, y: u32, (r, g, b): (u8, u8, u8), alpha: f32) {
        if x >= self.width || y >= self.height || alpha <= 0.0 {
            return;
        }
        let a = alpha.min(1.0);
        let i = ((y * self.width + x) * 4) as usize;
        let px = &mut self.data[i..i + 4];
        for (dst, src) in px.iter_mut().zip([b, g, r, 255]) {
            *dst = (src as f32 * a + *dst as f32 * (1.0 - a)).round() as u8;
        }
    }
}

/// A transparent `width`x`height` frame whose edges glow in the accent
/// colour, fading inwards.
pub fn glow(width: u32, height: u32) -> Image {
    let mut img = Image::new(width, height);
    let w = GLOW_WIDTH;
    for y in 0..height {
        let dy = y.min(height - 1 - y);
        // Rows far from the top and bottom only need their two side bands.
        let xs: Box<dyn Iterator<Item = u32>> = if dy >= w {
            Box::new((0..w.min(width)).chain(width.saturating_sub(w)..width))
        } else {
            Box::new(0..width)
        };
        for x in xs {
            let d = dy.min(x.min(width - 1 - x));
            if d < w {
                let t = 1.0 - d as f32 / w as f32;
                img.blend(x, y, ACCENT, 0.85 * t * t);
            }
        }
    }
    img
}

/// Coverage (0..=1) of a pixel centre by a rounded rectangle, antialiased
/// over one pixel.
fn rounded_rect_coverage(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let cx = x.clamp(r, w - r);
    let cy = y.clamp(r, h - r);
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// The status pill: a dark rounded bar with an accent dot and `text`, laid
/// out at `scale` buffer px per logical px. `font` renders glyphs; without
/// one the pill shows the dot only.
pub fn pill(text: &str, font: Option<&fontdue::Font>, scale: u32) -> Image {
    let s = scale as f32;
    let (height, pad, dot) = (30.0 * s, 13.0 * s, 8.0 * s);
    let size = 14.0 * s;
    let glyphs: Vec<(fontdue::Metrics, Vec<u8>)> = font
        .map(|f| text.chars().map(|c| f.rasterize(c, size)).collect())
        .unwrap_or_default();
    let text_w: f32 = glyphs.iter().map(|(m, _)| m.advance_width).sum();
    let gap = if glyphs.is_empty() { 0.0 } else { 8.0 * s };
    // A whole number of logical px: a buffer's size must divide by its scale.
    let width = ((pad + dot + gap + text_w + pad) / s).ceil() * s;
    let mut img = Image::new(width as u32, height as u32);
    let (bg, bg_a) = ((PILL_BG.0, PILL_BG.1, PILL_BG.2), PILL_BG.3 as f32 / 255.0);
    for y in 0..img.height {
        for x in 0..img.width {
            let c =
                rounded_rect_coverage(x as f32 + 0.5, y as f32 + 0.5, width, height, height / 2.0);
            img.blend(x, y, bg, c * bg_a);
            let (dx, dy) = (
                x as f32 + 0.5 - (pad + dot / 2.0),
                y as f32 + 0.5 - height / 2.0,
            );
            let dc = (dot / 2.0 + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            img.blend(x, y, ACCENT, dc);
        }
    }
    // Baseline so the text's cap height sits centred in the bar.
    let baseline = height / 2.0 + size * 0.35;
    let mut pen = pad + dot + gap;
    for (m, bitmap) in &glyphs {
        let left = (pen + m.xmin as f32).round() as i64;
        let top = (baseline - m.height as f32 - m.ymin as f32).round() as i64;
        for gy in 0..m.height {
            for gx in 0..m.width {
                let cov = bitmap[gy * m.width + gx] as f32 / 255.0;
                let (x, y) = (left + gx as i64, top + gy as i64);
                if x >= 0 && y >= 0 {
                    img.blend(x as u32, y as u32, (0xF5, 0xF4, 0xEF), cov);
                }
            }
        }
        pen += m.advance_width;
    }
    img
}

/// Logical size of the click ring's square.
pub const RING_SIZE: u32 = 44;

/// An accent ring centred in a `RING_SIZE`-square at `scale`.
pub fn ring(scale: u32) -> Image {
    let size = RING_SIZE * scale;
    let mut img = Image::new(size, size);
    let s = scale as f32;
    let (c, r, stroke) = (size as f32 / 2.0, 15.0 * s, 3.0 * s);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let edge = stroke / 2.0 - (d - r).abs();
            img.blend(x, y, ACCENT, (edge + 0.5).clamp(0.0, 1.0));
            // A faint fill so the target point shows on any background.
            if d < r {
                img.blend(x, y, ACCENT, 0.18);
            }
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(img: &Image, x: u32, y: u32) -> u8 {
        img.data[((y * img.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn glow_is_on_the_edges_and_clear_inside() {
        let g = glow(100, 60);
        assert!(alpha(&g, 0, 30) > 150);
        assert!(alpha(&g, 50, 0) > 150);
        assert!(alpha(&g, 99, 59) > 150);
        assert!(alpha(&g, 3, 30) > 0 && alpha(&g, 3, 30) < alpha(&g, 0, 30));
        assert_eq!(alpha(&g, 50, 30), 0);
        assert_eq!(alpha(&g, GLOW_WIDTH, 30), 0);
        // Premultiplied: no channel above alpha.
        assert!(
            g.data
                .chunks(4)
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3])
        );
    }

    #[test]
    fn pill_without_font_is_a_dot_in_a_bar() {
        let p = pill("Claude · click", None, 2);
        assert_eq!(p.height, 60);
        assert_eq!(p.width, (13 + 8 + 13) * 2);
        // With text, still a whole number of logical px.
        let font = None::<&fontdue::Font>;
        assert_eq!(pill("x", font, 3).width % 3, 0);
        // Opaque-ish middle, transparent rounded corner.
        assert!(alpha(&p, p.width / 2, 30) > 200);
        assert_eq!(alpha(&p, 0, 0), 0);
    }

    #[test]
    fn ring_is_hollow_with_a_faint_fill() {
        let r = ring(1);
        assert_eq!((r.width, r.height), (RING_SIZE, RING_SIZE));
        let mid = RING_SIZE / 2;
        assert!(alpha(&r, mid + 15, mid) > 200);
        assert!(alpha(&r, mid, mid) > 0 && alpha(&r, mid, mid) < 80);
        assert_eq!(alpha(&r, 0, 0), 0);
    }
}
