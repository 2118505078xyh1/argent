//! The argent mark, drawn in code.
//!
//! One source of truth for the shipped assets: the RGBA window icon and the
//! `.ico` a desktop shortcut points at both come from this geometry. Keeping
//! the mark here rather than in a binary asset stops the two from drifting.

use crate::palette::Palette;

/// Render the mark at `size` pixels square, as RGBA.
pub fn rgba(size: usize) -> Vec<u8> {
    let mut pixels = vec![0u8; size * size * 4];
    let accent = Palette::dark().accent;
    let ink = Palette::dark().background;
    let radius = size as f32 * 0.22;

    // A chevron, drawn as two thick segments meeting at the right.
    let scale = size as f32 / 64.0;
    let segments = [
        (20.0 * scale, 22.0 * scale, 32.0 * scale, 32.0 * scale),
        (32.0 * scale, 32.0 * scale, 20.0 * scale, 42.0 * scale),
    ];
    let half_stroke = 3.0 * scale;

    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if !inside_rounded_square(px, py, size as f32, radius) {
                continue;
            }
            let mut rgb = accent;
            for (x0, y0, x1, y1) in segments {
                if distance_to_segment(px, py, x0, y0, x1, y1) <= half_stroke {
                    rgb = ink;
                }
            }
            let index = (y * size + x) * 4;
            pixels[index] = rgb.r;
            pixels[index + 1] = rgb.g;
            pixels[index + 2] = rgb.b;
            pixels[index + 3] = 255;
        }
    }
    pixels
}

/// Write a single-image `.ico` file.
///
/// The format is a directory header, one entry, then a bottom-up 32-bit BMP
/// with an all-opaque mask. No compression is involved, which is why this needs
/// no encoder and no dependency.
pub fn write_ico(path: &std::path::Path, size: usize) -> std::io::Result<()> {
    let pixels = rgba(size);
    let width = size as u32;
    let height = size as u32;
    // BMP rows are bottom-up and padded to four bytes; 32-bit pixels are always
    // aligned, so there is no padding to add.
    let mask_bytes = ((width as usize + 31) / 32) * 4 * height as usize;
    let image_bytes = (width * height * 4) as usize + mask_bytes;

    let mut out: Vec<u8> = Vec::with_capacity(22 + image_bytes);

    // ICONDIR
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&1u16.to_le_bytes()); // image count

    // ICONDIRENTRY
    out.push(if size >= 256 { 0 } else { size as u8 }); // width, 0 means 256
    out.push(if size >= 256 { 0 } else { size as u8 }); // height
    out.push(0); // palette size
    out.push(0); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&(image_bytes as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes()); // offset of the image

    // BITMAPINFOHEADER: height counts the colour image plus the mask.
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&((height * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bit count
    out.extend_from_slice(&0u32.to_le_bytes()); // compression
    out.extend_from_slice(&(image_bytes as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution and palette counts

    // Pixels, BGRA, bottom-up.
    for y in (0..size).rev() {
        for x in 0..size {
            let index = (y * size + x) * 4;
            out.push(pixels[index + 2]); // blue
            out.push(pixels[index + 1]); // green
            out.push(pixels[index]); // red
            out.push(pixels[index + 3]); // alpha
        }
    }
    // AND mask: fully opaque, so all zero.
    out.extend(std::iter::repeat(0u8).take(mask_bytes));

    std::fs::write(path, out)
}

fn inside_rounded_square(x: f32, y: f32, size: f32, radius: f32) -> bool {
    let inner_min = radius;
    let inner_max = size - radius;
    if x >= inner_min && x <= inner_max && y >= 0.0 && y <= size {
        return true;
    }
    if y >= inner_min && y <= inner_max && x >= 0.0 && x <= size {
        return true;
    }
    let cx = x.clamp(inner_min, inner_max);
    let cy = y.clamp(inner_min, inner_max);
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

fn distance_to_segment(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let length_sq = dx * dx + dy * dy;
    if length_sq == 0.0 {
        return ((px - x0).powi(2) + (py - y0).powi(2)).sqrt();
    }
    let t = (((px - x0) * dx + (py - y0) * dy) / length_sq).clamp(0.0, 1.0);
    let cx = x0 + t * dx;
    let cy = y0 + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_is_square_and_opaque_in_the_centre() {
        let size = 32;
        let pixels = rgba(size);
        assert_eq!(pixels.len(), size * size * 4);
        let centre = (size / 2) * 4 + (size / 2) * size * 4;
        assert_eq!(pixels[centre + 3], 255);
    }

    #[test]
    fn the_corners_are_transparent() {
        let pixels = rgba(32);
        // Top-left corner falls outside the rounded square.
        assert_eq!(pixels[3], 0);
    }

    #[test]
    fn the_chevron_differs_from_the_background() {
        let size = 64;
        let pixels = rgba(size);
        let mut colours: Vec<(u8, u8, u8)> = Vec::new();
        for pixel in pixels.chunks(4) {
            if pixel[3] == 255 {
                colours.push((pixel[0], pixel[1], pixel[2]));
            }
        }
        colours.sort_unstable();
        colours.dedup();
        assert_eq!(colours.len(), 2, "expected a background and a glyph colour");
    }

    #[test]
    fn writing_an_ico_produces_a_valid_directory() {
        let path = std::env::temp_dir().join("argent-icon-test.ico");
        write_ico(&path, 32).expect("write");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(&bytes[0..2], &[0, 0], "reserved field");
        assert_eq!(&bytes[2..4], &[1, 0], "type is icon");
        assert_eq!(&bytes[4..6], &[1, 0], "one image");
        assert_eq!(bytes[6], 32, "width in the entry");
        assert_eq!(bytes[7], 32, "height in the entry");
        assert_eq!(&bytes[18..22], &22u32.to_le_bytes(), "image offset");
        let _ = std::fs::remove_file(&path);
    }
}
