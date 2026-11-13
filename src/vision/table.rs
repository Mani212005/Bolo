//! Pointed regions for table rows.
//!
//! A table row is wide and short, so [`super::shot::choose_region`] skips it
//! as too small to carry context, and its parent (the whole table) is too
//! big, which leaves the 640x400 pointer box. That box is centred on the
//! pointer and drops the table header and the first column, the cells that
//! say what the pointed row is.
//!
//! When the pointer box would be used and the pointed element is a short row
//! of a table, the region is the table's full width (first column included)
//! from the header down to the pointed row and one row after it instead.
//! Whether a parent is a table is read from the pixels, because the
//! accessibility frames carry no row/column structure: a table repeats one
//! row pitch and keeps blank gutters between its columns, which running text,
//! chat bubbles and flat panels do not.
//!
//! Everything here is pure and display-free so it is unit tested on fixtures.

use super::shot::{PxRect, Rect, CONTEXT_FRACTION, ELEMENT_PADDING, MIN_ELEMENT};
use image::RgbImage;

/// A row is at least this many times wider than tall.
pub const MIN_ROW_ASPECT: f64 = 6.0;
/// The table holds at least this many rows' height, so a lone wide element
/// inside a wrapper is not mistaken for a row of a table.
pub const MIN_TABLE_ROWS: f64 = 4.0;
/// A row spans at least this share of its table's width; a cell does not.
pub const MIN_ROW_WIDTH_SHARE: f64 = 0.6;
/// Rows after the pointed one that stay in the region, in row heights.
pub const TRAILING_ROWS: f64 = 1.0;
/// The share of the display a table crop is reported as covering at most,
/// just under [`CONTEXT_FRACTION`], so the context image is kept.
pub const CONTEXT_SHARE: f64 = CONTEXT_FRACTION - 0.01;
/// Frames of the same element can differ by a point between APIs.
const FRAME_SLACK: f64 = 2.0;
/// A pixel is ink when a channel differs this much from its scan line's
/// background.
const INK_DELTA: u8 = 40;
/// The pitch autocorrelation must reach this share of the profile variance.
const MIN_PITCH_CORRELATION: f64 = 0.35;
/// The pitch is searched within this share either side of the row height.
const PITCH_TOLERANCE: f64 = 0.08;
/// A column gutter is blank for at least this many points...
const MIN_GUTTER_PT: f64 = 8.0;
/// ...a table has at least this many of them between its columns...
const MIN_GUTTERS: usize = 2;
/// ...and a column is blank when at most this share of its pixels is ink.
const BLANK_COLUMN_SHARE: f64 = 0.01;

/// The region for the table row under `pointer`, or `None` when the pointer
/// is not on a table row. `elements` are the accessibility frames under the
/// pointer, innermost first; `scale` is frame pixels per point.
pub fn table_region(
    frame: &RgbImage,
    display: Rect,
    scale: f64,
    pointer: (f64, f64),
    elements: &[Rect],
) -> Option<Rect> {
    for (i, row) in elements.iter().enumerate() {
        if !is_short_row(row, pointer) {
            continue;
        }
        // The nearest ancestor that could be the row's table decides; a
        // bigger one would only add whatever surrounds the table.
        let Some(table) = elements[i + 1..].iter().find(|t| is_table_frame(t, row)) else {
            continue;
        };
        let Some(visible) = intersect(*table, display) else {
            continue;
        };
        let px = visible.to_pixels((display.x, display.y), scale, frame.dimensions());
        if !looks_like_table(frame, px, row.h * scale, scale) {
            continue;
        }
        return Some(expand(display, visible, row));
    }
    None
}

/// A wide, short element under the pointer that [`super::shot::choose_region`]
/// would skip as too small.
fn is_short_row(row: &Rect, pointer: (f64, f64)) -> bool {
    row.w > 0.0
        && row.h > 0.0
        && row.contains(pointer)
        && row.w >= MIN_ROW_ASPECT * row.h
        && row.h + 2.0 * ELEMENT_PADDING < MIN_ELEMENT.1
}

/// Whether `table` can be the table `row` belongs to: it holds the row, spans
/// several rows' height, and the row is nearly as wide as it.
fn is_table_frame(table: &Rect, row: &Rect) -> bool {
    table.x <= row.x + FRAME_SLACK
        && table.y <= row.y + FRAME_SLACK
        && table.x + table.w + FRAME_SLACK >= row.x + row.w
        && table.y + table.h + FRAME_SLACK >= row.y + row.h
        && table.h >= MIN_TABLE_ROWS * row.h
        && row.w >= MIN_ROW_WIDTH_SHARE * table.w
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Full table width from the header (the top of the table) to one row below
/// the pointed row, padded and shifted inside the display like any region.
fn expand(display: Rect, table: Rect, row: &Rect) -> Rect {
    let top = table.y - ELEMENT_PADDING;
    let bottom = (row.y + row.h + TRAILING_ROWS * row.h).min(table.y + table.h) + ELEMENT_PADDING;
    let height = (bottom - top).max(MIN_ELEMENT.1);
    Rect::new(
        table.x - ELEMENT_PADDING,
        top,
        table.w + 2.0 * ELEMENT_PADDING,
        height,
    )
    .shifted_inside(display)
}

/// Whether the pixels of `area` look like a table whose rows are `pitch` px
/// apart at `scale` px per point: the ink repeats at that pitch and at least
/// [`MIN_GUTTERS`] blank gutters separate columns.
pub fn looks_like_table(frame: &RgbImage, area: PxRect, pitch: f64, scale: f64) -> bool {
    let (w, h) = (area.w as usize, area.h as usize);
    if pitch < 2.0 || (h as f64) < MIN_TABLE_ROWS * pitch || w < 2 {
        return false;
    }
    let ink = Ink::measure(frame, area);
    ink.repeats_at(pitch) && ink.gutters((MIN_GUTTER_PT * scale).ceil() as usize) >= MIN_GUTTERS
}

/// How much ink each scan line and each column of an area holds.
struct Ink {
    per_line: Vec<u32>,
    per_column: Vec<u32>,
}

impl Ink {
    fn measure(frame: &RgbImage, area: PxRect) -> Self {
        let (w, h) = (area.w as usize, area.h as usize);
        let mut per_line = vec![0u32; h];
        let mut per_column = vec![0u32; w];
        // The background of a scan line is its most common colour at 4 bits
        // a channel, so stripes, rules and tinted headers are not ink.
        let mut votes = vec![0u32; 4096];
        let mut first = vec![[0u8; 3]; 4096];
        let key = |p: &[u8; 3]| {
            ((p[0] >> 4) as usize) << 8 | ((p[1] >> 4) as usize) << 4 | (p[2] >> 4) as usize
        };
        for (y, line_ink) in per_line.iter_mut().enumerate() {
            votes.iter_mut().for_each(|v| *v = 0);
            for x in 0..w {
                let p = frame.get_pixel(area.x + x as u32, area.y + y as u32).0;
                let k = key(&p);
                if votes[k] == 0 {
                    first[k] = p;
                }
                votes[k] += 1;
            }
            // Lowest key wins a tie, so the result does not depend on hashing.
            let mode = (0..4096).max_by_key(|&k| (votes[k], 4096 - k)).unwrap_or(0);
            let bg = first[mode];
            for (x, column_ink) in per_column.iter_mut().enumerate() {
                let p = frame.get_pixel(area.x + x as u32, area.y + y as u32).0;
                if p.iter().zip(bg).any(|(a, b)| a.abs_diff(b) > INK_DELTA) {
                    *line_ink += 1;
                    *column_ink += 1;
                }
            }
        }
        Self {
            per_line,
            per_column,
        }
    }

    /// Whether the ink profile down the area repeats every `pitch` px.
    fn repeats_at(&self, pitch: f64) -> bool {
        let n = self.per_line.len();
        let width = self.per_column.len() as f64;
        let p: Vec<f64> = self.per_line.iter().map(|&c| c as f64 / width).collect();
        let mean = p.iter().sum::<f64>() / n as f64;
        let variance: f64 = p.iter().map(|v| (v - mean) * (v - mean)).sum();
        if variance <= f64::EPSILON {
            return false;
        }
        let lo = (pitch * (1.0 - PITCH_TOLERANCE)).floor().max(1.0) as usize;
        let hi = (pitch * (1.0 + PITCH_TOLERANCE)).ceil() as usize;
        (lo..=hi).filter(|&lag| lag < n).any(|lag| {
            let c: f64 = (0..n - lag)
                .map(|y| (p[y] - mean) * (p[y + lag] - mean))
                .sum();
            c / variance >= MIN_PITCH_CORRELATION
        })
    }

    /// Blank column runs at least `min_px` wide with ink on both sides.
    fn gutters(&self, min_px: usize) -> usize {
        let h = self.per_line.len() as f64;
        let blank = |x: usize| self.per_column[x] as f64 <= BLANK_COLUMN_SHARE * h;
        let Some(first) = (0..self.per_column.len()).find(|&x| !blank(x)) else {
            return 0;
        };
        let last = (first..self.per_column.len())
            .rev()
            .find(|&x| !blank(x))
            .unwrap_or(first);
        let (mut count, mut run) = (0, 0);
        for x in first..=last {
            if blank(x) {
                run += 1;
            } else {
                if run >= min_px {
                    count += 1;
                }
                run = 0;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::shot::{choose_region, choose_region_in_frame, RegionSource};
    use image::Rgb;

    const WHITE: Rgb<u8> = Rgb([255, 255, 255]);
    const INK: Rgb<u8> = Rgb([30, 30, 30]);
    const HEADER: Rgb<u8> = Rgb([235, 238, 242]);
    const STRIPE: Rgb<u8> = Rgb([246, 247, 249]);
    const RULE: Rgb<u8> = Rgb([225, 225, 225]);

    /// A 800x600 pt display.
    const DISPLAY: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 800.0,
        h: 600.0,
    };
    /// Column starts of the fixture table, in points from its left edge.
    const COLUMNS: [f64; 5] = [12.0, 130.0, 300.0, 420.0, 520.0];

    fn fill(img: &mut RgbImage, x: f64, y: f64, w: f64, h: f64, s: f64, c: Rgb<u8>) {
        let (x0, y0) = ((x * s).round() as u32, (y * s).round() as u32);
        let (x1, y1) = (((x + w) * s).round() as u32, ((y + h) * s).round() as u32);
        for py in y0..y1.min(img.height()) {
            for px in x0..x1.min(img.width()) {
                img.put_pixel(px, py, c);
            }
        }
    }

    /// A "word": 8 pt tall strokes 1.5 pt wide every 4 pt, like glyphs, so a
    /// line of text covers well under half of its scan line with ink.
    fn glyphs(img: &mut RgbImage, x: f64, y: f64, w: f64, s: f64) {
        let mut gx = 0.0;
        while gx + 1.5 <= w {
            fill(img, x + gx, y, 1.5, 8.0, s, INK);
            gx += 4.0;
        }
    }

    /// A deterministic "word" of ink: width and position depend on the cell.
    fn word(img: &mut RgbImage, x: f64, y: f64, r: usize, c: usize, s: f64) {
        let w = 28.0 + ((r * 7 + c * 13) % 6) as f64 * 9.0;
        glyphs(img, x, y + 6.0, w, s);
    }

    struct Table {
        x: f64,
        y: f64,
        w: f64,
        rows: usize,
        pitch: f64,
        zebra: bool,
    }

    impl Table {
        fn new(rows: usize) -> Self {
            Self {
                x: 100.0,
                y: 120.0,
                w: 600.0,
                rows,
                pitch: 24.0,
                zebra: false,
            }
        }

        /// The table frame, header row included.
        fn frame(&self) -> Rect {
            Rect::new(self.x, self.y, self.w, self.pitch * (self.rows + 1) as f64)
        }

        fn row(&self, i: usize) -> Rect {
            Rect::new(
                self.x,
                self.y + self.pitch * (i + 1) as f64,
                self.w,
                self.pitch,
            )
        }

        fn draw(&self, s: f64) -> RgbImage {
            let mut img =
                RgbImage::from_pixel((DISPLAY.w * s) as u32, (DISPLAY.h * s) as u32, WHITE);
            fill(&mut img, self.x, self.y, self.w, self.pitch, s, HEADER);
            for r in 0..=self.rows {
                let y = self.y + self.pitch * r as f64;
                if r > 0 {
                    if self.zebra && r % 2 == 0 {
                        fill(&mut img, self.x, y, self.w, self.pitch, s, STRIPE);
                    }
                    fill(&mut img, self.x, y, self.w, 1.0, s, RULE);
                }
                for (c, cx) in COLUMNS.iter().enumerate() {
                    word(&mut img, self.x + cx, y, r, c, s);
                }
            }
            img
        }
    }

    /// Running text: ragged lines at a fixed pitch, no column gutters.
    fn paragraph(s: f64) -> (RgbImage, Rect) {
        let mut img = RgbImage::from_pixel((DISPLAY.w * s) as u32, (DISPLAY.h * s) as u32, WHITE);
        for line in 0..12 {
            let w = 420.0 + ((line * 37) % 160) as f64;
            let mut x = 100.0;
            // Words of varying length with 5 pt spaces: never a wide gap.
            let mut k = 0;
            while x < 100.0 + w {
                let ww = 12.0 + ((line * 31 + k * 17 + line * k * 7) % 9) as f64 * 5.0;
                glyphs(&mut img, x, 120.0 + 20.0 * line as f64 + 6.0, ww, s);
                x += ww + 5.0;
                k += 1;
            }
        }
        (img, Rect::new(100.0, 120.0, 600.0, 240.0))
    }

    /// Chat bubbles: uneven heights, alternating sides, one wide blank lane.
    fn chat(s: f64) -> (RgbImage, Rect) {
        let mut img = RgbImage::from_pixel((DISPLAY.w * s) as u32, (DISPLAY.h * s) as u32, WHITE);
        let heights = [30.0, 52.0, 30.0, 74.0, 30.0, 52.0, 30.0, 30.0];
        let mut y = 120.0;
        for (i, h) in heights.iter().enumerate() {
            let (x, w) = if i % 2 == 0 {
                (110.0, 330.0)
            } else {
                (330.0, 360.0)
            };
            fill(&mut img, x, y, w, *h, s, Rgb([200, 220, 250]));
            glyphs(&mut img, x + 8.0, y + 8.0, w - 30.0, s);
            y += h + 10.0;
        }
        (img, Rect::new(100.0, 120.0, 600.0, y - 120.0))
    }

    fn pointer_on(row: Rect) -> (f64, f64) {
        (row.x + row.w / 2.0, row.y + row.h / 2.0)
    }

    #[test]
    fn table_fixture_is_recognised_at_both_scales() {
        for s in [1.0, 2.0] {
            let t = Table::new(10);
            let img = t.draw(s);
            let px = t.frame().to_pixels((0.0, 0.0), s, img.dimensions());
            assert!(looks_like_table(&img, px, t.pitch * s, s), "scale {s}");
        }
    }

    #[test]
    fn striped_table_without_rules_is_recognised() {
        let mut t = Table::new(10);
        t.zebra = true;
        let img = t.draw(2.0);
        let px = t.frame().to_pixels((0.0, 0.0), 2.0, img.dimensions());
        assert!(looks_like_table(&img, px, t.pitch * 2.0, 2.0));
    }

    #[test]
    fn running_text_is_not_a_table() {
        let (img, area) = paragraph(2.0);
        let px = area.to_pixels((0.0, 0.0), 2.0, img.dimensions());
        assert!(!looks_like_table(&img, px, 40.0, 2.0));
    }

    #[test]
    fn chat_bubbles_are_not_a_table() {
        let (img, area) = chat(2.0);
        let px = area.to_pixels((0.0, 0.0), 2.0, img.dimensions());
        assert!(!looks_like_table(&img, px, 60.0, 2.0));
    }

    #[test]
    fn flat_and_too_short_areas_are_not_tables() {
        let img = RgbImage::from_pixel(1600, 1200, WHITE);
        let whole = PxRect {
            x: 0,
            y: 0,
            w: 1600,
            h: 1200,
        };
        assert!(!looks_like_table(&img, whole, 48.0, 2.0));
        // A real table cut to two rows has too few rows to repeat.
        let t = Table::new(10);
        let img = t.draw(2.0);
        let two =
            Rect::new(t.x, t.y, t.w, 2.0 * t.pitch).to_pixels((0.0, 0.0), 2.0, img.dimensions());
        assert!(!looks_like_table(&img, two, 48.0, 2.0));
    }

    #[test]
    fn a_pitch_that_does_not_match_the_rows_is_rejected() {
        let t = Table::new(10);
        let img = t.draw(2.0);
        let px = t.frame().to_pixels((0.0, 0.0), 2.0, img.dimensions());
        // Half a row is not the repeat distance of this table.
        assert!(!looks_like_table(&img, px, t.pitch, 2.0));
    }

    #[test]
    fn row_region_keeps_header_first_column_and_the_row() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(15);
        let elements = [row, t.frame(), Rect::new(0.0, 24.0, 800.0, 576.0)];
        let r = table_region(&img, DISPLAY, 2.0, pointer_on(row), &elements).expect("table");
        // Full table width plus padding.
        assert_eq!(r.x, t.x - ELEMENT_PADDING);
        assert_eq!(r.w, t.w + 2.0 * ELEMENT_PADDING);
        // From the header, with padding, down to one row after the pointed one.
        assert_eq!(r.y, t.y - ELEMENT_PADDING);
        assert_eq!(r.y + r.h, row.y + 2.0 * row.h + ELEMENT_PADDING);
        assert!(r.contains((t.x, t.y)) && r.contains(pointer_on(row)));
    }

    #[test]
    fn region_stops_at_the_end_of_a_short_table() {
        let t = Table::new(8);
        let img = t.draw(2.0);
        let row = t.row(7); // the last row
        let r =
            table_region(&img, DISPLAY, 2.0, pointer_on(row), &[row, t.frame()]).expect("table");
        let table = t.frame();
        assert_eq!(r.y + r.h, table.y + table.h + ELEMENT_PADDING);
    }

    #[test]
    fn pointing_at_the_header_still_gets_a_usable_region() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let header = Rect::new(t.x, t.y, t.w, t.pitch);
        let r = table_region(&img, DISPLAY, 2.0, pointer_on(header), &[header, t.frame()])
            .expect("table");
        assert!(r.h >= MIN_ELEMENT.1);
        assert!(r.contains(pointer_on(header)));
    }

    #[test]
    fn cells_are_not_rows() {
        // The hit element is a cell; its parent row is not 4 rows tall, and
        // the table is much wider than the cell, so the row above is used.
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(5);
        let cell = Rect::new(row.x + COLUMNS[1], row.y, 160.0, row.h);
        let r = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(cell),
            &[cell, row, t.frame()],
        )
        .expect("table");
        assert_eq!(r.w, t.w + 2.0 * ELEMENT_PADDING);
    }

    #[test]
    fn region_is_shifted_inside_the_display() {
        let mut t = Table::new(20);
        t.x = 4.0;
        t.w = 792.0;
        let img = t.draw(2.0);
        let row = t.row(3);
        let r =
            table_region(&img, DISPLAY, 2.0, pointer_on(row), &[row, t.frame()]).expect("table");
        assert!(r.x >= 0.0 && r.x + r.w <= DISPLAY.w + 1e-9);
        assert!(r.y >= 0.0 && r.y + r.h <= DISPLAY.h + 1e-9);
    }

    #[test]
    fn a_table_scrolled_above_the_display_is_cut_to_what_is_visible() {
        let mut t = Table::new(40);
        t.y = -300.0;
        let img = t.draw(2.0);
        let row = t.row(20);
        let table = t.frame();
        let r = table_region(&img, DISPLAY, 2.0, pointer_on(row), &[row, table]);
        // Only the visible part of the table is inspected; it still repeats.
        let r = r.expect("visible rows form a table");
        assert!(r.y >= 0.0 && r.contains(pointer_on(row)));
    }

    #[test]
    fn non_rows_and_non_tables_are_left_alone() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(5);
        // Pointer outside the row.
        assert!(table_region(
            &img,
            DISPLAY,
            2.0,
            (row.x + 5.0, row.y - 40.0),
            &[row, t.frame()]
        )
        .is_none());
        // Not wide: a 150x24 box.
        let small = Rect::new(row.x, row.y, 150.0, row.h);
        assert!(table_region(&img, DISPLAY, 2.0, pointer_on(small), &[small, t.frame()]).is_none());
        // No parent.
        assert!(table_region(&img, DISPLAY, 2.0, pointer_on(row), &[row]).is_none());
        // A parent shorter than 4 rows.
        let short = Rect::new(t.x, row.y - 10.0, t.w, 3.0 * t.pitch);
        assert!(table_region(&img, DISPLAY, 2.0, pointer_on(row), &[row, short]).is_none());
        // Tall rows are ordinary elements: choose_region handles them.
        let tall = Rect::new(t.x, 300.0, t.w, 70.0);
        assert!(table_region(&img, DISPLAY, 2.0, pointer_on(tall), &[tall, t.frame()]).is_none());
    }

    #[test]
    fn text_lines_and_chat_messages_keep_the_pointer_box() {
        // Both sit in a page too big to be an element, so the pointer box
        // stays unless the page looks like a table.
        let page = Rect::new(60.0, 100.0, 680.0, 480.0);
        let (img, _) = paragraph(2.0);
        let line = Rect::new(100.0, 120.0 + 20.0 * 5.0, 600.0, 20.0);
        let r = choose_region_in_frame(&img, DISPLAY, 2.0, pointer_on(line), &[line, page]);
        assert_eq!(r.source, RegionSource::Pointer);

        let (img, _) = chat(2.0);
        let message = Rect::new(100.0, 160.0, 600.0, 30.0);
        let r = choose_region_in_frame(&img, DISPLAY, 2.0, pointer_on(message), &[message, page]);
        assert_eq!(r.source, RegionSource::Pointer);
    }

    #[test]
    fn table_rows_get_the_table_source_and_elements_keep_theirs() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(12);
        let window = Rect::new(0.0, 24.0, 800.0, 576.0);
        let r = choose_region_in_frame(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, t.frame(), window],
        );
        assert_eq!(r.source, RegionSource::Table);
        // A card that already qualifies as an element wins over the table.
        let card = Rect::new(t.x, row.y - 100.0, t.w, 240.0);
        let r =
            choose_region_in_frame(&img, DISPLAY, 2.0, pointer_on(row), &[row, card, t.frame()]);
        assert_eq!(r.source, RegionSource::Element);
        assert_eq!(
            r,
            choose_region(DISPLAY, pointer_on(row), &[row, card, t.frame()])
        );
    }

    #[test]
    fn the_table_crop_is_one_downscaled_band_that_fits_the_budget() {
        use crate::vision::shot::{render_pointed, Budget};
        let mut t = Table::new(20);
        t.x = 0.0;
        t.w = 790.0;
        let img = t.draw(2.0);
        let row = t.row(12);
        let shot = render_pointed(
            &img,
            DISPLAY,
            pointer_on(row),
            &[row, t.frame()],
            Budget::STANDARD,
        );
        assert!(Budget::STANDARD.fits(shot.crop.width(), shot.crop.height()));
        // Wider than tall: the band spans the table, not the pointer box.
        assert!(shot.crop.width() > shot.crop.height());
        // It covers over a third of the display, yet keeps the context image.
        let band = shot.crop.width() as f64 * shot.crop.height() as f64;
        assert!(band / (img.width() as f64 * img.height() as f64) > CONTEXT_FRACTION);
        assert!(shot.region_fraction < CONTEXT_FRACTION);
        assert!(crate::vision::shot::wants_context(
            crate::vision::shot::ContextPolicy::Auto,
            shot.region_fraction,
            "check this row"
        ));
    }
}
