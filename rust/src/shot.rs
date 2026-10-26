//! Screenshot post-processing shared by every backend: shrink a capture to
//! a size limit and encode it once, as a PNG for an MCP image block.

use std::io::BufReader;

use crate::backend::R;

/// An 8-bit image, `channels` bytes per pixel, rows packed.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub data: Vec<u8>,
}

pub fn read_png(path: &str) -> R<Pixels> {
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut dec = png::Decoder::new(BufReader::new(file));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or("PNG too large to decode")?;
    let mut data = vec![0; size];
    let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
    data.truncate(info.buffer_size());
    let channels = info.color_type.samples();
    // Rows can be padded past width * channels; repack them tightly.
    let row = info.width as usize * channels;
    if info.line_size != row {
        data = data
            .chunks(info.line_size)
            .flat_map(|r| r[..row].to_vec())
            .collect();
    }
    Ok(Pixels {
        width: info.width,
        height: info.height,
        channels,
        data,
    })
}

pub fn write_png(img: &Pixels) -> R<Vec<u8>> {
    let color = match img.channels {
        1 => png::ColorType::Grayscale,
        2 => png::ColorType::GrayscaleAlpha,
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        n => return Err(format!("unsupported channel count {n}")),
    };
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, img.width, img.height);
    enc.set_color(color);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()
        .and_then(|mut w| w.write_image_data(&img.data))
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Size that fits in `max_w` x `max_h` keeping the aspect ratio; never
/// larger than the original.
pub fn fit((w, h): (u32, u32), max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    (
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
    )
}

/// Fixed-point unit of the shrink weights: the shares of the source pixels
/// under one output pixel add up to exactly this.
const ONE: u32 = 1 << 16;

/// Per output pixel along one axis: the first source pixel it covers and
/// the share of each covered source pixel, in units of ONE.
fn spans(src: usize, dst: usize) -> Vec<(usize, Vec<u32>)> {
    let step = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let (a, b) = (i as f64 * step, ((i + 1) as f64 * step).min(src as f64));
            let first = (a.floor() as usize).min(src - 1);
            let end = (b.ceil() as usize).clamp(first + 1, src);
            let mut shares: Vec<u32> = (first..end)
                .map(|s| {
                    let cover = (b.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0);
                    (cover / (b - a) * f64::from(ONE)).round() as u32
                })
                .collect();
            // Rounding leaves the sum a little off: settle it on the biggest
            // share, so a flat area keeps its exact value.
            let sum: u32 = shares.iter().sum();
            let big = (0..shares.len()).max_by_key(|&k| shares[k]).unwrap_or(0);
            shares[big] = (shares[big] + ONE).saturating_sub(sum);
            (first, shares)
        })
        .collect()
}

/// Horizontal pass for C channels: each source row to `w` pixels, kept
/// with 8 fractional bits (at most 255 << 8, so u16 holds it).
fn shrink_rows<const C: usize>(img: &Pixels, xs: &[(usize, Vec<u32>)]) -> Vec<u16> {
    let (sw, w) = (img.width as usize, xs.len());
    let mut out = vec![0u16; img.height as usize * w * C];
    for (row, dst) in img
        .data
        .chunks_exact(sw * C)
        .zip(out.chunks_exact_mut(w * C))
    {
        for ((first, shares), px) in xs.iter().zip(dst.as_chunks_mut::<C>().0) {
            let mut acc = [0u32; C];
            let src = &row[first * C..(first + shares.len()) * C];
            for (s, &share) in src.as_chunks::<C>().0.iter().zip(shares) {
                for (a, &v) in acc.iter_mut().zip(s) {
                    *a += u32::from(v) * share;
                }
            }
            for (p, a) in px.iter_mut().zip(acc) {
                *p = ((a + 128) >> 8) as u16;
            }
        }
    }
    out
}

/// Shrink by averaging each output pixel's source area (area-weighted, so
/// thin lines and text stay readable instead of aliasing away). The box
/// filter is separable: rows first, then columns, in fixed point.
pub fn shrink(img: &Pixels, w: u32, h: u32) -> Pixels {
    let (sh, c) = (img.height as usize, img.channels);
    let (w, h) = (w as usize, h as usize);
    let xs = spans(img.width as usize, w);
    let rows = match c {
        1 => shrink_rows::<1>(img, &xs),
        2 => shrink_rows::<2>(img, &xs),
        3 => shrink_rows::<3>(img, &xs),
        _ => shrink_rows::<4>(img, &xs),
    };
    let line = w * c;
    let mut data = vec![0u8; h * line];
    // Max (255 << 8) * ONE plus rounding still fits u32.
    let mut acc = vec![0u32; line];
    for ((first, shares), dst) in spans(sh, h).iter().zip(data.chunks_exact_mut(line)) {
        acc.fill(1 << 23);
        for (k, &share) in shares.iter().enumerate() {
            let src = &rows[(first + k) * line..(first + k + 1) * line];
            for (a, &v) in acc.iter_mut().zip(src) {
                *a += u32::from(v) * share;
            }
        }
        for (d, &a) in dst.iter_mut().zip(&acc) {
            *d = (a >> 24) as u8;
        }
    }
    Pixels {
        width: w as u32,
        height: h as u32,
        channels: c,
        data,
    }
}

/// RGBA whose every pixel is opaque as RGB: a smaller, faster PNG.
fn drop_opaque_alpha(img: Pixels) -> Pixels {
    if img.channels != 4 || img.data.as_chunks::<4>().0.iter().any(|p| p[3] != 255) {
        return img;
    }
    let data = img
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    Pixels {
        channels: 3,
        data,
        ..img
    }
}

/// Produces a backend's captured pixels; run off the event loop.
pub type Convert = Box<dyn FnOnce() -> R<Pixels> + Send>;

/// The PNG sent back for a capture and its size: `pixels` (or, without
/// them, the PNG an external tool wrote to `path`) shrunk to fit in `max` x
/// `max` and encoded once. `path` ends up holding exactly that PNG, the
/// image the screenshot's coordinates refer to; it is removed on failure.
pub fn finish(path: &str, pixels: Option<Convert>, max: u32) -> R<((u32, u32), Vec<u8>)> {
    let done = (|| {
        let img = match pixels {
            Some(convert) => convert()?,
            None => {
                let img = read_png(path)?;
                if fit((img.width, img.height), max, max) == (img.width, img.height) {
                    let png = std::fs::read(path).map_err(|e| e.to_string())?;
                    return Ok(((img.width, img.height), png));
                }
                img
            }
        };
        if img.width == 0 || img.height == 0 {
            return Err("empty screenshot".into());
        }
        let (w, h) = fit((img.width, img.height), max, max);
        let img = if (w, h) == (img.width, img.height) {
            img
        } else {
            shrink(&img, w, h)
        };
        let png = write_png(&drop_opaque_alpha(img))?;
        std::fs::write(path, &png).map_err(|e| format!("{path}: {e}"))?;
        Ok(((w, h), png))
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(path);
    }
    done
}

pub fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc);
        }
    }

    #[test]
    fn fit_keeps_aspect_and_never_grows() {
        assert_eq!(fit((1920, 1080), 1280, 1280), (1280, 720));
        assert_eq!(fit((720, 1280), 1280, 1280), (720, 1280));
        assert_eq!(fit((800, 600), 1280, 1280), (800, 600));
        assert_eq!(fit((4000, 10), 100, 100), (100, 1));
    }

    #[test]
    fn shrink_averages_source_areas() {
        // 4x2 grey: left half 0, right half 200 -> 2x1 keeps the halves.
        let img = Pixels {
            width: 4,
            height: 2,
            channels: 1,
            data: vec![0, 0, 200, 200, 0, 0, 200, 200],
        };
        let s = shrink(&img, 2, 1);
        assert_eq!((s.width, s.height, s.data), (2, 1, vec![0, 200]));
        // A 3 -> 2 shrink splits the middle pixel between both outputs.
        let img = Pixels {
            width: 3,
            height: 1,
            channels: 1,
            data: vec![0, 90, 180],
        };
        assert_eq!(shrink(&img, 2, 1).data, vec![30, 150]);
    }

    /// The straightforward area average `shrink` must agree with.
    fn shrink_reference(img: &Pixels, w: usize, h: usize) -> Vec<u8> {
        let (sw, sh, c) = (img.width as usize, img.height as usize, img.channels);
        let cover = |src: usize, dst: usize, i: usize, s: usize| {
            let step = src as f64 / dst as f64;
            let (a, b) = (i as f64 * step, (i + 1) as f64 * step);
            (b.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0)
        };
        let mut out = Vec::with_capacity(w * h * c);
        for oy in 0..h {
            for ox in 0..w {
                for ch in 0..c {
                    let (mut sum, mut total) = (0.0, 0.0);
                    for sy in 0..sh {
                        for sx in 0..sw {
                            let wt = cover(sw, w, ox, sx) * cover(sh, h, oy, sy);
                            sum += f64::from(img.data[(sy * sw + sx) * c + ch]) * wt;
                            total += wt;
                        }
                    }
                    out.push((sum / total).round() as u8);
                }
            }
        }
        out
    }

    /// Deterministic noise with flat runs, like UI pixels.
    fn noise(width: u32, height: u32, channels: usize) -> Pixels {
        let mut seed = 0x2545_f491_u32;
        let data = (0..width as usize * height as usize * channels)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if i % 7 < 3 { 255 } else { (seed >> 24) as u8 }
            })
            .collect();
        Pixels {
            width,
            height,
            channels,
            data,
        }
    }

    #[test]
    fn shrink_matches_the_exact_area_average() {
        for channels in 1..=4 {
            let img = noise(37, 23, channels);
            for (w, h) in [(20, 12), (7, 5), (36, 22), (1, 1), (37, 9)] {
                let got = shrink(&img, w, h);
                let want = shrink_reference(&img, w as usize, h as usize);
                assert_eq!(got.data.len(), want.len());
                let off = got
                    .data
                    .iter()
                    .zip(&want)
                    .map(|(&g, &r)| g.abs_diff(r))
                    .max();
                assert!(off <= Some(1), "{channels} ch to {w}x{h}: off by {off:?}");
            }
        }
    }

    #[test]
    fn shrink_keeps_flat_areas_exact() {
        for v in [0, 1, 128, 254, 255] {
            let img = Pixels {
                width: 1920,
                height: 1080,
                channels: 4,
                data: vec![v; 1920 * 1080 * 4],
            };
            let s = shrink(&img, 1277, 719);
            assert!(s.data.iter().all(|&p| p == v), "value {v}");
        }
    }

    #[test]
    fn opaque_rgba_is_sent_as_rgb_and_translucent_kept() {
        let rgba = |a| Pixels {
            width: 1,
            height: 2,
            channels: 4,
            data: vec![1, 2, 3, 255, 4, 5, 6, a],
        };
        let rgb = drop_opaque_alpha(rgba(255));
        assert_eq!((rgb.channels, rgb.data), (3, vec![1, 2, 3, 4, 5, 6]));
        assert_eq!(drop_opaque_alpha(rgba(0)).channels, 4);
    }

    fn temp(name: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "desktop-mcp-shot-{name}-{}.png",
                std::process::id()
            ))
            .display()
            .to_string()
    }

    #[test]
    fn finish_writes_the_shrunk_image_it_returns() {
        // Captured pixels: the (empty) file gets the one encoded PNG.
        let path = temp("pixels");
        std::fs::write(&path, b"").unwrap();
        let convert: Convert = Box::new(|| Ok(noise(300, 100, 4)));
        let ((w, h), png) = finish(&path, Some(convert), 150).unwrap();
        assert_eq!((w, h), (150, 50));
        assert_eq!(std::fs::read(&path).unwrap(), png);
        // Another tool's PNG (the spectacle/grim fallback) is decoded.
        let small = finish(&path, None, 60).unwrap();
        assert_eq!(small.0, (60, 20));
        let back = read_png(&path).unwrap();
        assert_eq!((back.width, back.height, back.channels), (60, 20, 4));
        // Small enough already: sent as the tool wrote it.
        let same = finish(&path, None, 100).unwrap();
        assert_eq!((same.0, same.1), ((60, 20), small.1));
        std::fs::remove_file(&path).unwrap();
        // A failed capture leaves no file behind.
        let convert: Convert = Box::new(|| Err("gone".into()));
        assert!(finish(&path, Some(convert), 100).is_err());
        assert!(std::fs::metadata(&path).is_err());
    }

    /// `cargo test --release -- --ignored --nocapture shrink_timing`
    #[test]
    #[ignore]
    fn shrink_timing() {
        let img = noise(1920, 1080, 4);
        for (w, h) in [(1280, 720), (640, 360)] {
            let start = std::time::Instant::now();
            let runs = 20;
            for _ in 0..runs {
                std::hint::black_box(shrink(&img, w, h));
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            println!("shrink 1920x1080 RGBA to {w}x{h}: {ms:.2} ms");
        }
    }

    #[test]
    fn png_round_trip_keeps_pixels() {
        let img = Pixels {
            width: 2,
            height: 1,
            channels: 3,
            data: vec![1, 2, 3, 250, 251, 252],
        };
        let bytes = write_png(&img).unwrap();
        let path =
            std::env::temp_dir().join(format!("desktop-mcp-shot-{}.png", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let back = read_png(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((back.width, back.height, back.channels), (2, 1, 3));
        assert_eq!(back.data, img.data);
    }
}
