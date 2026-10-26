//! Screenshot post-processing shared by every backend: shrink a PNG to a
//! size limit and encode it for an MCP image block.

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

/// Shrink by averaging each output pixel's source area (area-weighted, so
/// thin lines and text stay readable instead of aliasing away).
pub fn shrink(img: &Pixels, w: u32, h: u32) -> Pixels {
    let (sw, sh, c) = (img.width as usize, img.height as usize, img.channels);
    let (w, h) = (w as usize, h as usize);
    // Per output column: the source columns it covers and their weights.
    let spans = |src: usize, dst: usize| -> Vec<Vec<(usize, f64)>> {
        let step = src as f64 / dst as f64;
        (0..dst)
            .map(|i| {
                let (a, b) = (i as f64 * step, (i + 1) as f64 * step);
                (a.floor() as usize..(b.ceil() as usize).min(src))
                    .map(|s| (s, (b.min(s as f64 + 1.0) - a.max(s as f64)).max(0.0)))
                    .filter(|&(_, wt)| wt > 0.0)
                    .collect()
            })
            .collect()
    };
    let (xs, ys) = (spans(sw, w), spans(sh, h));
    let mut data = vec![0u8; w * h * c];
    let mut acc = vec![0f64; c];
    for (oy, yspan) in ys.iter().enumerate() {
        for (ox, xspan) in xs.iter().enumerate() {
            acc.iter_mut().for_each(|v| *v = 0.0);
            let mut total = 0.0;
            for &(sy, wy) in yspan {
                let row = sy * sw * c;
                for &(sx, wx) in xspan {
                    let wt = wx * wy;
                    let p = row + sx * c;
                    for (v, &px) in acc.iter_mut().zip(&img.data[p..p + c]) {
                        *v += px as f64 * wt;
                    }
                    total += wt;
                }
            }
            let o = (oy * w + ox) * c;
            for (d, v) in data[o..o + c].iter_mut().zip(&acc) {
                *d = (v / total).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Pixels {
        width: w as u32,
        height: h as u32,
        channels: c,
        data,
    }
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
