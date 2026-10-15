//! Scan damage features from the page image, layout features from its
//! ALTO. No model: each is a fixed computation on a downscaled grey copy
//! of the page (integer box averaging, so the copy is a function of the
//! pixels alone) or on the ALTO's geometry.
//!
//! - `skew_deg`: the angle in -3..3 degrees, in 0.1 degree steps, at which
//!   the horizontal projection profile of the ink has the highest
//!   variance. Positive when text lines descend to the right in image
//!   coordinates.
//! - `blur`: variance of the 3x3 Laplacian; lower is blurrier.
//! - `contrast`: the 95th less the 5th percentile of grey.
//! - `noise`: mean absolute residual of a 3x3 median filter.
//! - `bleed_through`: the share of pixels in the middle third of grey
//!   between the ink mode and the background mode.
//! - `ink_density`: the share of pixels below the Otsu threshold.
use image::GrayImage;
use usnm_alto::Page;
pub use usnm_ocr_contract::quality::{ImageFeatures, LayoutFeatures};

/// Long side of the grey copy the features are computed on.
pub const MAX_SIDE: u32 = 1024;
/// Skew search: `-SKEW_STEPS..=SKEW_STEPS` tenths of a degree.
pub const SKEW_STEPS: i32 = 30;

/// A grey copy with its long side at most [`MAX_SIDE`], each pixel the
/// mean of a `scale` by `scale` block (truncated to the pixels inside the
/// image). Returns the copy and the factor.
pub fn downscale(img: &GrayImage) -> (GrayImage, u32) {
    let (w, h) = img.dimensions();
    let scale = w.max(h).div_ceil(MAX_SIDE).max(1);
    if scale == 1 {
        return (img.clone(), 1);
    }
    let (ow, oh) = (w.div_ceil(scale), h.div_ceil(scale));
    let mut out = GrayImage::new(ow, oh);
    for oy in 0..oh {
        for ox in 0..ow {
            let (mut sum, mut n) = (0u64, 0u64);
            for y in oy * scale..((oy + 1) * scale).min(h) {
                for x in ox * scale..((ox + 1) * scale).min(w) {
                    sum += u64::from(img.get_pixel(x, y).0[0]);
                    n += 1;
                }
            }
            out.put_pixel(ox, oy, image::Luma([(sum / n.max(1)) as u8]));
        }
    }
    (out, scale)
}

pub fn histogram(img: &GrayImage) -> [u64; 256] {
    let mut h = [0u64; 256];
    for p in img.pixels() {
        h[usize::from(p.0[0])] += 1;
    }
    h
}

/// Otsu's threshold: the grey level that maximises between-class
/// variance; pixels below it are ink.
pub fn otsu(hist: &[u64; 256]) -> u8 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 128;
    }
    let sum_all: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, n)| i as f64 * *n as f64)
        .sum();
    let (mut w0, mut sum0) = (0f64, 0f64);
    let (mut best, mut at) = (f64::NEG_INFINITY, 128usize);
    for (t, n) in hist.iter().enumerate() {
        w0 += *n as f64;
        if w0 == 0.0 {
            continue;
        }
        let w1 = total as f64 - w0;
        if w1 == 0.0 {
            break;
        }
        sum0 += t as f64 * *n as f64;
        let m0 = sum0 / w0;
        let m1 = (sum_all - sum0) / w1;
        let between = w0 * w1 * (m0 - m1) * (m0 - m1);
        if between > best {
            best = between;
            at = t;
        }
    }
    at.min(254) as u8 + 1
}

fn percentile(hist: &[u64; 256], p: f64) -> u8 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0;
    }
    let want = (p * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in hist.iter().enumerate() {
        seen += n;
        if seen >= want {
            return i as u8;
        }
    }
    255
}

fn skew(img: &GrayImage, threshold: u8) -> f64 {
    let (w, h) = img.dimensions();
    let dark: Vec<(f64, f64)> = img
        .enumerate_pixels()
        .filter(|(_, _, p)| p.0[0] < threshold)
        .map(|(x, y, _)| (f64::from(x), f64::from(y)))
        .collect();
    if dark.is_empty() || h == 0 {
        return 0.0;
    }
    let cx = f64::from(w) / 2.0;
    let margin =
        (f64::from(w) * (f64::from(SKEW_STEPS) / 10.0).to_radians().tan()).ceil() as usize + 2;
    let rows = h as usize + 2 * margin;
    let mut best = (f64::NEG_INFINITY, 0i32);
    for step in -SKEW_STEPS..=SKEW_STEPS {
        let t = (f64::from(step) / 10.0).to_radians().tan();
        let mut profile = vec![0u32; rows];
        for &(x, y) in &dark {
            let r = (y - (x - cx) * t).round() as i64 + margin as i64;
            if let Some(cell) = usize::try_from(r).ok().and_then(|r| profile.get_mut(r)) {
                *cell += 1;
            }
        }
        let n = profile.len() as f64;
        let mean = profile.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let var = profile
            .iter()
            .map(|v| {
                let d = f64::from(*v) - mean;
                d * d
            })
            .sum::<f64>()
            / n;
        // Ties go to the smaller angle, then the earlier one.
        if var > best.0 || (var == best.0 && step.abs() < best.1.abs()) {
            best = (var, step);
        }
    }
    f64::from(best.1) / 10.0
}

fn laplacian_variance(img: &GrayImage) -> f64 {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let at = |x: u32, y: u32| f64::from(img.get_pixel(x, y).0[0]);
    let mut vals = Vec::with_capacity(((w - 2) * (h - 2)) as usize);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            vals.push(4.0 * at(x, y) - at(x - 1, y) - at(x + 1, y) - at(x, y - 1) - at(x, y + 1));
        }
    }
    let n = vals.len() as f64;
    let mean = vals.iter().sum::<f64>() / n;
    vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n
}

fn median_residual(img: &GrayImage) -> f64 {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut sum = 0f64;
    let mut n = 0u64;
    let mut win = [0u8; 9];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let mut k = 0;
            for dy in 0..3 {
                for dx in 0..3 {
                    win[k] = img.get_pixel(x + dx - 1, y + dy - 1).0[0];
                    k += 1;
                }
            }
            win.sort_unstable();
            let c = i32::from(img.get_pixel(x, y).0[0]);
            sum += f64::from((c - i32::from(win[4])).abs());
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

fn bleed_through(hist: &[u64; 256], threshold: u8) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let t = usize::from(threshold);
    let mode = |range: std::ops::Range<usize>| -> usize {
        range
            .clone()
            .fold(range.start, |b, i| if hist[i] > hist[b] { i } else { b })
    };
    let ink = mode(0..t.max(1));
    let bg = mode(t.min(255)..256);
    if bg <= ink {
        return 0.0;
    }
    let span = bg - ink;
    let lo = ink + span / 3;
    let hi = ink + 2 * span / 3;
    let band: u64 = (lo..=hi).map(|i| hist[i]).sum();
    band as f64 / total as f64
}

/// The damage features of a page image.
pub fn image_features(img: &GrayImage) -> ImageFeatures {
    let (width, height) = img.dimensions();
    let (small, scale) = downscale(img);
    let hist = histogram(&small);
    let threshold = otsu(&hist);
    let total: u64 = hist.iter().sum();
    let ink: u64 = hist[..usize::from(threshold)].iter().sum();
    ImageFeatures {
        bleed_through: bleed_through(&hist, threshold),
        blur: laplacian_variance(&small),
        contrast: f64::from(percentile(&hist, 0.95)) - f64::from(percentile(&hist, 0.05)),
        height,
        ink_density: if total == 0 {
            0.0
        } else {
            ink as f64 / total as f64
        },
        noise: median_residual(&small),
        scale,
        skew_deg: skew(&small, threshold),
        threshold,
        width,
    }
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Text blocks clustered by x-centre: a new column starts where the gap
/// to the previous centre is more than half the median block width.
pub fn columns(page: &Page) -> usize {
    let blocks: Vec<&usnm_alto::Block> = page.blocks.iter().filter(|b| b.kind.is_text()).collect();
    if blocks.is_empty() {
        return 0;
    }
    let mut widths: Vec<f64> = blocks.iter().map(|b| b.rect.width()).collect();
    let gap = (median(&mut widths).unwrap_or(0.0) / 2.0).max(1.0);
    let mut centres: Vec<f64> = blocks.iter().map(|b| b.rect.centre().0).collect();
    centres.sort_by(f64::total_cmp);
    let mut n = 1;
    for w in centres.windows(2) {
        if w[1] - w[0] > gap {
            n += 1;
        }
    }
    n
}

/// The layout features of a page's ALTO (in image pixels).
pub fn layout_features(page: &Page) -> LayoutFeatures {
    let (blocks, lines, words) = page.counts();
    let mut heights: Vec<f64> = page
        .blocks
        .iter()
        .filter(|b| b.kind.is_text())
        .flat_map(|b| &b.lines)
        .map(|l| l.rect.height())
        .collect();
    LayoutFeatures {
        blocks,
        columns: columns(page),
        line_height_px: median(&mut heights),
        lines,
        mean_word_conf: page.mean_conf(),
        words,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use usnm_alto::{Block, BlockKind, Granularity, Line, Rect, Word};

    /// A light page with dark text-like rows (short dashes of ink along
    /// each row) whose baselines follow `slope` (dy per dx).
    pub(crate) fn synthetic_page(w: u32, h: u32, slope: f64, bg: u8, ink: u8) -> GrayImage {
        let mut img = GrayImage::from_pixel(w, h, image::Luma([bg]));
        let cx = f64::from(w) / 2.0;
        let mut row = 60.0;
        while row < f64::from(h) - 60.0 {
            for x in 40..w - 40 {
                // Words: 7 px of ink, 3 px gap, as a crude line of type.
                if (x / 10) % 10 == 9 {
                    continue;
                }
                let y0 = row + (f64::from(x) - cx) * slope;
                for dy in 0..6 {
                    let y = y0.round() as i64 + dy;
                    if y >= 0 && y < i64::from(h) {
                        img.put_pixel(x, y as u32, image::Luma([ink]));
                    }
                }
            }
            row += 24.0;
        }
        img
    }

    fn box_blur(img: &GrayImage) -> GrayImage {
        let (w, h) = img.dimensions();
        let mut out = img.clone();
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let mut s = 0u32;
                for dy in 0..3 {
                    for dx in 0..3 {
                        s += u32::from(img.get_pixel(x + dx - 1, y + dy - 1).0[0]);
                    }
                }
                out.put_pixel(x, y, image::Luma([(s / 9) as u8]));
            }
        }
        out
    }

    #[test]
    fn skew_blur_contrast_on_a_rendered_page() {
        let slope = 1.5f64.to_radians().tan();
        let img = synthetic_page(800, 1000, slope, 235, 30);
        let f = image_features(&img);
        assert!((f.skew_deg - 1.5).abs() <= 0.3, "skew {}", f.skew_deg);
        assert!(f.contrast >= 200.0, "contrast {}", f.contrast);
        assert!(
            f.ink_density > 0.05 && f.ink_density < 0.5,
            "ink {}",
            f.ink_density
        );
        assert_eq!(f.scale, 1);
        let straight = image_features(&synthetic_page(800, 1000, 0.0, 235, 30));
        assert!(straight.skew_deg.abs() <= 0.1, "skew {}", straight.skew_deg);
        let blurred = image_features(&box_blur(&img));
        assert!(blurred.blur < f.blur, "{} !< {}", blurred.blur, f.blur);
        let other = synthetic_page(800, 1000, -slope, 235, 30);
        assert!((image_features(&other).skew_deg + 1.5).abs() <= 0.3);
        // Low contrast when ink and background are close.
        let faint = image_features(&synthetic_page(800, 1000, 0.0, 200, 150));
        assert!(faint.contrast < f.contrast);
        // A large page is downscaled by an integer factor and still reads.
        let big = synthetic_page(2600, 3000, slope, 235, 30);
        let fb = image_features(&big);
        assert_eq!(fb.scale, 3);
        assert_eq!((fb.width, fb.height), (2600, 3000));
        assert!((fb.skew_deg - 1.5).abs() <= 0.3, "skew {}", fb.skew_deg);
    }

    #[test]
    fn noise_and_bleed_move_the_right_way() {
        let clean = synthetic_page(400, 400, 0.0, 240, 20);
        let mut noisy = clean.clone();
        let mut s = 11u32;
        for p in noisy.pixels_mut() {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            if s.is_multiple_of(7) {
                p.0[0] = p.0[0].saturating_sub(40);
            }
        }
        let (fc, fn_) = (image_features(&clean), image_features(&noisy));
        assert!(fn_.noise > fc.noise);
        let mut bled = clean.clone();
        for (x, y, p) in bled.enumerate_pixels_mut() {
            if p.0[0] == 240 && (x / 5 + y / 5).is_multiple_of(8) {
                p.0[0] = 130;
            }
        }
        assert!(image_features(&bled).bleed_through > fc.bleed_through);
        let empty = image_features(&GrayImage::new(0, 0));
        assert_eq!(empty.skew_deg, 0.0);
    }

    pub(crate) fn page_with_columns(cols: usize, lines_per_block: usize) -> Page {
        let (w, h) = (4000.0, 6000.0);
        let col_w = w / cols as f64;
        let mut blocks = Vec::new();
        for c in 0..cols {
            for b in 0..2 {
                let x0 = c as f64 * col_w + 40.0;
                let y0 = 200.0 + b as f64 * 2800.0;
                let mut lines = Vec::new();
                for l in 0..lines_per_block {
                    let ly = y0 + l as f64 * 40.0;
                    let words = (0..5)
                        .map(|k| Word {
                            rect: Rect::new(
                                x0 + k as f64 * 130.0,
                                ly,
                                x0 + k as f64 * 130.0 + 110.0,
                                ly + 30.0,
                            ),
                            text: format!("w{c}{b}{l}{k}"),
                            conf: Some(0.5 + 0.1 * (c % 3) as f64),
                        })
                        .collect();
                    lines.push(Line {
                        rect: Rect::new(x0, ly, x0 + col_w - 80.0, ly + 30.0),
                        words,
                        hyphen: None,
                    });
                }
                blocks.push(Block {
                    kind: BlockKind::Text,
                    rect: Rect::new(
                        x0,
                        y0,
                        x0 + col_w - 80.0,
                        y0 + lines_per_block as f64 * 40.0,
                    ),
                    lang: None,
                    lines,
                });
            }
        }
        Page {
            width: w,
            height: h,
            granularity: Granularity::Word,
            blocks,
        }
    }

    #[test]
    fn layout_counts_columns_and_lines() {
        let f = layout_features(&page_with_columns(4, 10));
        assert_eq!(f.columns, 4);
        assert_eq!(f.blocks, 8);
        assert_eq!(f.lines, 80);
        assert_eq!(f.words, 400);
        assert_eq!(f.line_height_px, Some(30.0));
        assert!(f.mean_word_conf.is_some());
        let empty = layout_features(&Page {
            width: 1.0,
            height: 1.0,
            granularity: Granularity::Page,
            blocks: vec![],
        });
        assert_eq!(empty.columns, 0);
        assert_eq!(empty.line_height_px, None);
    }
}
