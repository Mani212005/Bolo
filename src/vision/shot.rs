//! Turns a captured display frame into the screenshot an agent reads well.
//!
//! A full-screen capture is several times larger than a vision model's pixel
//! budget, so the model shrinks it and small text turns to smudge. A person
//! taking a screenshot for an LLM crops to the part they want understood, so
//! Bolo does the same:
//!
//! 1. Pick the region the user points at: the UI element under the pointer
//!    (with padding) when the platform can name one, otherwise a 640x400
//!    logical box around the pointer ([`choose_region`]). A table row gets
//!    its table's header and columns too ([`choose_region_in_frame`]).
//! 2. Crop it from the native-resolution frame of the display under the
//!    pointer, and downscale only when it exceeds the model budget
//!    ([`Budget`]), with Lanczos3. Never upscale.
//! 3. Frame it with a 3 px border added around the pixels, so the highlight
//!    never covers content ([`add_border`]).
//! 4. Keep a small full-screen context image with the region outlined
//!    ([`context_thumbnail`]); it is sent only when it earns its tokens
//!    ([`wants_context`]).
//! 5. Encode as lossless 8-bit RGB PNG with no metadata ([`encode_png`]).
//!
//! Everything here is pure and display-free so it is unit tested on fixtures.

use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::{Rgb, RgbImage};
use std::path::Path;

#[cfg(any(target_os = "macos", test))]
use super::table;

/// Claude bills and encodes images on a 28x28 px grid.
pub const VISUAL_TOKEN_PX: u32 = 28;
/// The pointer box when no element is known, in logical points. At 2x it is
/// 1280x800 px, which fits the standard budget with no resampling.
#[cfg(any(target_os = "macos", test))]
pub const DEFAULT_BOX: (f64, f64) = (640.0, 400.0);
/// Space kept around an accessibility element, in logical points.
#[cfg(any(target_os = "macos", test))]
pub const ELEMENT_PADDING: f64 = 16.0;
/// Below this padded size an element (one word, one icon) carries too little
/// context on its own, so its parent is tried instead.
#[cfg(any(target_os = "macos", test))]
pub const MIN_ELEMENT: (f64, f64) = (240.0, 80.0);
/// An element covering more than this share of the display is a window or a
/// whole editor, not something pointed at; the pointer box is used instead.
#[cfg(any(target_os = "macos", test))]
pub const MAX_ELEMENT_FRACTION: f64 = 0.5;
/// Width of the box drawn on the pointed row of a table crop, in output pixels.
#[cfg(any(target_os = "macos", test))]
pub const ROW_MARK_PX: i64 = 2;
/// How strongly the pointed row is tinted on the context image.
#[cfg(any(target_os = "macos", test))]
pub const ROW_BAND_ALPHA: f64 = 0.55;
/// Width of the frame drawn around the crop, in output pixels.
pub const BORDER_PX: u32 = 3;
/// `#00BFFF`, readable on both light and dark UI.
pub const BORDER_COLOR: Rgb<u8> = Rgb([0x00, 0xBF, 0xFF]);
/// Long edge of the full-screen context image.
pub const CONTEXT_LONG_EDGE: u32 = 768;
/// A crop smaller than this share of the display gets the context image.
pub const CONTEXT_FRACTION: f64 = 0.35;

/// A rectangle in logical points (macOS global display coordinates).
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[cfg(any(target_os = "macos", test))]
impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn centered(center: (f64, f64), w: f64, h: f64) -> Self {
        Self::new(center.0 - w / 2.0, center.1 - h / 2.0, w, h)
    }

    pub fn area(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }

    pub fn padded(&self, d: f64) -> Self {
        Self::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    /// Moves the rectangle inside `bounds` without shrinking it; a rectangle
    /// larger than `bounds` on an axis is cut to `bounds` on that axis.
    /// Shifting keeps the requested size at a screen edge, where cutting
    /// would leave a sliver.
    pub fn shifted_inside(&self, bounds: Rect) -> Self {
        let fit = |pos: f64, len: f64, lo: f64, span: f64| {
            if len >= span {
                (lo, span)
            } else {
                (pos.clamp(lo, lo + span - len), len)
            }
        };
        let (x, w) = fit(self.x, self.w, bounds.x, bounds.w);
        let (y, h) = fit(self.y, self.h, bounds.y, bounds.h);
        Self::new(x, y, w, h)
    }

    /// The pixels this rectangle covers in a frame of `frame` size whose
    /// top-left is the logical point `origin`, `scale` pixels per point.
    pub fn to_pixels(self, origin: (f64, f64), scale: f64, frame: (u32, u32)) -> PxRect {
        let x0 = ((self.x - origin.0) * scale).round().max(0.0) as u32;
        let y0 = ((self.y - origin.1) * scale).round().max(0.0) as u32;
        let x1 = (((self.x + self.w - origin.0) * scale).round().max(0.0) as u32).min(frame.0);
        let y1 = (((self.y + self.h - origin.1) * scale).round().max(0.0) as u32).min(frame.1);
        let x0 = x0.min(frame.0.saturating_sub(1));
        let y0 = y0.min(frame.1.saturating_sub(1));
        PxRect {
            x: x0,
            y: y0,
            w: x1.saturating_sub(x0).max(1),
            h: y1.saturating_sub(y0).max(1),
        }
    }
}

/// A rectangle in frame pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Where the region came from, for logs and tests.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionSource {
    /// The accessibility element under the pointer, padded.
    Element,
    /// The default box around the pointer.
    Pointer,
    /// A table row under the pointer, with its header and first column
    /// ([`table::table_region`]).
    Table,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub source: RegionSource,
    /// The pointed row of a [`RegionSource::Table`] region, to be marked.
    pub mark: Option<Rect>,
}

/// Picks the region to capture on `display` (logical points).
///
/// `elements` are the accessibility frames under the pointer, innermost
/// first (the hit element, then its parents). The first one that is big
/// enough to carry context and small enough to still be "the thing pointed
/// at" wins, padded by [`ELEMENT_PADDING`]. Otherwise a [`DEFAULT_BOX`]
/// around the pointer. The result is shifted inside the display, not shrunk.
#[cfg(any(target_os = "macos", test))]
pub fn choose_region(display: Rect, pointer: (f64, f64), elements: &[Rect]) -> Region {
    let max_area = display.area() * MAX_ELEMENT_FRACTION;
    for element in elements {
        if element.w <= 0.0 || element.h <= 0.0 || !element.contains(pointer) {
            continue;
        }
        let padded = element.padded(ELEMENT_PADDING);
        if padded.area() > max_area {
            // Parents only get bigger.
            break;
        }
        if padded.w < MIN_ELEMENT.0 || padded.h < MIN_ELEMENT.1 {
            continue;
        }
        return Region {
            rect: padded.shifted_inside(display),
            source: RegionSource::Element,
            mark: None,
        };
    }
    Region {
        rect: Rect::centered(pointer, DEFAULT_BOX.0, DEFAULT_BOX.1).shifted_inside(display),
        source: RegionSource::Pointer,
        mark: None,
    }
}

/// [`choose_region`], except that a table row, which would otherwise get the
/// pointer box, gets the table's header and first column with it
/// ([`table::table_region`]). `scale` is frame pixels per point.
#[cfg(any(target_os = "macos", test))]
pub fn choose_region_in_frame(
    frame: &RgbImage,
    display: Rect,
    scale: f64,
    pointer: (f64, f64),
    elements: &[Rect],
) -> Region {
    let region = choose_region(display, pointer, elements);
    if region.source == RegionSource::Pointer {
        if let Some(hit) = table::table_region(frame, display, scale, pointer, elements) {
            return Region {
                rect: hit.region,
                source: RegionSource::Table,
                mark: Some(hit.row),
            };
        }
    }
    region
}

/// A vision model's image budget: the long edge and the 28 px visual tokens
/// it accepts before it resamples the image itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub long_edge: u32,
    pub tokens: u32,
}

impl Budget {
    /// Claude's standard tier.
    pub const STANDARD: Budget = Budget {
        long_edge: 1568,
        tokens: 1568,
    };
    /// Claude's high-resolution tier (opt-in, about 3x the tokens).
    pub const HIGH: Budget = Budget {
        long_edge: 2576,
        tokens: 4784,
    };

    pub fn tokens_for(w: u32, h: u32) -> u32 {
        w.div_ceil(VISUAL_TOKEN_PX) * h.div_ceil(VISUAL_TOKEN_PX)
    }

    pub fn fits(&self, w: u32, h: u32) -> bool {
        w.max(h) <= self.long_edge && Self::tokens_for(w, h) <= self.tokens
    }

    /// The largest size with the same aspect that fits; `(w, h)` itself when
    /// it already fits. Never larger than `(w, h)`.
    pub fn fit(&self, w: u32, h: u32) -> (u32, u32) {
        if self.fits(w, h) {
            return (w, h);
        }
        let (wf, hf) = (w as f64, h as f64);
        let by_edge = self.long_edge as f64 / wf.max(hf);
        let px = (self.tokens * VISUAL_TOKEN_PX * VISUAL_TOKEN_PX) as f64;
        let by_tokens = (px / (wf * hf)).sqrt();
        let mut s = by_edge.min(by_tokens).min(1.0);
        loop {
            // The epsilon keeps 3000 * (1568 / 3000) at 1568, not 1567.
            let nw = ((wf * s + 1e-9).floor() as u32).max(1);
            let nh = ((hf * s + 1e-9).floor() as u32).max(1);
            if self.fits(nw, nh) || (nw == 1 && nh == 1) {
                return (nw, nh);
            }
            s *= 0.995;
        }
    }
}

/// What one capture produced.
#[derive(Debug, Clone)]
pub struct Shot {
    /// The region, framed, at native pixels or downscaled to the budget.
    pub crop: RgbImage,
    /// The whole display, small, with the region outlined.
    pub context: Option<RgbImage>,
    /// Region area over display area, in 0..=1.
    pub region_fraction: f64,
}

/// Builds the shot from the region's pixels.
///
/// `frame` is the whole display when the platform captured it, and `region`
/// is where `crop` sits in it when known. Without a frame there is no
/// context image; without a known position the context image has no outline.
pub fn render(
    crop: &RgbImage,
    frame: Option<&RgbImage>,
    region: Option<PxRect>,
    budget: Budget,
) -> Shot {
    let inner = budget.fit(crop.width() + 2 * BORDER_PX, crop.height() + 2 * BORDER_PX);
    let inner = (
        inner.0.saturating_sub(2 * BORDER_PX).max(1),
        inner.1.saturating_sub(2 * BORDER_PX).max(1),
    );
    let sized = if inner == crop.dimensions() {
        crop.clone()
    } else {
        imageops::resize(crop, inner.0, inner.1, FilterType::Lanczos3)
    };
    let region_fraction = match frame {
        Some(f) if f.width() > 0 && f.height() > 0 => {
            let frame_area = f.width() as f64 * f.height() as f64;
            let crop_area = crop.width() as f64 * crop.height() as f64;
            (crop_area / frame_area).min(1.0)
        }
        _ => 1.0,
    };
    Shot {
        crop: add_border(&sized),
        context: frame.map(|f| context_thumbnail(f, region)),
        region_fraction,
    }
}

/// Crops `region` from `frame` and renders it.
#[cfg(any(target_os = "macos", test))]
pub fn render_region(frame: &RgbImage, region: PxRect, budget: Budget) -> Shot {
    let crop = imageops::crop_imm(frame, region.x, region.y, region.w, region.h).to_image();
    render(&crop, Some(frame), Some(region), budget)
}

/// Builds the shot from one display's `frame`, given the display's logical
/// bounds and the accessibility frames under the pointer (innermost first).
#[cfg(any(target_os = "macos", test))]
pub fn render_pointed(
    frame: &RgbImage,
    display: Rect,
    pointer: (f64, f64),
    elements: &[Rect],
    budget: Budget,
) -> Shot {
    let scale = if display.w > 0.0 {
        frame.width() as f64 / display.w
    } else {
        1.0
    };
    let region = choose_region_in_frame(frame, display, scale, pointer, elements);
    let px = region
        .rect
        .to_pixels((display.x, display.y), scale, frame.dimensions());
    eprintln!(
        "[vision] region {:?} {:.0}x{:.0} pt at ({:.0},{:.0}) scale={scale:.2} -> {}x{} px",
        region.source, region.rect.w, region.rect.h, region.rect.x, region.rect.y, px.w, px.h
    );
    let mut shot = render_region(frame, px, budget);
    if let Some(row) = region.mark {
        let row = row.to_pixels((display.x, display.y), scale, frame.dimensions());
        mark_row(&mut shot, frame.dimensions(), px, row);
    }
    if region.source == RegionSource::Table {
        // A table crop is wide because tables are, not because it already
        // shows the screen: the title, filters and caption around the table
        // still earn the context image, so it counts as just under the
        // share that would drop it.
        shot.region_fraction = shot.region_fraction.min(table::CONTEXT_SHARE);
    }
    shot
}

/// Marks the pointed `row` (frame pixels) of a table crop, which spans many
/// rows, so the agent can tell which one was meant: a [`ROW_MARK_PX`] box
/// inside the row on the crop, and a band over the row on the context image.
/// `region` is where the crop sits in the `frame_size` frame. The box lies on
/// the row's own edges, where rows keep their padding, not on its text.
#[cfg(any(target_os = "macos", test))]
fn mark_row(shot: &mut Shot, frame_size: (u32, u32), region: PxRect, row: PxRect) {
    let inner = (
        shot.crop.width().saturating_sub(2 * BORDER_PX).max(1) as f64,
        shot.crop.height().saturating_sub(2 * BORDER_PX).max(1) as f64,
    );
    let (sx, sy) = (inner.0 / region.w as f64, inner.1 / region.h as f64);
    let to_crop = |v: u32, origin: u32, s: f64| {
        BORDER_PX as i64 + ((v as f64 - origin as f64) * s).round() as i64
    };
    let x0 = to_crop(row.x, region.x, sx).max(BORDER_PX as i64);
    let y0 = to_crop(row.y, region.y, sy);
    let x1 = to_crop(row.x + row.w, region.x, sx).min(BORDER_PX as i64 + inner.0 as i64);
    let y1 = to_crop(row.y + row.h, region.y, sy);
    draw_inner_outline(&mut shot.crop, x0, y0, x1, y1, ROW_MARK_PX);
    if let Some(ctx) = shot.context.as_mut() {
        let s = ctx.width() as f64 / frame_size.0.max(1) as f64;
        let to_ctx = |v: u32| (v as f64 * s).round() as i64;
        let (cx0, cy0) = (to_ctx(row.x), to_ctx(row.y));
        let (cx1, cy1) = (to_ctx(row.x + row.w), to_ctx(row.y + row.h).max(cy0 + 2));
        tint(ctx, cx0, cy0, cx1, cy1, ROW_BAND_ALPHA);
    }
}

/// Strokes `t` px just inside the box `[x0, x1) x [y0, y1)`, clipped to the image.
#[cfg(any(target_os = "macos", test))]
fn draw_inner_outline(img: &mut RgbImage, x0: i64, y0: i64, x1: i64, y1: i64, t: i64) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            if x < x0 + t || x >= x1 - t || y < y0 + t || y >= y1 - t {
                img.put_pixel(x as u32, y as u32, BORDER_COLOR);
            }
        }
    }
}

/// Blends [`BORDER_COLOR`] over the box `[x0, x1) x [y0, y1)` at `alpha`.
#[cfg(any(target_os = "macos", test))]
fn tint(img: &mut RgbImage, x0: i64, y0: i64, x1: i64, y1: i64, alpha: f64) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            let p = img.get_pixel_mut(x as u32, y as u32);
            for (c, b) in p.0.iter_mut().zip(BORDER_COLOR.0) {
                *c = (*c as f64 * (1.0 - alpha) + b as f64 * alpha).round() as u8;
            }
        }
    }
}

/// Adds a [`BORDER_PX`] frame around the image. The frame is new canvas, so
/// it never covers a captured pixel.
pub fn add_border(img: &RgbImage) -> RgbImage {
    let mut out = RgbImage::from_pixel(
        img.width() + 2 * BORDER_PX,
        img.height() + 2 * BORDER_PX,
        BORDER_COLOR,
    );
    imageops::replace(&mut out, img, BORDER_PX as i64, BORDER_PX as i64);
    out
}

/// The frame shrunk to [`CONTEXT_LONG_EDGE`], with `region` outlined just
/// outside its edge so the outline does not hide the region itself.
pub fn context_thumbnail(frame: &RgbImage, region: Option<PxRect>) -> RgbImage {
    let (w, h) = frame.dimensions();
    let s = (CONTEXT_LONG_EDGE as f64 / w.max(h) as f64).min(1.0);
    let (tw, th) = (
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
    );
    // Area averaging: several times faster than Lanczos3 over a whole
    // display, and the thumbnail's job is layout, not legible text.
    let mut thumb = if (tw, th) == (w, h) {
        frame.clone()
    } else {
        imageops::thumbnail(frame, tw, th)
    };
    if let Some(r) = region {
        let x0 = (r.x as f64 * s).floor() as i64;
        let y0 = (r.y as f64 * s).floor() as i64;
        let x1 = ((r.x + r.w) as f64 * s).ceil() as i64;
        let y1 = ((r.y + r.h) as f64 * s).ceil() as i64;
        draw_outline(&mut thumb, x0, y0, x1, y1, BORDER_PX as i64);
    }
    thumb
}

/// Strokes `t` px just outside the box `[x0, x1) x [y0, y1)`, clipped to the image.
fn draw_outline(img: &mut RgbImage, x0: i64, y0: i64, x1: i64, y1: i64, t: i64) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    for y in (y0 - t).max(0)..(y1 + t).min(h) {
        for x in (x0 - t).max(0)..(x1 + t).min(w) {
            let inside = x >= x0 && x < x1 && y >= y0 && y < y1;
            if !inside {
                img.put_pixel(x as u32, y as u32, BORDER_COLOR);
            }
        }
    }
}

/// Writes `img` as an 8-bit RGB PNG: lossless, no alpha, no metadata.
pub fn encode_png(img: &RgbImage, path: &Path) -> Result<()> {
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let encoder = PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        CompressionType::Default,
        PngFilter::Adaptive,
    );
    encoder
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .with_context(|| format!("encoding {}", path.display()))?;
    Ok(())
}

/// Finds where `needle` sits in `haystack`, pixel for pixel.
///
/// Used on Linux, where the portal returns the region the user selected
/// without saying where it was: matching it against a full-display capture
/// taken just before places the outline on the context image. Allows a few
/// changed pixels (a blinking caret) but not a different place. `None` when
/// the needle is not there or is a flat colour that fits anywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only Linux selects areas
pub fn locate(haystack: &RgbImage, needle: &RgbImage) -> Option<PxRect> {
    const MAX_EDGE_PROBES: usize = 512;
    const GRID: u32 = 24;
    let (hw, hh) = haystack.dimensions();
    let (nw, nh) = needle.dimensions();
    if nw == 0 || nh == 0 || nw > hw || nh > hh {
        return None;
    }
    // Pixels on both sides of a colour change: a shifted position always
    // mismatches some of them, which flat areas cannot tell.
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for y in 0..nh {
        for x in 0..nw {
            let p = needle.get_pixel(x, y);
            if x + 1 < nw && needle.get_pixel(x + 1, y) != p {
                edges.extend([(x, y), (x + 1, y)]);
            }
            if y + 1 < nh && needle.get_pixel(x, y + 1) != p {
                edges.extend([(x, y), (x, y + 1)]);
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    let stride = edges.len().div_ceil(MAX_EDGE_PROBES);
    let mut probes: Vec<(u32, u32)> = edges.into_iter().step_by(stride).collect();
    // Plus an even grid, so a change anywhere in the region counts.
    for j in 0..GRID {
        for i in 0..GRID {
            probes.push((i * (nw - 1) / (GRID - 1), j * (nh - 1) / (GRID - 1)));
        }
    }
    probes.sort_unstable();
    probes.dedup();
    // Rare colours first: most wrong positions fail on the first probe.
    let mut counts = std::collections::HashMap::new();
    for &(px, py) in &probes {
        *counts.entry(needle.get_pixel(px, py).0).or_insert(0u32) += 1;
    }
    probes.sort_by_key(|&(px, py)| counts[&needle.get_pixel(px, py).0]);

    // Screenshots are lossless, so unchanged pixels match exactly; the
    // slack only absorbs rounding in a compositor's colour pipeline.
    let diff = |a: &Rgb<u8>, b: &Rgb<u8>| -> u32 {
        a.0.iter()
            .zip(b.0.iter())
            .map(|(x, y)| x.abs_diff(*y) as u32)
            .sum()
    };
    let allowed_misses = probes.len() / 20; // 95% must match
                                            // Fewest misses wins, then the smallest total difference.
    let mut best: Option<(usize, u64, u32, u32)> = None;
    let mut best_count = 0usize;
    for y in 0..=(hh - nh) {
        for x in 0..=(hw - nw) {
            let limit = best.map_or(allowed_misses, |(m, ..)| m);
            let mut misses = 0usize;
            let mut total = 0u64;
            for &(px, py) in &probes {
                let d = diff(haystack.get_pixel(x + px, y + py), needle.get_pixel(px, py));
                total += d as u64;
                if d > 6 {
                    misses += 1;
                    if misses > limit {
                        break;
                    }
                }
            }
            if misses <= limit {
                let score = (misses, total);
                match best.map(|(m, t, ..)| (m, t).cmp(&score)) {
                    None | Some(std::cmp::Ordering::Greater) => {
                        best = Some((misses, total, x, y));
                        best_count = 1;
                    }
                    Some(std::cmp::Ordering::Equal) => {
                        best_count += 1;
                        if misses == 0 && total == 0 {
                            return None;
                        }
                    }
                    Some(std::cmp::Ordering::Less) => {}
                }
            }
        }
    }
    if best_count == 1 {
        best.map(|(.., x, y)| PxRect { x, y, w: nw, h: nh })
    } else {
        None
    }
}

/// When to send the full-screen context image next to the crop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContextPolicy {
    /// When the crop is under [`CONTEXT_FRACTION`] of the display, or the
    /// dictation refers to what is around it.
    #[default]
    Auto,
    Always,
    Never,
}

/// Whether to send the context image for a crop covering `region_fraction`
/// of its display, given what the user said.
pub fn wants_context(policy: ContextPolicy, region_fraction: f64, transcript: &str) -> bool {
    match policy {
        ContextPolicy::Always => true,
        ContextPolicy::Never => false,
        ContextPolicy::Auto => {
            region_fraction < CONTEXT_FRACTION || refers_to_surroundings(transcript)
        }
    }
}

/// Whether the dictation talks about the screen, page or app around the
/// pointed region rather than the region alone.
pub fn refers_to_surroundings(transcript: &str) -> bool {
    const CUES: &[&str] = &[
        "screen",
        "this page",
        "the page",
        "this app",
        "the app",
        "this window",
        "the window",
        "this tab",
        "sidebar",
        "layout",
        "around it",
        "around this",
        "surrounding",
        "next to",
        "whole",
        "entire",
        "rest of",
        "where is",
        "where's",
        "which app",
        "which page",
        "what app",
        "what page",
    ];
    let lower = transcript.to_lowercase();
    CUES.iter().any(|cue| contains_phrase(&lower, cue))
}

/// `phrase` occurs in `text` on word boundaries.
fn contains_phrase(text: &str, phrase: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '\'';
    text.match_indices(phrase).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + phrase.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// A deterministic, non-repeating test "screen": every pixel's colour is a
/// fixed hash of its position, so every crop is unique and checkable.
#[cfg(test)]
pub(crate) fn test_frame(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        let v =
            (x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77)).wrapping_mul(0xC2B2_AE3D);
        Rgb([(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1512.0,
        h: 982.0,
    };

    use super::test_frame as fixture_frame;

    #[test]
    fn pointer_box_is_640x400_centered_on_the_pointer() {
        let r = choose_region(DISPLAY, (700.0, 500.0), &[]);
        assert_eq!(r.source, RegionSource::Pointer);
        assert_eq!(r.rect, Rect::new(380.0, 300.0, 640.0, 400.0));
    }

    #[test]
    fn pointer_box_shifts_inside_at_edges_instead_of_shrinking() {
        let r = choose_region(DISPLAY, (5.0, 975.0), &[]);
        assert_eq!(r.rect, Rect::new(0.0, 582.0, 640.0, 400.0));
        let r = choose_region(DISPLAY, (1510.0, 2.0), &[]);
        assert_eq!(r.rect, Rect::new(872.0, 0.0, 640.0, 400.0));
        // A secondary display left of the main one has negative x.
        let left = Rect::new(-1920.0, 0.0, 1920.0, 1080.0);
        let r = choose_region(left, (-1915.0, 540.0), &[]);
        assert_eq!(r.rect, Rect::new(-1920.0, 340.0, 640.0, 400.0));
    }

    #[test]
    fn pointer_box_on_a_tiny_display_is_cut_to_it() {
        let tiny = Rect::new(0.0, 0.0, 500.0, 300.0);
        let r = choose_region(tiny, (250.0, 150.0), &[]);
        assert_eq!(r.rect, tiny);
    }

    #[test]
    fn element_under_the_pointer_wins_with_padding() {
        let dialog = Rect::new(456.0, 317.0, 600.0, 348.0);
        let r = choose_region(DISPLAY, (756.0, 491.0), &[dialog]);
        assert_eq!(r.source, RegionSource::Element);
        assert_eq!(r.rect, Rect::new(440.0, 301.0, 632.0, 380.0));
    }

    #[test]
    fn tiny_elements_defer_to_their_parent() {
        let word = Rect::new(700.0, 480.0, 40.0, 16.0);
        let row = Rect::new(500.0, 476.0, 520.0, 24.0); // still too short
        let panel = Rect::new(456.0, 317.0, 600.0, 348.0);
        let r = choose_region(DISPLAY, (710.0, 488.0), &[word, row, panel]);
        assert_eq!(r.source, RegionSource::Element);
        assert_eq!(r.rect, panel.padded(ELEMENT_PADDING));
    }

    #[test]
    fn window_sized_elements_fall_back_to_the_pointer_box() {
        let word = Rect::new(700.0, 480.0, 40.0, 16.0);
        let editor = Rect::new(250.0, 40.0, 1260.0, 900.0);
        let window = DISPLAY;
        let r = choose_region(DISPLAY, (710.0, 488.0), &[word, editor, window]);
        assert_eq!(r.source, RegionSource::Pointer);
        assert_eq!(r.rect, Rect::new(390.0, 288.0, 640.0, 400.0));
    }

    #[test]
    fn elements_not_under_the_pointer_are_ignored() {
        let elsewhere = Rect::new(0.0, 0.0, 400.0, 300.0);
        let r = choose_region(DISPLAY, (1000.0, 700.0), &[elsewhere]);
        assert_eq!(r.source, RegionSource::Pointer);
    }

    #[test]
    fn element_at_a_display_edge_is_shifted_inside() {
        let menu = Rect::new(1300.0, 0.0, 212.0, 300.0);
        let r = choose_region(DISPLAY, (1400.0, 100.0), &[menu]);
        assert_eq!(r.rect, Rect::new(1268.0, 0.0, 244.0, 332.0));
    }

    #[test]
    fn logical_rect_maps_to_pixels_per_display_scale() {
        let r = Rect::new(380.0, 300.0, 640.0, 400.0);
        assert_eq!(
            r.to_pixels((0.0, 0.0), 2.0, (3024, 1964)),
            PxRect {
                x: 760,
                y: 600,
                w: 1280,
                h: 800
            }
        );
        // Fractional scale, display with a negative origin.
        let r = Rect::new(-1500.0, 100.0, 640.0, 400.0);
        assert_eq!(
            r.to_pixels((-1920.0, 0.0), 1.5, (2880, 1620)),
            PxRect {
                x: 630,
                y: 150,
                w: 960,
                h: 600
            }
        );
        // Never outside the frame.
        let r = Rect::new(1400.0, 900.0, 640.0, 400.0);
        let px = r.to_pixels((0.0, 0.0), 2.0, (3024, 1964));
        assert_eq!(px.x + px.w, 3024);
        assert_eq!(px.y + px.h, 1964);
    }

    #[test]
    fn budget_tokens_follow_the_28px_grid() {
        assert_eq!(Budget::tokens_for(1280, 800), 46 * 29);
        assert_eq!(Budget::tokens_for(28, 28), 1);
        assert_eq!(Budget::tokens_for(29, 28), 2);
    }

    #[test]
    fn budget_keeps_crops_that_fit_and_shrinks_the_rest() {
        let b = Budget::STANDARD;
        // The default 640x400 box at 2x stays native.
        assert_eq!(b.fit(1286, 806), (1286, 806));
        // A full 3024x1964 display has to shrink to about 0.45.
        let (w, h) = b.fit(3024, 1964);
        assert!(b.fits(w, h));
        assert!((1300..=1400).contains(&w), "{w}x{h}");
        assert!((w as f64 / h as f64 - 3024.0 / 1964.0).abs() < 0.01);
        // The long-edge limit binds for a wide strip.
        assert_eq!(b.fit(3000, 100).0, 1568);
        // The high-res tier keeps more pixels.
        let (hw, _) = Budget::HIGH.fit(3024, 1964);
        assert!(hw > w);
        assert!(Budget::HIGH.fits(hw, Budget::HIGH.fit(3024, 1964).1));
    }

    #[test]
    fn native_crop_is_kept_pixel_for_pixel_inside_the_border() {
        let frame = fixture_frame(3024, 1964);
        let region = PxRect {
            x: 760,
            y: 600,
            w: 1280,
            h: 800,
        };
        let shot = render_region(&frame, region, Budget::STANDARD);
        assert_eq!(shot.crop.dimensions(), (1280 + 6, 800 + 6));
        for (x, y) in [(0, 0), (1279, 0), (0, 799), (1279, 799), (640, 400)] {
            assert_eq!(
                shot.crop.get_pixel(x + BORDER_PX, y + BORDER_PX),
                frame.get_pixel(region.x + x, region.y + y),
                "pixel {x},{y} must be untouched"
            );
        }
        // The border is only on the added canvas.
        assert_eq!(*shot.crop.get_pixel(0, 0), BORDER_COLOR);
        assert_eq!(*shot.crop.get_pixel(2, 400), BORDER_COLOR);
        assert_eq!(*shot.crop.get_pixel(1285, 805), BORDER_COLOR);
        assert!(Budget::STANDARD.fits(1286, 806));
    }

    #[test]
    fn border_never_covers_a_captured_pixel() {
        let crop = RgbImage::from_pixel(50, 20, Rgb([1, 2, 3]));
        let framed = add_border(&crop);
        assert_eq!(framed.dimensions(), (56, 26));
        let interior = imageops::crop_imm(&framed, 3, 3, 50, 20).to_image();
        assert_eq!(interior, crop);
        let border_pixels = framed.pixels().filter(|p| **p == BORDER_COLOR).count();
        assert_eq!(border_pixels, 56 * 26 - 50 * 20);
    }

    #[test]
    fn oversized_crop_is_downscaled_to_the_budget_never_upscaled() {
        let frame = fixture_frame(3024, 1964);
        let whole = PxRect {
            x: 0,
            y: 0,
            w: 3024,
            h: 1964,
        };
        let shot = render_region(&frame, whole, Budget::STANDARD);
        let (w, h) = shot.crop.dimensions();
        assert!(Budget::STANDARD.fits(w, h), "{w}x{h}");
        assert_eq!(shot.region_fraction, 1.0);

        let small = PxRect {
            x: 10,
            y: 10,
            w: 200,
            h: 100,
        };
        let shot = render_region(&frame, small, Budget::STANDARD);
        assert_eq!(shot.crop.dimensions(), (206, 106));
    }

    #[test]
    fn high_tier_keeps_a_large_crop_native() {
        let frame = fixture_frame(3024, 1964);
        let region = PxRect {
            x: 0,
            y: 0,
            w: 2400,
            h: 1400,
        };
        let standard = render_region(&frame, region, Budget::STANDARD);
        assert!(standard.crop.width() < 2400);
        let region = PxRect {
            x: 0,
            y: 0,
            w: 2000,
            h: 1200,
        };
        let high = render_region(&frame, region, Budget::HIGH);
        assert_eq!(high.crop.dimensions(), (2006, 1206));
    }

    #[test]
    fn context_image_is_small_and_outlines_the_region() {
        let frame = RgbImage::from_pixel(3024, 1964, Rgb([255, 255, 255]));
        let region = PxRect {
            x: 760,
            y: 600,
            w: 1280,
            h: 800,
        };
        let shot = render_region(&frame, region, Budget::STANDARD);
        let ctx = shot.context.expect("context image");
        assert_eq!(ctx.width(), CONTEXT_LONG_EDGE);
        assert_eq!(ctx.height(), (1964.0f64 * 768.0 / 3024.0).round() as u32);
        let s = 768.0 / 3024.0;
        let (x0, y0) = ((760.0 * s) as u32, (600.0 * s) as u32);
        let (x1, y1) = (((760.0 + 1280.0) * s) as u32, ((600.0 + 800.0) * s) as u32);
        // Outline just outside the region, region interior untouched.
        assert_eq!(*ctx.get_pixel(x0 - 1, (y0 + y1) / 2), BORDER_COLOR);
        assert_eq!(*ctx.get_pixel((x0 + x1) / 2, y1 + 1), BORDER_COLOR);
        assert_eq!(
            *ctx.get_pixel((x0 + x1) / 2, (y0 + y1) / 2),
            Rgb([255, 255, 255])
        );
        assert_eq!(*ctx.get_pixel(x0 + 2, y0 + 2), Rgb([255, 255, 255]));
        let fraction = (1280.0 * 800.0) / (3024.0 * 1964.0);
        assert!((shot.region_fraction - fraction).abs() < 1e-9);
    }

    #[test]
    fn crop_without_a_frame_has_no_context() {
        let crop = fixture_frame(300, 200);
        let shot = render(&crop, None, None, Budget::STANDARD);
        assert!(shot.context.is_none());
        assert_eq!(shot.region_fraction, 1.0);
    }

    #[test]
    fn png_is_lossless_rgb_without_metadata_and_private_mode() {
        let dir = std::env::temp_dir().join(format!("bolo_shot_png_{}", std::process::id()));
        let path = dir.join("nested").join("crop.png");
        let img = fixture_frame(97, 41);
        encode_png(&img, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // IHDR colour type 2 = truecolour RGB, bit depth 8.
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(bytes[24], 8);
        assert_eq!(bytes[25], 2);
        for chunk in [&b"tEXt"[..], b"iTXt", b"zTXt", b"eXIf", b"iCCP"] {
            assert!(!bytes.windows(4).any(|w| w == chunk));
        }
        let back = image::open(&path).unwrap().to_rgb8();
        assert_eq!(back, img);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn locate_finds_the_selected_region_in_the_frame() {
        let frame = fixture_frame(1920, 1080);
        let region = PxRect {
            x: 701,
            y: 333,
            w: 420,
            h: 260,
        };
        let mut crop =
            imageops::crop_imm(&frame, region.x, region.y, region.w, region.h).to_image();
        assert_eq!(locate(&frame, &crop), Some(region));
        // A blinking caret drawn into the crop still matches.
        for y in 100..118 {
            crop.put_pixel(200, y, Rgb([0, 0, 0]));
            crop.put_pixel(201, y, Rgb([0, 0, 0]));
        }
        assert_eq!(locate(&frame, &crop), Some(region));
    }

    #[test]
    fn locate_rejects_repeated_identical_regions() {
        let needle = fixture_frame(40, 30);
        let mut frame = RgbImage::from_pixel(130, 50, Rgb([11, 19, 27]));
        imageops::replace(&mut frame, &needle, 10, 10);
        imageops::replace(&mut frame, &needle, 75, 10);
        assert_eq!(locate(&frame, &needle), None);
    }

    #[test]
    fn locate_on_a_flat_background_with_one_feature() {
        let mut frame = RgbImage::from_pixel(800, 600, Rgb([30, 30, 30]));
        for y in 300..320 {
            for x in 500..560 {
                frame.put_pixel(x, y, Rgb([200, 80, 10]));
            }
        }
        let region = PxRect {
            x: 450,
            y: 250,
            w: 200,
            h: 120,
        };
        let crop = imageops::crop_imm(&frame, region.x, region.y, region.w, region.h).to_image();
        assert_eq!(locate(&frame, &crop), Some(region));
    }

    #[test]
    fn locate_rejects_content_that_is_not_there() {
        let frame = fixture_frame(640, 400);
        let other = RgbImage::from_fn(100, 80, |x, y| {
            Rgb([(x * 7 % 256) as u8, 255, (y * 3 % 256) as u8])
        });
        assert_eq!(locate(&frame, &other), None);
        let too_big = fixture_frame(700, 400);
        assert_eq!(locate(&frame, &too_big), None);
    }

    #[test]
    fn context_policy_auto_uses_size_and_wording() {
        use ContextPolicy::*;
        assert!(wants_context(Auto, 0.2, "fix this"));
        assert!(!wants_context(Auto, 0.5, "fix this"));
        assert!(wants_context(
            Auto,
            0.5,
            "Why is the sidebar empty on this page?"
        ));
        assert!(wants_context(Always, 0.9, ""));
        assert!(!wants_context(Never, 0.01, "the whole screen"));
    }

    #[test]
    fn surroundings_cues_match_whole_words_only() {
        assert!(refers_to_surroundings("what's wrong with this screen"));
        assert!(refers_to_surroundings("Where is this setting"));
        assert!(refers_to_surroundings("look at the whole thing"));
        assert!(!refers_to_surroundings("rename this variable"));
        assert!(!refers_to_surroundings("the screenshot test fails")); // not "screen"
        assert!(!refers_to_surroundings("wholesale pricing"));
        assert!(!refers_to_surroundings("the application crashed")); // not "the app"
    }

    #[test]
    fn retina_pointer_box_is_cropped_at_native_pixels() {
        let display = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let f = fixture_frame(3024, 1964);
        let shot = render_pointed(&f, display, (539.0, 219.0), &[], Budget::STANDARD);
        // 640x400 pt at 2x, plus the frame.
        assert_eq!(shot.crop.dimensions(), (1286, 806));
        // Box origin (219, 19) pt is (438, 38) px.
        assert_eq!(*shot.crop.get_pixel(3, 3), *f.get_pixel(438, 38));
    }

    #[test]
    fn secondary_display_uses_its_own_origin_and_scale() {
        // A 1x display left of the main one.
        let display = Rect::new(-1920.0, 0.0, 1920.0, 1080.0);
        let f = fixture_frame(1920, 1080);
        let shot = render_pointed(&f, display, (-960.0, 540.0), &[], Budget::STANDARD);
        assert_eq!(shot.crop.dimensions(), (646, 406));
        assert_eq!(*shot.crop.get_pixel(3, 3), *f.get_pixel(640, 340));
    }

    #[test]
    fn element_frame_drives_the_crop() {
        let display = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let f = fixture_frame(3024, 1964);
        let dialog = Rect::new(456.0, 317.0, 600.0, 348.0);
        let shot = render_pointed(&f, display, (756.0, 491.0), &[dialog], Budget::STANDARD);
        // (600 + 32) x (348 + 32) pt at 2x, plus the frame.
        assert_eq!(shot.crop.dimensions(), (1264 + 6, 760 + 6));
        assert_eq!(*shot.crop.get_pixel(3, 3), *f.get_pixel(880, 602));
    }
}
