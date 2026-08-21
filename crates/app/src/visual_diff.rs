//! Pixel comparison for visual regression tests.
//!
//! Platform-independent (plain image math) so it is unit-tested on every
//! platform, even though screenshots can only be captured on macOS.

use image::{Rgba, RgbaImage};

/// Result of comparing a captured screenshot against a baseline.
pub struct Comparison {
    /// Fraction of pixels that match, 0.0..=1.0.
    pub match_fraction: f64,
    /// Number of differing pixels (size mismatches count as differing).
    pub diff_pixel_count: u64,
    /// Total pixels compared (union of both image sizes).
    pub total_pixels: u64,
    /// Visualization: differing pixels bright red, matching pixels dimmed.
    pub diff_image: RgbaImage,
}

/// Compare two images per-pixel with a per-channel tolerance.
///
/// The comparison canvas is the union of both sizes; any pixel present in one
/// image but not the other counts as a difference, so a size change always
/// fails loudly instead of being cropped away.
pub fn compare(actual: &RgbaImage, expected: &RgbaImage, tolerance: u8) -> Comparison {
    let width = actual.width().max(expected.width());
    let height = actual.height().max(expected.height());
    let total_pixels = u64::from(width) * u64::from(height);

    let mut diff_image = RgbaImage::new(width, height);
    let mut matching: u64 = 0;

    for y in 0..height {
        for x in 0..width {
            let a = pixel_or_transparent(actual, x, y);
            let e = pixel_or_transparent(expected, x, y);
            if channels_match(a, e, tolerance) {
                matching += 1;
                // Dimmed copy of the actual pixel, so the diff image stays readable.
                diff_image.put_pixel(x, y, Rgba([a.0[0] / 4, a.0[1] / 4, a.0[2] / 4, 255]));
            } else {
                diff_image.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
    }

    Comparison {
        match_fraction: if total_pixels == 0 {
            1.0
        } else {
            matching as f64 / total_pixels as f64
        },
        diff_pixel_count: total_pixels - matching,
        total_pixels,
        diff_image,
    }
}

fn pixel_or_transparent(img: &RgbaImage, x: u32, y: u32) -> Rgba<u8> {
    if x < img.width() && y < img.height() {
        *img.get_pixel(x, y)
    } else {
        Rgba([0, 0, 0, 0])
    }
}

fn channels_match(a: Rgba<u8>, b: Rgba<u8>, tolerance: u8) -> bool {
    a.0.iter()
        .zip(b.0.iter())
        .all(|(&ca, &cb)| ca.abs_diff(cb) <= tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba(color))
    }

    #[test]
    fn identical_images_fully_match() {
        let img = solid(8, 8, [10, 20, 30, 255]);
        let cmp = compare(&img, &img, 0);
        assert_eq!(cmp.match_fraction, 1.0);
        assert_eq!(cmp.diff_pixel_count, 0);
        assert_eq!(cmp.total_pixels, 64);
    }

    #[test]
    fn single_changed_pixel_is_counted() {
        let expected = solid(8, 8, [10, 20, 30, 255]);
        let mut actual = expected.clone();
        actual.put_pixel(3, 3, Rgba([200, 20, 30, 255]));
        let cmp = compare(&actual, &expected, 0);
        assert_eq!(cmp.diff_pixel_count, 1);
        assert_eq!(cmp.diff_image.get_pixel(3, 3), &Rgba([255, 0, 0, 255]));
        assert!(cmp.match_fraction < 1.0);
    }

    #[test]
    fn tolerance_absorbs_small_channel_noise() {
        let expected = solid(4, 4, [100, 100, 100, 255]);
        let actual = solid(4, 4, [102, 98, 101, 255]);
        assert_eq!(compare(&actual, &expected, 2).diff_pixel_count, 0);
        assert_eq!(compare(&actual, &expected, 1).diff_pixel_count, 16);
    }

    #[test]
    fn size_mismatch_counts_missing_region_as_diff() {
        let expected = solid(10, 10, [10, 20, 30, 255]);
        let actual = solid(10, 8, [10, 20, 30, 255]);
        let cmp = compare(&actual, &expected, 0);
        assert_eq!(cmp.total_pixels, 100);
        assert_eq!(cmp.diff_pixel_count, 20);
    }
}
