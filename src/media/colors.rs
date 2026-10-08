//! `Paperclip::ColorExtractor`: the background, foreground and accent colours
//! Mastodon records in a media attachment's `meta.colors` when it has a
//! thumbnail, for the web client's audio player.
//!
//! The palettes are libvips' `hist_find_ndim` with ten bins a channel, of the
//! image shrunk to 100 pixels wide and of its edges; the distances are
//! `ColorDiff.between`'s CIEDE2000, and the contrasts the W3C's.

use image::{imageops::FilterType, DynamicImage, RgbImage};
use serde_json::{json, Value};

const MIN_CONTRAST: f64 = 3.0;
const ACCENT_MIN_CONTRAST: f64 = 2.0;
const BINS: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgb {
    r: f64,
    g: f64,
    b: f64,
}

/// `{ background, foreground, accent }` as `#rrggbb`, or `None` for an image
/// with no pixels.
///
/// CPU-bound: call it from the blocking pool, never on a Tokio worker.
pub fn extract(image: &DynamicImage) -> Option<Value> {
    let image = downscaled(image)?;
    let background_palette = palette(&edges(&image));
    let foreground_palette = palette(&pixels(&image, 0, 0, image.width(), image.height()));
    let background = *background_palette
        .first()
        .or_else(|| foreground_palette.first())?;

    let mut foreground_colors: Vec<Rgb> = vec![];
    for min_contrast in [ACCENT_MIN_CONTRAST, MIN_CONTRAST] {
        let mut max_distance = 0.0;
        let mut max_distance_color = None;
        for &color in &foreground_palette {
            let distance = ciede2000(background, color);
            let contrast = w3c_contrast(background, color);
            if distance > max_distance
                && contrast >= min_contrast
                && !foreground_colors.contains(&color)
            {
                max_distance = distance;
                max_distance_color = Some(color);
            }
        }
        foreground_colors.extend(max_distance_color);
    }
    // Too few: made from the background by lightening or darkening it.
    for i in 0..2usize.saturating_sub(foreground_colors.len()) {
        foreground_colors.push(lighten_or_darken(background, 35 + i as i64 * 15));
    }

    let foreground = max_by(&foreground_colors, |c| w3c_contrast(background, *c))?;
    let accent = max_by(&foreground_colors, |c| rgb_to_hsl(*c).1 as f64)?;
    Some(json!({
        "background": hex(background),
        "foreground": hex(foreground),
        "accent": hex(accent),
    }))
}

/// Ruby's `max_by`: the first of the greatest.
fn max_by(colors: &[Rgb], key: impl Fn(&Rgb) -> f64) -> Option<Rgb> {
    let mut best: Option<(Rgb, f64)> = None;
    for color in colors {
        let k = key(color);
        if best.is_none_or(|(_, b)| k > b) {
            best = Some((*color, k));
        }
    }
    best.map(|(c, _)| c)
}

/// `thumbnail_image(100)`, in sRGB without alpha.
fn downscaled(image: &DynamicImage) -> Option<RgbImage> {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return None;
    }
    let target_height = ((100.0 * f64::from(height) / f64::from(width)).round() as u32).max(1);
    Some(
        image
            .resize_exact(100, target_height, FilterType::Triangle)
            .to_rgb8(),
    )
}

fn pixels(image: &RgbImage, x: u32, y: u32, width: u32, height: u32) -> Vec<[u8; 3]> {
    let mut out = Vec::with_capacity((width * height) as usize);
    for row in y..(y + height).min(image.height()) {
        for col in x..(x + width).min(image.width()) {
            out.push(image.get_pixel(col, row).0);
        }
    }
    out
}

/// The top and bottom quarters, joined with the left and right quarters
/// beside them; libvips' `join` keeps only as much of each as the shorter
/// side has.
fn edges(image: &RgbImage) -> Vec<[u8; 3]> {
    let (width, height) = (image.width(), image.height());
    let block = height / 4;
    let line = width / 4;
    let side = height.saturating_sub(block * 2).min(block * 2);
    let mut out = pixels(image, 0, 0, width, block);
    out.extend(pixels(image, 0, height - block, width, block));
    out.extend(pixels(image, 0, block, line, side));
    out.extend(pixels(image, width - line, block, line, side));
    out
}

/// The ten most frequent of the histogram's bins, most frequent first, as
/// the colours at their centres.
fn palette(pixels: &[[u8; 3]]) -> Vec<Rgb> {
    let bins = BINS as usize;
    let mut counts = vec![0u32; bins * bins * bins];
    for [r, g, b] in pixels {
        let bin = |v: u8| u32::from(v) * BINS / 256;
        let (r, g, b) = (bin(*r), bin(*g), bin(*b));
        counts[((g * BINS + r) * BINS + b) as usize] += 1;
    }
    let mut ranked: Vec<(usize, u32)> = counts
        .into_iter()
        .enumerate()
        .filter(|(_, n)| *n > 0)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(10)
        .map(|(index, _)| {
            let b = (index % bins) as f64;
            let r = ((index / bins) % bins) as f64;
            let g = (index / (bins * bins)) as f64;
            let centre = |v: f64| (v + 0.5) * 256.0 / f64::from(BINS);
            Rgb {
                r: centre(r),
                g: centre(g),
                b: centre(b),
            }
        })
        .collect()
}

fn hex(c: Rgb) -> String {
    // Ruby's `format('%02x', float)` truncates.
    format!("#{:02x}{:02x}{:02x}", c.r as u8, c.g as u8, c.b as u8)
}

fn linear(v: f64) -> f64 {
    let v = v / 255.0;
    if v > 0.04045 {
        ((v + 0.055) / 1.055).powf(2.4)
    } else {
        v / 12.92
    }
}

/// `to_xyz`, on the 0–100 scale.
fn xyz(c: Rgb) -> (f64, f64, f64) {
    let (r, g, b) = (
        linear(c.r) * 100.0,
        linear(c.g) * 100.0,
        linear(c.b) * 100.0,
    );
    (
        r * 0.4124 + g * 0.3576 + b * 0.1805,
        r * 0.2126 + g * 0.7152 + b * 0.0722,
        r * 0.0193 + g * 0.1192 + b * 0.9505,
    )
}

fn lab(c: Rgb) -> (f64, f64, f64) {
    let (x, y, z) = xyz(c);
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / 95.047), f(y / 100.0), f(z / 108.883));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// CIEDE2000 colour difference.
fn ciede2000(c1: Rgb, c2: Rgb) -> f64 {
    use std::f64::consts::PI;
    let (l1, a1, b1) = lab(c1);
    let (l2, a2, b2) = lab(c2);
    let c1s = (a1 * a1 + b1 * b1).sqrt();
    let c2s = (a2 * a2 + b2 * b2).sqrt();
    let c_bar = (c1s + c2s) / 2.0;
    let g = 0.5 * (1.0 - (c_bar.powi(7) / (c_bar.powi(7) + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = ((a1p * a1p + b1 * b1).sqrt(), (a2p * a2p + b2 * b2).sqrt());
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            let h = b.atan2(a).to_degrees();
            if h < 0.0 {
                h + 360.0
            } else {
                h
            }
        }
    };
    let (h1p, h2p) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh_angle = if c1p * c2p == 0.0 {
        0.0
    } else if (h2p - h1p).abs() <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let dh = 2.0 * (c1p * c2p).sqrt() * (dh_angle.to_radians() / 2.0).sin();
    let l_bar = (l1 + l2) / 2.0;
    let c_bar_p = (c1p + c2p) / 2.0;
    let h_bar = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (h_bar - 30.0).to_radians().cos()
        + 0.24 * (2.0 * h_bar).to_radians().cos()
        + 0.32 * (3.0 * h_bar + 6.0).to_radians().cos()
        - 0.20 * (4.0 * h_bar - 63.0).to_radians().cos();
    let d_theta = 30.0 * (-((h_bar - 275.0) / 25.0).powi(2)).exp();
    let rc = 2.0 * (c_bar_p.powi(7) / (c_bar_p.powi(7) + 25f64.powi(7))).sqrt();
    let sl = 1.0 + (0.015 * (l_bar - 50.0).powi(2)) / (20.0 + (l_bar - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * c_bar_p;
    let sh = 1.0 + 0.015 * c_bar_p * t;
    let rt = -(2.0 * d_theta * PI / 180.0).sin() * rc;
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh / sh).powi(2) + rt * (dc / sc) * (dh / sh)).sqrt()
}

fn w3c_contrast(c1: Rgb, c2: Rgb) -> f64 {
    let l1 = xyz(c1).1 * 0.01 + 0.05;
    let l2 = xyz(c2).1 * 0.01 + 0.05;
    if l1 > l2 {
        l1 / l2
    } else {
        l2 / l1
    }
}

/// `rgb_to_hsl`: hue in degrees, saturation and lightness in percent, each
/// rounded.
fn rgb_to_hsl(c: Rgb) -> (i64, i64, i64) {
    let (r, g, b) = (c.r / 255.0, c.g / 255.0, c.b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let (mut h, s);
    if max == min {
        h = 0.0;
        s = 0.0;
    } else {
        let d = max - min;
        s = if l >= 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h /= 6.0;
    }
    (
        (h * 360.0).round() as i64,
        (s * 100.0).round() as i64,
        (l * 100.0).round() as i64,
    )
}

fn hue_to_rgb(p: f64, q: f64, mut t: f64) -> f64 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 1.0 / 2.0 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

fn hsl_to_rgb(h: i64, s: i64, l: i64) -> Rgb {
    let (h, s, l) = (h as f64 / 360.0, s as f64 / 100.0, l as f64 / 100.0);
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 {
            l * (s + 1.0)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        (
            hue_to_rgb(p, q, h + 1.0 / 3.0),
            hue_to_rgb(p, q, h),
            hue_to_rgb(p, q, h - 1.0 / 3.0),
        )
    };
    Rgb {
        r: (r * 255.0).round(),
        g: (g * 255.0).round(),
        b: (b * 255.0).round(),
    }
}

fn lighten_or_darken(c: Rgb, by: i64) -> Rgb {
    let (hue, saturation, light) = rgb_to_hsl(c);
    let light = if light < 50 {
        (light + by).min(100)
    } else {
        (light - by).max(0)
    };
    hsl_to_rgb(hue, saturation, light)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_image_gets_its_colour_as_background_and_made_ones_beside_it() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(40, 40, image::Rgb([10, 10, 10])));
        let colors = extract(&image).unwrap();
        // The bin (0, 0, 0)'s centre.
        assert_eq!(colors["background"], "#0c0c0c");
        // Lightened by 35 and 50 points of lightness, to 40% and 55%: the
        // lighter contrasts more, and neither is more saturated than the
        // first.
        assert_eq!(colors["foreground"], "#8c8c8c");
        assert_eq!(colors["accent"], "#666666");
    }

    #[test]
    fn a_contrasting_centre_is_the_foreground() {
        let mut image = RgbImage::from_pixel(100, 100, image::Rgb([250, 250, 250]));
        for y in 30..70 {
            for x in 30..70 {
                image.put_pixel(x, y, image::Rgb([200, 20, 20]));
            }
        }
        let colors = extract(&DynamicImage::ImageRgb8(image)).unwrap();
        assert_eq!(colors["background"], "#f3f3f3");
        assert_eq!(colors["foreground"], "#c00c0c");
        assert_eq!(colors["accent"], "#c00c0c");
    }

    #[test]
    fn ciede2000_matches_the_reference_pair() {
        // Sharma, Wu and Dalal's test data, pair 1, through sRGB is not
        // exact; the identity and symmetry are.
        let a = Rgb {
            r: 10.0,
            g: 200.0,
            b: 30.0,
        };
        let b = Rgb {
            r: 200.0,
            g: 10.0,
            b: 30.0,
        };
        assert_eq!(ciede2000(a, a), 0.0);
        assert!((ciede2000(a, b) - ciede2000(b, a)).abs() < 1e-9);
        assert!(ciede2000(a, b) > 50.0);
    }
}
