//! On-the-fly image transformations for `GET /assets/:id` (resize, crop, format conversion).
//!
//! Everything here is pure and synchronous (decode → transform → encode): the server runs it on a blocking
//! thread, behind a concurrency gate and an in-memory cache. Metadata (EXIF, ICC, ...) is dropped from the
//! output; the EXIF orientation of the source is applied first.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;

use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage};

use crate::{FerroError, Result};

/// Hard limits applied to every transformation.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest width or height of the output.
    pub max_dimension: u32,
    /// Largest `width * height` of a source image that will be decoded (decompression-bomb guard).
    pub max_input_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_dimension: 4096, max_input_pixels: 64_000_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Fill the box and crop what overflows (aligned by `position`).
    Cover,
    /// Fit inside the box and pad the rest with the background colour; the output is exactly the box.
    Contain,
    /// Fit inside the box keeping the ratio, no padding (the output can be smaller than the box).
    Inside,
    /// Cover the box keeping the ratio, no cropping (the output can be larger than the box).
    Outside,
    /// Stretch to exactly the box, ignoring the ratio.
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFormat {
    Jpeg,
    Png,
    /// Lossless WebP.
    Webp,
}

impl OutFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            OutFormat::Jpeg => "image/jpeg",
            OutFormat::Png => "image/png",
            OutFormat::Webp => "image/webp",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            OutFormat::Jpeg => "jpg",
            OutFormat::Png => "png",
            OutFormat::Webp => "webp",
        }
    }
}

/// Raw, unvalidated parameters as they arrive in the query string.
#[derive(Debug, Clone, Default)]
pub struct RawParams {
    pub width: Option<String>,
    pub height: Option<String>,
    pub fit: Option<String>,
    pub position: Option<String>,
    pub crop: Option<String>,
    pub format: Option<String>,
    pub quality: Option<String>,
    pub without_enlargement: Option<String>,
    pub background: Option<String>,
}

/// Validated, normalised parameters. Two requests that mean the same thing compare (and `cache_key`) equal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransformParams {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fit: Option<Fit>,
    /// Horizontal and vertical alignment of a `cover` crop, 0.0 (left/top) .. 1.0 (right/bottom).
    pub position: Option<(u8, u8)>,
    /// Source rectangle `[x, y, w, h]`, applied before resizing.
    pub crop: Option<[u32; 4]>,
    pub format: Option<OutFormat>,
    pub quality: Option<u8>,
    pub without_enlargement: bool,
    pub background: Option<[u8; 3]>,
}

fn parse_num(name: &str, v: &str) -> Result<u32> {
    v.trim().parse::<u32>().map_err(|_| FerroError::invalid(format!("`{name}` must be a positive integer")))
}

fn parse_bool(name: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(FerroError::invalid(format!("`{name}` must be true or false"))),
    }
}

impl TransformParams {
    pub fn parse(raw: &RawParams, limits: &Limits) -> Result<TransformParams> {
        let mut p = TransformParams::default();
        for (name, value, slot) in [("width", &raw.width, &mut p.width), ("height", &raw.height, &mut p.height)] {
            if let Some(v) = value {
                let n = parse_num(name, v)?;
                if n == 0 || n > limits.max_dimension {
                    return Err(FerroError::invalid(format!("`{name}` must be between 1 and {}", limits.max_dimension)));
                }
                *slot = Some(n);
            }
        }
        if let Some(v) = &raw.fit {
            p.fit = Some(match v.trim().to_ascii_lowercase().as_str() {
                "cover" => Fit::Cover,
                "contain" => Fit::Contain,
                "inside" => Fit::Inside,
                "outside" => Fit::Outside,
                "fill" => Fit::Fill,
                _ => return Err(FerroError::invalid("`fit` must be cover, contain, inside, outside or fill")),
            });
        }
        if let Some(v) = &raw.position {
            let v = v.trim().to_ascii_lowercase().replace(['-', ' '], "_");
            let (x, y) = match v.as_str() {
                "center" | "centre" => (1, 1),
                "top" => (1, 0),
                "bottom" => (1, 2),
                "left" => (0, 1),
                "right" => (2, 1),
                "top_left" | "left_top" => (0, 0),
                "top_right" | "right_top" => (2, 0),
                "bottom_left" | "left_bottom" => (0, 2),
                "bottom_right" | "right_bottom" => (2, 2),
                _ => return Err(FerroError::invalid("`position` must be center, top, bottom, left, right or a corner such as top_left")),
            };
            p.position = Some((x, y));
        }
        if let Some(v) = &raw.crop {
            let parts: Vec<&str> = v.split(',').collect();
            if parts.len() != 4 {
                return Err(FerroError::invalid("`crop` must be x,y,width,height (pixels of the original)"));
            }
            let n: Vec<u32> = parts.iter().map(|s| parse_num("crop", s)).collect::<Result<_>>()?;
            if n[2] == 0 || n[3] == 0 {
                return Err(FerroError::invalid("`crop` width and height must be at least 1"));
            }
            p.crop = Some([n[0], n[1], n[2], n[3]]);
        }
        if let Some(v) = &raw.format {
            p.format = Some(match v.trim().to_ascii_lowercase().as_str() {
                "jpg" | "jpeg" => OutFormat::Jpeg,
                "png" => OutFormat::Png,
                "webp" => OutFormat::Webp,
                _ => return Err(FerroError::invalid("`format` must be jpg, png or webp")),
            });
        }
        if let Some(v) = &raw.quality {
            let q = parse_num("quality", v)?;
            if !(1..=100).contains(&q) {
                return Err(FerroError::invalid("`quality` must be between 1 and 100"));
            }
            p.quality = Some(q as u8);
        }
        if let Some(v) = &raw.without_enlargement {
            p.without_enlargement = parse_bool("withoutEnlargement", v)?;
        }
        if let Some(v) = &raw.background {
            let h = v.trim().trim_start_matches('#');
            let ok = h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(FerroError::invalid("`background` must be a hex colour such as ffffff"));
            }
            let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
            p.background = Some([b(0), b(2), b(4)]);
        }
        // Parameters that need others to mean anything are rejected instead of silently ignored.
        if p.fit.is_some() && p.width.is_none() && p.height.is_none() {
            return Err(FerroError::invalid("`fit` needs `width` and/or `height`"));
        }
        if p.position.is_some() && p.fit.map(|f| f != Fit::Cover).unwrap_or(true) && !(p.width.is_some() && p.height.is_some()) {
            return Err(FerroError::invalid("`position` applies to fit=cover with both `width` and `height`"));
        }
        Ok(p)
    }

    /// No transformation requested: the original bytes are served untouched.
    pub fn is_noop(&self) -> bool {
        *self == TransformParams::default()
    }

    /// Stable identity of the request (cache key, ETag).
    pub fn cache_key(&self) -> String {
        format!("{self:?}")
    }
}

/// Result of a transformation.
#[derive(Debug, Clone)]
pub struct Transformed {
    pub data: Bytes,
    pub format: OutFormat,
    pub width: u32,
    pub height: u32,
}

fn source_format(data: &[u8]) -> Result<ImageFormat> {
    match image::guess_format(data) {
        Ok(f @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP | ImageFormat::Bmp | ImageFormat::Tiff)) => Ok(f),
        _ => Err(FerroError::invalid("only JPEG, PNG, GIF, WebP, BMP and TIFF images can be transformed")),
    }
}

/// Width and height of a supported image, read from its header only (no decoding). `None` for anything else
/// (SVG, PDF, text, corrupt files).
pub fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let fmt = source_format(data).ok()?;
    ImageReader::with_format(Cursor::new(data), fmt).into_dimensions().ok()
}

/// Refuse to allocate a bitmap bigger than the configured output limit (checked *before* resizing: an extreme
/// aspect ratio would otherwise turn `width=4096` on a 1x4000 image into a multi-gigabyte allocation).
fn guard(w: u32, h: u32, limits: &Limits) -> Result<()> {
    if w.max(h) > limits.max_dimension {
        return Err(FerroError::invalid(format!(
            "the result would be {w}x{h}, above the limit of {} pixels per side",
            limits.max_dimension
        )));
    }
    Ok(())
}

fn scaled(v: u32, scale: f64) -> u32 {
    ((v as f64 * scale).round() as u32).max(1)
}

/// Decode, transform and encode. CPU-bound: call it from a blocking thread.
pub fn transform(data: &[u8], p: &TransformParams, limits: &Limits) -> Result<Transformed> {
    let fmt = source_format(data)?;
    let (sw, sh) = ImageReader::with_format(Cursor::new(data), fmt)
        .into_dimensions()
        .map_err(|_| FerroError::invalid("the image is corrupt or cannot be read"))?;
    if (sw as u64) * (sh as u64) > limits.max_input_pixels {
        return Err(FerroError::invalid(format!(
            "the image is too large to transform ({sw}x{sh}, at most {} megapixels)",
            limits.max_input_pixels / 1_000_000
        )));
    }
    let mut decoder = ImageReader::with_format(Cursor::new(data), fmt)
        .into_decoder()
        .map_err(|_| FerroError::invalid("the image is corrupt or cannot be read"))?;
    let mut lim = image::Limits::default();
    lim.max_alloc = Some(limits.max_input_pixels.saturating_mul(8));
    decoder.set_limits(lim).map_err(|_| FerroError::invalid("the image is too large to transform"))?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| FerroError::invalid("the image is corrupt or cannot be read"))?;
    img.apply_orientation(orientation);

    // 1. explicit crop, in pixels of the (oriented) original
    if let Some([x, y, w, h]) = p.crop {
        let (cw, ch) = (img.width(), img.height());
        if x.checked_add(w).map(|e| e > cw).unwrap_or(true) || y.checked_add(h).map(|e| e > ch).unwrap_or(true) {
            return Err(FerroError::invalid(format!("`crop` must lie inside the image ({cw}x{ch})")));
        }
        img = img.crop_imm(x, y, w, h);
    }

    // 2. resize
    let out_format = p.format.unwrap_or(match fmt {
        ImageFormat::Jpeg => OutFormat::Jpeg,
        ImageFormat::WebP => OutFormat::Webp,
        _ => OutFormat::Png,
    });
    let bg_rgb = p.background.or(if out_format == OutFormat::Jpeg { Some([255, 255, 255]) } else { None });
    let (cw, ch) = (img.width(), img.height());
    let mut canvas: Option<(u32, u32)> = None; // `contain`: padded box
    if p.width.is_some() || p.height.is_some() {
        let fit = p.fit.unwrap_or(if p.width.is_some() && p.height.is_some() { Fit::Cover } else { Fit::Inside });
        let (tw, th) = match (p.width, p.height) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, scaled(ch, w as f64 / cw as f64)),
            (None, Some(h)) => (scaled(cw, h as f64 / ch as f64), h),
            (None, None) => unreachable!(),
        };
        guard(tw, th, limits)?;
        let cap = |s: f64| if p.without_enlargement { s.min(1.0) } else { s };
        let (sx, sy) = (tw as f64 / cw as f64, th as f64 / ch as f64);
        match fit {
            Fit::Fill => {
                let (w, h) = if p.without_enlargement { (tw.min(cw), th.min(ch)) } else { (tw, th) };
                guard(w, h, limits)?;
                img = img.resize_exact(w, h, FilterType::Lanczos3);
            }
            Fit::Inside => {
                let s = cap(sx.min(sy));
                let (w, h) = (scaled(cw, s), scaled(ch, s));
                guard(w, h, limits)?;
                img = img.resize_exact(w, h, FilterType::Lanczos3);
            }
            Fit::Outside => {
                let s = cap(sx.max(sy));
                let (w, h) = (scaled(cw, s), scaled(ch, s));
                guard(w, h, limits)?;
                img = img.resize_exact(w, h, FilterType::Lanczos3);
            }
            Fit::Cover => {
                // cut the window of the original that has the box's aspect ratio, then scale only that window
                // (no oversized intermediate bitmap)
                let s_raw = sx.max(sy);
                let capped = p.without_enlargement && s_raw > 1.0;
                let (ww, wh, ow, oh) = if capped {
                    let (w, h) = (tw.min(cw), th.min(ch));
                    (w, h, w, h)
                } else {
                    let w = ((tw as f64 / s_raw).round() as u32).clamp(1, cw);
                    let h = ((th as f64 / s_raw).round() as u32).clamp(1, ch);
                    (w, h, tw, th)
                };
                guard(ow, oh, limits)?;
                let (px, py) = p.position.unwrap_or((1, 1));
                let at = |pos: u8, room: u32| (room as u64 * pos as u64 / 2) as u32;
                img = img.crop_imm(at(px, cw - ww), at(py, ch - wh), ww, wh);
                if (ww, wh) != (ow, oh) {
                    img = img.resize_exact(ow, oh, FilterType::Lanczos3);
                }
            }
            Fit::Contain => {
                let s = cap(sx.min(sy));
                let (w, h) = (scaled(cw, s), scaled(ch, s));
                guard(w, h, limits)?;
                guard(tw, th, limits)?;
                img = img.resize_exact(w, h, FilterType::Lanczos3);
                // padding only when the box is really reached: with `withoutEnlargement` a small image stays as it is
                if !(p.without_enlargement && (img.width() < tw || img.height() < th)) {
                    canvas = Some((tw, th));
                }
            }
        }
    }
    if let Some((bw, bh)) = canvas {
        let fill = match bg_rgb {
            Some([r, g, b]) => Rgba([r, g, b, 255]),
            None => Rgba([0, 0, 0, 0]),
        };
        let mut base = RgbaImage::from_pixel(bw, bh, fill);
        let (ox, oy) = ((bw - img.width()) / 2, (bh - img.height()) / 2);
        imageops::overlay(&mut base, &img.to_rgba8(), ox as i64, oy as i64);
        img = DynamicImage::ImageRgba8(base);
    }
    if img.width().max(img.height()) > limits.max_dimension {
        return Err(FerroError::invalid(format!("the result would exceed {} pixels per side", limits.max_dimension)));
    }

    // 3. encode
    let (width, height) = (img.width(), img.height());
    let mut out = Vec::new();
    let fail = |e: image::ImageError| FerroError::Internal(format!("cannot encode the image: {e}"));
    match out_format {
        OutFormat::Jpeg => {
            // JPEG has no alpha: flatten onto the background
            let rgb = if img.color().has_alpha() {
                let [r, g, b] = bg_rgb.unwrap_or([255, 255, 255]);
                let mut base = RgbaImage::from_pixel(width, height, Rgba([r, g, b, 255]));
                imageops::overlay(&mut base, &img.to_rgba8(), 0, 0);
                DynamicImage::ImageRgba8(base).to_rgb8()
            } else {
                img.to_rgb8()
            };
            let enc = JpegEncoder::new_with_quality(&mut out, p.quality.unwrap_or(80));
            DynamicImage::ImageRgb8(rgb).write_with_encoder(enc).map_err(fail)?;
        }
        OutFormat::Png => {
            let norm = if img.color().has_alpha() { DynamicImage::ImageRgba8(img.to_rgba8()) } else { DynamicImage::ImageRgb8(img.to_rgb8()) };
            norm.write_with_encoder(PngEncoder::new(&mut out)).map_err(fail)?;
        }
        OutFormat::Webp => {
            let norm = if img.color().has_alpha() { DynamicImage::ImageRgba8(img.to_rgba8()) } else { DynamicImage::ImageRgb8(img.to_rgb8()) };
            norm.write_with_encoder(WebPEncoder::new_lossless(&mut out)).map_err(fail)?;
        }
    }
    Ok(Transformed { data: Bytes::from(out), format: out_format, width, height })
}

/// Bounded in-memory cache of transformed images (per instance, least recently used goes first).
pub struct TransformCache {
    budget: usize,
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    map: HashMap<String, (Bytes, OutFormat, u64)>,
    bytes: usize,
    tick: u64,
}

impl TransformCache {
    /// `budget` in bytes; 0 disables the cache.
    pub fn new(budget: usize) -> TransformCache {
        TransformCache { budget, inner: Mutex::new(CacheInner::default()) }
    }

    pub fn get(&self, key: &str) -> Option<(Bytes, OutFormat)> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.tick += 1;
        let tick = g.tick;
        let e = g.map.get_mut(key)?;
        e.2 = tick;
        Some((e.0.clone(), e.1))
    }

    pub fn put(&self, key: String, data: Bytes, format: OutFormat) {
        // an entry larger than a quarter of the budget would just flush everything else
        if self.budget == 0 || data.len() > self.budget / 4 {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.tick += 1;
        let tick = g.tick;
        g.bytes += data.len();
        if let Some(old) = g.map.insert(key, (data, format, tick)) {
            g.bytes -= old.0.len();
        }
        while g.bytes > self.budget {
            let Some(oldest) = g.map.iter().min_by_key(|(_, v)| v.2).map(|(k, _)| k.clone()) else { break };
            if let Some(v) = g.map.remove(&oldest) {
                g.bytes -= v.0.len();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 coloured quadrants: red | green on top, blue | yellow below.
    fn quadrants(w: u32, h: u32, alpha: u8) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| match (x < w / 2, y < h / 2) {
            (true, true) => Rgba([255, 0, 0, alpha]),
            (false, true) => Rgba([0, 255, 0, alpha]),
            (true, false) => Rgba([0, 0, 255, alpha]),
            (false, false) => Rgba([255, 255, 0, alpha]),
        })
    }

    fn encode(img: &RgbaImage, fmt: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        match fmt {
            ImageFormat::Jpeg => DynamicImage::ImageRgba8(img.clone()).to_rgb8().write_to(&mut out, fmt).unwrap(),
            _ => img.write_to(&mut out, fmt).unwrap(),
        }
        out.into_inner()
    }

    fn params(f: impl FnOnce(&mut RawParams)) -> TransformParams {
        let mut raw = RawParams::default();
        f(&mut raw);
        TransformParams::parse(&raw, &Limits::default()).unwrap()
    }

    fn decode(t: &Transformed) -> RgbaImage {
        image::load_from_memory(&t.data).unwrap().to_rgba8()
    }

    fn near(p: &Rgba<u8>, rgb: [u8; 3]) -> bool {
        (0..3).all(|i| (p[i] as i32 - rgb[i] as i32).abs() < 40)
    }

    #[test]
    fn parses_and_validates_parameters() {
        let l = Limits::default();
        let bad = |f: &dyn Fn(&mut RawParams)| {
            let mut r = RawParams::default();
            f(&mut r);
            TransformParams::parse(&r, &l).is_err()
        };
        assert!(bad(&|r| r.width = Some("0".into())));
        assert!(bad(&|r| r.width = Some("4097".into())));
        assert!(bad(&|r| r.width = Some("-5".into())));
        assert!(bad(&|r| r.width = Some("abc".into())));
        assert!(bad(&|r| r.fit = Some("stretch".into())));
        assert!(bad(&|r| r.fit = Some("cover".into())), "fit without a size");
        assert!(bad(&|r| r.format = Some("avif".into())));
        assert!(bad(&|r| r.quality = Some("0".into())));
        assert!(bad(&|r| r.quality = Some("101".into())));
        assert!(bad(&|r| r.crop = Some("1,2,3".into())));
        assert!(bad(&|r| r.crop = Some("0,0,0,10".into())));
        assert!(bad(&|r| r.background = Some("red".into())));
        assert!(bad(&|r| r.position = Some("middle".into())));
        assert!(bad(&|r| {
            r.width = Some("10".into());
            r.position = Some("top".into())
        }), "position needs both sides");
        let p = params(|r| {
            r.width = Some(" 100 ".into());
            r.format = Some("JPEG".into());
            r.without_enlargement = Some("true".into());
            r.background = Some("#00ff80".into());
        });
        assert_eq!((p.width, p.format, p.without_enlargement, p.background), (Some(100), Some(OutFormat::Jpeg), true, Some([0, 255, 128])));
        assert!(TransformParams::default().is_noop() && !p.is_noop());
        assert_eq!(params(|r| r.format = Some("jpg".into())).cache_key(), params(|r| r.format = Some("JPEG".into())).cache_key());
    }

    #[test]
    fn resize_keeps_the_ratio_with_one_dimension() {
        let src = encode(&quadrants(400, 200, 255), ImageFormat::Png);
        let t = transform(&src, &params(|r| r.width = Some("100".into())), &Limits::default()).unwrap();
        assert_eq!((t.width, t.height, t.format), (100, 50, OutFormat::Png));
        let t = transform(&src, &params(|r| r.height = Some("20".into())), &Limits::default()).unwrap();
        assert_eq!((t.width, t.height), (40, 20));
    }

    #[test]
    fn fit_modes() {
        let src = encode(&quadrants(400, 200, 255), ImageFormat::Png); // 2:1
        let run = |fit: &str| {
            let t = transform(&src, &params(|r| {
                r.width = Some("100".into());
                r.height = Some("100".into());
                r.fit = Some(fit.into());
            }), &Limits::default())
            .unwrap();
            (t.width, t.height)
        };
        assert_eq!(run("cover"), (100, 100));
        assert_eq!(run("fill"), (100, 100));
        assert_eq!(run("contain"), (100, 100));
        assert_eq!(run("inside"), (100, 50));
        assert_eq!(run("outside"), (200, 100));
        // both sizes without `fit` mean cover
        let t = transform(&src, &params(|r| {
            r.width = Some("30".into());
            r.height = Some("60".into());
        }), &Limits::default())
        .unwrap();
        assert_eq!((t.width, t.height), (30, 60));
    }

    #[test]
    fn cover_crops_where_position_says() {
        let src = encode(&quadrants(400, 200, 255), ImageFormat::Png);
        let at = |pos: &str| {
            let t = transform(&src, &params(|r| {
                r.width = Some("100".into());
                r.height = Some("100".into());
                r.position = Some(pos.into());
            }), &Limits::default())
            .unwrap();
            decode(&t)
        };
        // 400x200 → 200x100 scaled → a 100x100 window: left half is red/blue, right half green/yellow
        let left = at("left");
        assert!(near(left.get_pixel(10, 10), [255, 0, 0]) && near(left.get_pixel(10, 90), [0, 0, 255]));
        let right = at("right");
        assert!(near(right.get_pixel(90, 10), [0, 255, 0]) && near(right.get_pixel(90, 90), [255, 255, 0]));
        let centre = at("center"); // the window straddles the vertical middle line
        assert!(near(centre.get_pixel(10, 10), [255, 0, 0]) && near(centre.get_pixel(90, 10), [0, 255, 0]));
    }

    #[test]
    fn explicit_crop_then_resize() {
        let src = encode(&quadrants(400, 200, 255), ImageFormat::Png);
        // the yellow quadrant only
        let t = transform(&src, &params(|r| {
            r.crop = Some("200,100,200,100".into());
            r.width = Some("50".into());
        }), &Limits::default())
        .unwrap();
        assert_eq!((t.width, t.height), (50, 25));
        let img = decode(&t);
        assert!(img.pixels().all(|p| near(p, [255, 255, 0])));
        // outside the image, or overflowing integers
        for c in ["300,0,200,10", "0,150,10,100", "4294967295,0,10,10"] {
            let r = transform(&src, &params(|r| r.crop = Some(c.into())), &Limits::default());
            assert!(matches!(r, Err(FerroError::Invalid(_))), "{c}");
        }
    }

    #[test]
    fn formats_and_transparency() {
        let png = encode(&quadrants(40, 40, 128), ImageFormat::Png); // half transparent
        let as_jpg = transform(&png, &params(|r| r.format = Some("jpg".into())), &Limits::default()).unwrap();
        assert_eq!(&as_jpg.data[..3], &[0xFF, 0xD8, 0xFF]);
        let flattened = decode(&as_jpg);
        assert_eq!(flattened.get_pixel(5, 5)[3], 255, "JPEG has no alpha");
        assert!(flattened.get_pixel(5, 5)[1] > 100, "red at 50% over white is pinkish, not black");
        let as_webp = transform(&png, &params(|r| r.format = Some("webp".into())), &Limits::default()).unwrap();
        assert_eq!(&as_webp.data[..4], b"RIFF");
        assert_eq!(decode(&as_webp).get_pixel(5, 5)[3], 128, "WebP keeps the alpha channel");
        // a JPEG stays a JPEG, a GIF/BMP becomes a PNG
        let jpg = encode(&quadrants(40, 40, 255), ImageFormat::Jpeg);
        assert_eq!(transform(&jpg, &params(|r| r.width = Some("20".into())), &Limits::default()).unwrap().format, OutFormat::Jpeg);
        let bmp = encode(&quadrants(40, 40, 255), ImageFormat::Bmp);
        assert_eq!(transform(&bmp, &params(|r| r.width = Some("20".into())), &Limits::default()).unwrap().format, OutFormat::Png);
        // quality changes the size of a JPEG
        let photo = RgbaImage::from_fn(200, 200, |x, y| Rgba([(x * 7 + y * 3) as u8, (x * y) as u8, (x ^ y) as u8, 255]));
        let big = encode(&photo, ImageFormat::Png);
        let hi = transform(&big, &params(|r| { r.format = Some("jpg".into()); r.quality = Some("95".into()) }), &Limits::default()).unwrap();
        let lo = transform(&big, &params(|r| { r.format = Some("jpg".into()); r.quality = Some("10".into()) }), &Limits::default()).unwrap();
        assert!(hi.data.len() > lo.data.len() * 2, "{} vs {}", hi.data.len(), lo.data.len());
    }

    #[test]
    fn contain_pads_with_the_background() {
        let src = encode(&quadrants(400, 200, 255), ImageFormat::Png);
        let t = transform(&src, &params(|r| {
            r.width = Some("100".into());
            r.height = Some("100".into());
            r.fit = Some("contain".into());
            r.background = Some("000000".into());
        }), &Limits::default())
        .unwrap();
        let img = decode(&t);
        assert_eq!(*img.get_pixel(50, 5), Rgba([0, 0, 0, 255]), "padding above");
        assert!(near(img.get_pixel(10, 40), [255, 0, 0]));
        // without a background a PNG is padded with transparency
        let t = transform(&src, &params(|r| {
            r.width = Some("100".into());
            r.height = Some("100".into());
            r.fit = Some("contain".into());
        }), &Limits::default())
        .unwrap();
        assert_eq!(decode(&t).get_pixel(50, 5)[3], 0);
    }

    #[test]
    fn without_enlargement_never_scales_up() {
        let src = encode(&quadrants(100, 50, 255), ImageFormat::Png);
        for fit in ["cover", "contain", "inside", "outside", "fill"] {
            let t = transform(&src, &params(|r| {
                r.width = Some("400".into());
                r.height = Some("400".into());
                r.fit = Some(fit.into());
                r.without_enlargement = Some("true".into());
            }), &Limits::default())
            .unwrap();
            assert!(t.width <= 100 && t.height <= 50, "{fit}: {}x{}", t.width, t.height);
        }
        let t = transform(&src, &params(|r| r.width = Some("400".into())), &Limits::default()).unwrap();
        assert_eq!((t.width, t.height), (400, 200), "enlarging is allowed by default");
    }

    #[test]
    fn exif_orientation_is_applied() {
        // a landscape JPEG whose EXIF says "rotate 90°" must come out as portrait
        let jpg = encode(&quadrants(40, 20, 255), ImageFormat::Jpeg);
        let with_exif = insert_exif_orientation(&jpg, 6);
        let t = transform(&with_exif, &params(|r| r.format = Some("png".into())), &Limits::default()).unwrap();
        assert_eq!((t.width, t.height), (20, 40));
        assert_eq!(dimensions(&with_exif), Some((40, 20)), "the header still says 40x20");
    }

    /// JPEG APP1/EXIF segment with a single Orientation tag, inserted right after SOI.
    fn insert_exif_orientation(jpg: &[u8], orientation: u16) -> Vec<u8> {
        let mut tiff = vec![b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1];
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut seg = b"Exif\0\0".to_vec();
        seg.extend_from_slice(&tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&seg);
        out.extend_from_slice(&jpg[2..]);
        out
    }

    #[test]
    fn refuses_what_it_cannot_or_should_not_decode() {
        let l = Limits::default();
        let p = params(|r| r.width = Some("10".into()));
        for junk in [&b"<svg xmlns='http://www.w3.org/2000/svg'/>"[..], b"%PDF-1.4", b"plain text", b""] {
            assert!(matches!(transform(junk, &p, &l), Err(FerroError::Invalid(_))));
            assert_eq!(dimensions(junk), None);
        }
        // truncated PNG: header fine, body gone
        let png = encode(&quadrants(50, 50, 255), ImageFormat::Png);
        assert!(matches!(transform(&png[..png.len() / 2], &p, &l), Err(FerroError::Invalid(_))));
        // decompression-bomb guard: judged from the header
        let tight = Limits { max_dimension: 4096, max_input_pixels: 1000 };
        let err = transform(&png, &p, &tight).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
        // an output beyond the dimension limit
        let small = Limits { max_dimension: 60, max_input_pixels: 64_000_000 };
        assert!(transform(&png, &params(|r| r.width = Some("60".into())), &small).is_ok());
        let mut raw = RawParams::default();
        raw.width = Some("61".into());
        assert!(TransformParams::parse(&raw, &small).is_err());
        let scale_up = transform(&png, &TransformParams { crop: Some([0, 0, 50, 50]), width: Some(60), height: Some(60), ..Default::default() }, &small);
        assert!(scale_up.is_ok());
    }

    #[test]
    fn extreme_aspect_ratios_cannot_allocate_giant_bitmaps() {
        let l = Limits::default();
        let tall = encode(&RgbaImage::from_pixel(1, 3000, Rgba([9, 9, 9, 255])), ImageFormat::Png);
        // width=4096 on a 1x3000 image would be 4096x12_288_000 pixels: refused up front, not attempted
        for fit in [None, Some("inside"), Some("outside"), Some("fill"), Some("contain")] {
            let r = transform(&tall, &params(|r| {
                r.width = Some("4096".into());
                r.fit = fit.map(String::from);
            }), &l);
            assert!(matches!(r, Err(FerroError::Invalid(_))), "{fit:?}: {r:?}");
        }
        let wide = encode(&RgbaImage::from_pixel(3000, 1, Rgba([9, 9, 9, 255])), ImageFormat::Png);
        assert!(matches!(transform(&wide, &params(|r| r.height = Some("4096".into())), &l), Err(FerroError::Invalid(_))));
        // cover works on the same inputs and stays within the box
        let t = transform(&tall, &params(|r| {
            r.width = Some("300".into());
            r.height = Some("300".into());
        }), &l)
        .unwrap();
        assert_eq!((t.width, t.height), (300, 300));
        let t = transform(&wide, &params(|r| {
            r.width = Some("64".into());
            r.height = Some("64".into());
            r.position = Some("right".into());
        }), &l)
        .unwrap();
        assert_eq!((t.width, t.height), (64, 64));
        // a sane request on a thin image still works
        assert_eq!(transform(&tall, &params(|r| r.height = Some("100".into())), &l).map(|t| (t.width, t.height)).unwrap(), (1, 100));
    }

    #[test]
    fn dimensions_come_from_the_header() {
        assert_eq!(dimensions(&encode(&quadrants(30, 20, 255), ImageFormat::Png)), Some((30, 20)));
        assert_eq!(dimensions(&encode(&quadrants(30, 20, 255), ImageFormat::Jpeg)), Some((30, 20)));
        assert_eq!(dimensions(&encode(&quadrants(30, 20, 255), ImageFormat::Gif)), Some((30, 20)));
        assert_eq!(dimensions(&encode(&quadrants(30, 20, 255), ImageFormat::WebP)), Some((30, 20)));
    }

    #[test]
    fn cache_is_bounded_and_least_recently_used() {
        let c = TransformCache::new(100);
        let blob = |n: usize| Bytes::from(vec![0u8; n]);
        c.put("a".into(), blob(20), OutFormat::Png);
        c.put("b".into(), blob(20), OutFormat::Png);
        c.put("c".into(), blob(20), OutFormat::Png);
        assert!(c.get("a").is_some()); // refresh a: b is now the oldest
        c.put("d".into(), blob(20), OutFormat::Png);
        c.put("e".into(), blob(20), OutFormat::Png);
        assert_eq!(c.len(), 5);
        c.put("f".into(), blob(20), OutFormat::Png); // 120 > 100: evicts b
        assert!(c.get("b").is_none() && c.get("a").is_some() && c.get("f").is_some());
        c.put("huge".into(), blob(60), OutFormat::Png); // more than a quarter of the budget: not cached
        assert!(c.get("huge").is_none());
        c.put("a".into(), blob(10), OutFormat::Jpeg); // replacing accounts for the old size
        assert_eq!(c.get("a").unwrap().1, OutFormat::Jpeg);
        assert!(TransformCache::new(0).is_empty());
        let off = TransformCache::new(0);
        off.put("x".into(), blob(1), OutFormat::Png);
        assert!(off.get("x").is_none());
    }
}
