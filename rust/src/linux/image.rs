//! Raw QImage pixels from KWin's ScreenShot2 to straight-alpha RGBA.

use super::kwin::RawImage;
use crate::backend::R;
use crate::shot::Pixels;

// QImage::Format values KWin hands out for window captures.
const RGB32: u32 = 4;
const ARGB32: u32 = 5;
const ARGB32_PREMULTIPLIED: u32 = 6;
const RGBX8888: u32 = 16;
const RGBA8888: u32 = 17;
const RGBA8888_PREMULTIPLIED: u32 = 18;

fn unpremultiply(c: u8, a: u8) -> u8 {
    match a {
        0 => 0,
        255 => c,
        _ => ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8,
    }
}

/// Tightly packed, straight-alpha RGBA8 rows.
pub fn to_rgba(img: &RawImage) -> R<Vec<u8>> {
    // (byte offsets of R, G, B, A within a pixel; alpha present; premultiplied)
    let (layout, alpha, premul) = match img.format {
        // 32-bit ARGB words in native (little-endian) order: B, G, R, A.
        RGB32 => ([2, 1, 0, 3], false, false),
        ARGB32 => ([2, 1, 0, 3], true, false),
        ARGB32_PREMULTIPLIED => ([2, 1, 0, 3], true, true),
        RGBX8888 => ([0, 1, 2, 3], false, false),
        RGBA8888 => ([0, 1, 2, 3], true, false),
        RGBA8888_PREMULTIPLIED => ([0, 1, 2, 3], true, true),
        other => return Err(format!("unsupported QImage format {other}")),
    };
    let (w, h, stride) = (img.width as usize, img.height as usize, img.stride as usize);
    if stride < w * 4 || img.data.len() < stride * h {
        return Err("image buffer smaller than its size".into());
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for row in img.data.chunks(stride).take(h) {
        for px in row[..w * 4].as_chunks::<4>().0 {
            let a = if alpha { px[layout[3]] } else { 255 };
            let (r, g, b) = (px[layout[0]], px[layout[1]], px[layout[2]]);
            if premul {
                out.extend_from_slice(&[
                    unpremultiply(r, a),
                    unpremultiply(g, a),
                    unpremultiply(b, a),
                    a,
                ]);
            } else {
                out.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    Ok(out)
}

/// The capture as RGBA pixels, sized as KWin sent it.
pub fn to_pixels(img: &RawImage) -> R<Pixels> {
    Ok(Pixels {
        width: img.width,
        height: img.height,
        channels: 4,
        data: to_rgba(img)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(format: u32, stride: u32, data: Vec<u8>) -> RawImage {
        RawImage {
            width: 2,
            height: 1,
            stride,
            format,
            data,
        }
    }

    #[test]
    fn converts_native_argb_and_rgba_layouts() {
        // Pixels: opaque red, then half-transparent premultiplied green.
        let argb = img(
            ARGB32_PREMULTIPLIED,
            8,
            vec![0, 0, 255, 255, 0, 128, 0, 128],
        );
        assert_eq!(
            to_rgba(&argb).unwrap(),
            vec![255, 0, 0, 255, 0, 255, 0, 128]
        );
        let rgba = img(
            RGBA8888_PREMULTIPLIED,
            8,
            vec![255, 0, 0, 255, 0, 128, 0, 128],
        );
        assert_eq!(
            to_rgba(&rgba).unwrap(),
            vec![255, 0, 0, 255, 0, 255, 0, 128]
        );
        let straight = img(RGBA8888, 8, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(to_rgba(&straight).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn opaque_formats_ignore_padding_byte_and_row_padding() {
        // Stride 12: four bytes of row padding after two pixels.
        let rgb32 = img(RGB32, 12, vec![3, 2, 1, 0, 6, 5, 4, 0, 9, 9, 9, 9]);
        assert_eq!(to_rgba(&rgb32).unwrap(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
        let rgbx = img(RGBX8888, 8, vec![1, 2, 3, 0, 4, 5, 6, 7]);
        assert_eq!(to_rgba(&rgbx).unwrap(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rejects_unknown_format_and_short_buffer() {
        assert!(to_rgba(&img(13, 8, vec![0; 8])).is_err());
        assert!(to_rgba(&img(RGBA8888, 8, vec![0; 7])).is_err());
        assert!(to_rgba(&img(RGBA8888, 4, vec![0; 8])).is_err());
    }

    #[test]
    fn pixels_keep_the_capture_size() {
        let px = to_pixels(&img(RGBA8888, 8, vec![255; 8])).unwrap();
        assert_eq!(
            (px.width, px.height, px.channels, px.data.len()),
            (2, 1, 4, 8)
        );
    }
}
