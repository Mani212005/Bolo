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
//! When that band is so tall that the budget would shrink it below one output
//! pixel per point, the header is kept on top of a few rows around the pointed
//! one and the rows between them are left out. When the header is scrolled
//! out of view, only the rows around the pointed one are kept.
//! Whether a parent is a table is read from the pixels, because the
//! accessibility frames carry no row/column structure: a table repeats one
//! row pitch and keeps blank gutters between its columns, which running text,
//! chat bubbles and flat panels do not.
//!
//! Everything here is pure and display-free so it is unit tested on fixtures.

use super::shot::{
    Budget, PxRect, Rect, BORDER_PX, CONTEXT_FRACTION, ELEMENT_PADDING, MIN_ELEMENT,
};
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
/// Rows before the pointed one kept when the rows above them are left out.
pub const LEADING_ROWS: f64 = 2.0;
/// The pointed row keeps at least this many output pixels per point; a
/// taller band leaves out the rows between the header and the pointed row.
pub const MIN_ROW_SCALE: f64 = 1.0;
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

/// A table row under the pointer: the region to capture and the row in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableHit {
    pub region: Rect,
    /// The table header, kept above `region` when the rows between them are
    /// left out.
    pub header: Option<Rect>,
    /// The pointed row, in points, for the mark that says which row it is.
    pub row: Rect,
}

/// The region for the table row under `pointer`, or `None` when the pointer
/// is not on a table row. `elements` are the accessibility frames under the
/// pointer, innermost first; `scale` is frame pixels per point; `budget` is
/// the image budget the crop is sized for.
pub fn table_region(
    frame: &RgbImage,
    display: Rect,
    scale: f64,
    pointer: (f64, f64),
    elements: &[Rect],
    budget: Budget,
) -> Option<TableHit> {
    for (i, row) in elements.iter().enumerate() {
        if !is_short_row(row, pointer) {
            continue;
        }
        // The nearest ancestor that could be the row's table decides; a
        // bigger one would only add whatever surrounds the table.
        let ancestors = &elements[i + 1..];
        let Some(first) = ancestors.iter().position(|t| is_table_frame(t, row)) else {
            continue;
        };
        // The row's group (a body without the header) is no wider than the
        // table around it: climb to the widest frame with the same columns,
        // which is the one that holds the header.
        let mut t = first;
        while t + 1 < ancestors.len() && same_columns(&ancestors[t + 1], &ancestors[t]) {
            t += 1;
        }
        let table = ancestors[t];
        // The frames around the table (scroll area, web area, window) bound
        // what of it is on screen.
        let Some(visible) = ancestors[t + 1..]
            .iter()
            .filter(|outer| outer.contains(pointer))
            .try_fold(table, |v, outer| intersect(v, *outer))
            .and_then(|v| intersect(v, display))
        else {
            continue;
        };
        let px = visible.to_pixels((display.x, display.y), scale, frame.dimensions());
        if !looks_like_table(frame, px, row.h * scale, scale) {
            continue;
        }
        let around = band(
            display,
            visible,
            (row.y - LEADING_ROWS * row.h).max(visible.y),
            row,
        );
        if visible.y > table.y + FRAME_SLACK {
            // The header is scrolled out of view: the top of what is visible
            // is some other row, so only the rows around the pointed one say
            // anything.
            return Some(TableHit {
                region: around,
                header: None,
                row: *row,
            });
        }
        let whole = band(display, visible, visible.y, row);
        let body = ancestors[first];
        let header_h = if body.y > table.y + FRAME_SLACK {
            body.y - table.y
        } else {
            row.h
        };
        let header = Rect::new(whole.x, visible.y, whole.w, header_h);
        if stays_legible(whole, scale, budget) || around.y <= header.y + header.h {
            return Some(TableHit {
                region: whole,
                header: None,
                row: *row,
            });
        }
        return Some(TableHit {
            region: around,
            header: Some(header),
            row: *row,
        });
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

/// Whether `outer` holds `inner` and spans the same columns.
fn same_columns(outer: &Rect, inner: &Rect) -> bool {
    (outer.x - inner.x).abs() <= FRAME_SLACK
        && (outer.w - inner.w).abs() <= 2.0 * FRAME_SLACK
        && outer.y <= inner.y + FRAME_SLACK
        && outer.y + outer.h + FRAME_SLACK >= inner.y + inner.h
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Full table width from `top` to one row below the pointed row, padded at
/// the sides and bottom and shifted inside the display like any region. No
/// padding above: it would cut through the page title or the row above.
fn band(display: Rect, table: Rect, top: f64, row: &Rect) -> Rect {
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

/// Whether `region`, framed and fitted to `budget`, keeps at least
/// [`MIN_ROW_SCALE`] output pixels per point.
fn stays_legible(region: Rect, scale: f64, budget: Budget) -> bool {
    let frame = 2 * BORDER_PX;
    let w = (region.w * scale).round() as u32;
    let h = (region.h * scale).round() as u32;
    let fitted = budget.fit(w + frame, h + frame).0.saturating_sub(frame);
    fitted as f64 >= MIN_ROW_SCALE * region.w
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
    use crate::vision::shot::{
        choose_region, choose_region_in_frame, render_pointed, RegionSource, BORDER_COLOR,
        ELISION_PX,
    };
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
            self.draw_on(DISPLAY, s)
        }

        fn draw_on(&self, display: Rect, s: f64) -> RgbImage {
            let mut img =
                RgbImage::from_pixel((display.w * s) as u32, (display.h * s) as u32, WHITE);
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
        let r = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &elements,
            Budget::STANDARD,
        )
        .expect("table")
        .region;
        // Full table width plus padding.
        assert_eq!(r.x, t.x - ELEMENT_PADDING);
        assert_eq!(r.w, t.w + 2.0 * ELEMENT_PADDING);
        // From the header down to one row after the pointed one, padded below.
        assert_eq!(r.y, t.y);
        assert_eq!(r.y + r.h, row.y + 2.0 * row.h + ELEMENT_PADDING);
        assert!(r.contains((t.x, t.y)) && r.contains(pointer_on(row)));
    }

    #[test]
    fn region_stops_at_the_end_of_a_short_table() {
        let t = Table::new(8);
        let img = t.draw(2.0);
        let row = t.row(7); // the last row
        let r = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, t.frame()],
            Budget::STANDARD,
        )
        .expect("table")
        .region;
        let table = t.frame();
        assert_eq!(r.y + r.h, table.y + table.h + ELEMENT_PADDING);
    }

    #[test]
    fn pointing_at_the_header_still_gets_a_usable_region() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let header = Rect::new(t.x, t.y, t.w, t.pitch);
        let r = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(header),
            &[header, t.frame()],
            Budget::STANDARD,
        )
        .expect("table")
        .region;
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
            Budget::STANDARD,
        )
        .expect("table")
        .region;
        assert_eq!(r.w, t.w + 2.0 * ELEMENT_PADDING);
    }

    #[test]
    fn region_is_shifted_inside_the_display() {
        let mut t = Table::new(20);
        t.x = 4.0;
        t.w = 792.0;
        let img = t.draw(2.0);
        let row = t.row(3);
        let r = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, t.frame()],
            Budget::STANDARD,
        )
        .expect("table")
        .region;
        assert!(r.x >= 0.0 && r.x + r.w <= DISPLAY.w + 1e-9);
        assert!(r.y >= 0.0 && r.y + r.h <= DISPLAY.h + 1e-9);
    }

    #[test]
    fn a_table_scrolled_above_the_display_keeps_the_rows_around_the_pointer() {
        let mut t = Table::new(40);
        t.y = -300.0;
        let img = t.draw(2.0);
        let row = t.row(20);
        let hit = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, t.frame()],
            Budget::STANDARD,
        )
        .expect("visible rows form a table");
        // Not a band from the display top: the header is not on screen.
        assert_eq!(hit.header, None);
        assert_eq!(hit.region.y, row.y - LEADING_ROWS * row.h);
        assert_eq!(
            hit.region.y + hit.region.h,
            row.y + 2.0 * row.h + ELEMENT_PADDING
        );
        assert_eq!(hit.region.x, t.x - ELEMENT_PADDING);
        assert!(hit.region.contains(pointer_on(row)));
    }

    #[test]
    fn a_table_scrolled_under_its_scroll_area_is_cut_to_the_scroll_area() {
        let mut t = Table::new(40);
        t.y = 40.0;
        let img = t.draw(2.0);
        let scroll = Rect::new(0.0, 100.0, 800.0, 500.0);
        let window = Rect::new(0.0, 24.0, 800.0, 576.0);
        // Near the top of the scroll area: rows above it are hidden.
        let row = t.row(3);
        let hit = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, t.frame(), scroll, window],
            Budget::STANDARD,
        )
        .expect("table");
        assert_eq!(hit.header, None);
        assert_eq!(hit.region.y, scroll.y);
        assert!(hit.region.contains(pointer_on(row)));
        assert!(hit.region.h < 6.0 * row.h + ELEMENT_PADDING);
    }

    /// A 1920x1080 pt display at 1x: wide enough that a tall table band has
    /// to be shrunk to fit the budget.
    const WIDE: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };

    fn wide_table(rows: usize) -> Table {
        let mut t = Table::new(rows);
        t.x = 40.0;
        t.y = 40.0;
        t.w = 1500.0;
        t
    }

    /// Every ink pixel of `row` (frame pixels, 1x) is the same in `crop`,
    /// whose top-left inside its frame is the frame pixel `origin`.
    fn assert_row_glyphs_unchanged(
        frame: &RgbImage,
        crop: &RgbImage,
        origin: (i64, i64),
        row: Rect,
    ) {
        let b = crate::vision::shot::BORDER_PX as i64;
        let mut ink = 0;
        for y in row.y as u32..(row.y + row.h) as u32 {
            for x in row.x as u32..(row.x + row.w) as u32 {
                if *frame.get_pixel(x, y) != INK {
                    continue;
                }
                ink += 1;
                let (cx, cy) = (x as i64 - origin.0 + b, y as i64 - origin.1 + b);
                assert_eq!(
                    *crop.get_pixel(cx as u32, cy as u32),
                    INK,
                    "glyph pixel ({x},{y}) of the pointed row was painted over"
                );
            }
        }
        assert!(ink > 0, "the row has text");
    }

    #[test]
    fn a_tall_band_keeps_the_header_over_the_rows_around_the_pointer_at_native_scale() {
        let t = wide_table(40);
        let img = t.draw_on(WIDE, 1.0);
        let row = t.row(35);
        let elements = [row, t.frame()];
        let region = choose_region_in_frame(
            &img,
            WIDE,
            1.0,
            pointer_on(row),
            &elements,
            Budget::STANDARD,
        );
        assert_eq!(region.source, RegionSource::Table);
        let header = region.header.expect("the header is kept");
        assert_eq!(header.y, t.y);
        assert_eq!(header.h, t.pitch);
        assert_eq!(region.rect.y, row.y - LEADING_ROWS * row.h);
        assert_eq!(region.rect.w, header.w);

        let shot = render_pointed(&img, WIDE, pointer_on(row), &elements, Budget::STANDARD);
        let b = crate::vision::shot::BORDER_PX;
        // The pointed row is at no less than one output pixel per point.
        assert!((shot.crop.width() - 2 * b) as f64 >= region.rect.w);
        // The header sits on top, pixel for pixel.
        let (x0, hy) = (region.rect.x as u32, header.y as u32);
        for y in 0..header.h as u32 {
            for x in 0..region.rect.w as u32 {
                assert_eq!(
                    shot.crop.get_pixel(b + x, b + y),
                    img.get_pixel(x0 + x, hy + y)
                );
            }
        }
        // A dashed band of new pixels below it stands for the rows left out.
        let gap = b + header.h as u32;
        assert_eq!(*shot.crop.get_pixel(b, gap), BORDER_COLOR);
        assert_eq!(*shot.crop.get_pixel(b + 12, gap + ELISION_PX - 1), WHITE);
        // Then the rows around the pointed one, its text untouched.
        let origin = (
            region.rect.x as i64,
            region.rect.y as i64 - (header.h as u32 + ELISION_PX) as i64,
        );
        assert_row_glyphs_unchanged(&img, &shot.crop, origin, row);
        // The row is still marked, on its boundary lines.
        let top = (row.y as i64 - origin.1) as u32 + b;
        assert_eq!(*shot.crop.get_pixel(b + 200, top), BORDER_COLOR);
    }

    #[test]
    fn a_short_table_on_the_same_display_keeps_the_contiguous_band() {
        let t = wide_table(30);
        let img = t.draw_on(WIDE, 1.0);
        let row = t.row(6);
        let region = choose_region_in_frame(
            &img,
            WIDE,
            1.0,
            pointer_on(row),
            &[row, t.frame()],
            Budget::STANDARD,
        );
        assert_eq!(region.source, RegionSource::Table);
        assert_eq!(region.header, None);
        assert_eq!(region.rect.y, t.y);
        assert_eq!(
            region.rect.y + region.rect.h,
            row.y + 2.0 * row.h + ELEMENT_PADDING
        );
    }

    #[test]
    fn the_row_mark_leaves_the_glyphs_of_the_row_unchanged() {
        let t = Table::new(20);
        let img = t.draw(1.0);
        let row = t.row(8);
        let elements = [row, t.frame()];
        let region = choose_region_in_frame(
            &img,
            DISPLAY,
            1.0,
            pointer_on(row),
            &elements,
            Budget::STANDARD,
        );
        assert_eq!(region.header, None);
        let shot = render_pointed(&img, DISPLAY, pointer_on(row), &elements, Budget::STANDARD);
        let origin = (region.rect.x as i64, region.rect.y as i64);
        assert_row_glyphs_unchanged(&img, &shot.crop, origin, row);
        // The box is on the row's top boundary and in the margin beside it.
        let b = crate::vision::shot::BORDER_PX as i64;
        let top = (row.y as i64 - origin.1 + b) as u32;
        let left = (row.x as i64 - origin.0 + b) as u32;
        assert_eq!(*shot.crop.get_pixel(left + 100, top), BORDER_COLOR);
        assert_eq!(*shot.crop.get_pixel(left - 1, top + 12), BORDER_COLOR);
        assert_ne!(*shot.crop.get_pixel(left + 1, top + 12), BORDER_COLOR);
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
            &[row, t.frame()],
            Budget::STANDARD,
        )
        .is_none());
        // Not wide: a 150x24 box.
        let small = Rect::new(row.x, row.y, 150.0, row.h);
        assert!(table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(small),
            &[small, t.frame()],
            Budget::STANDARD
        )
        .is_none());
        // No parent.
        assert!(table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row],
            Budget::STANDARD
        )
        .is_none());
        // A parent shorter than 4 rows.
        let short = Rect::new(t.x, row.y - 10.0, t.w, 3.0 * t.pitch);
        assert!(table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, short],
            Budget::STANDARD
        )
        .is_none());
        // Tall rows are ordinary elements: choose_region handles them.
        let tall = Rect::new(t.x, 300.0, t.w, 70.0);
        assert!(table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(tall),
            &[tall, t.frame()],
            Budget::STANDARD
        )
        .is_none());
    }

    #[test]
    fn text_lines_and_chat_messages_keep_the_pointer_box() {
        // Both sit in a page too big to be an element, so the pointer box
        // stays unless the page looks like a table.
        let page = Rect::new(60.0, 100.0, 680.0, 480.0);
        let (img, _) = paragraph(2.0);
        let line = Rect::new(100.0, 120.0 + 20.0 * 5.0, 600.0, 20.0);
        let r = choose_region_in_frame(
            &img,
            DISPLAY,
            2.0,
            pointer_on(line),
            &[line, page],
            Budget::STANDARD,
        );
        assert_eq!(r.source, RegionSource::Pointer);

        let (img, _) = chat(2.0);
        let message = Rect::new(100.0, 160.0, 600.0, 30.0);
        let r = choose_region_in_frame(
            &img,
            DISPLAY,
            2.0,
            pointer_on(message),
            &[message, page],
            Budget::STANDARD,
        );
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
            Budget::STANDARD,
        );
        assert_eq!(r.source, RegionSource::Table);
        // A card that already qualifies as an element wins over the table.
        let card = Rect::new(t.x, row.y - 100.0, t.w, 240.0);
        let r = choose_region_in_frame(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, card, t.frame()],
            Budget::STANDARD,
        );
        assert_eq!(r.source, RegionSource::Element);
        assert_eq!(
            r,
            choose_region(DISPLAY, pointer_on(row), &[row, card, t.frame()])
        );
    }

    #[test]
    fn the_table_crop_is_one_downscaled_band_that_fits_the_budget() {
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

    /// The interior of a framed crop: everything inside the 3 px frame.
    fn interior_cyan(img: &RgbImage) -> Vec<(u32, u32)> {
        let b = crate::vision::shot::BORDER_PX;
        let cyan = crate::vision::shot::BORDER_COLOR;
        (b..img.height() - b)
            .flat_map(|y| (b..img.width() - b).map(move |x| (x, y)))
            .filter(|&(x, y)| *img.get_pixel(x, y) == cyan)
            .collect()
    }

    #[test]
    fn the_pointed_row_is_boxed_on_the_crop_and_banded_on_the_context_image() {
        let mut t = Table::new(20);
        t.x = 0.0;
        t.w = 790.0;
        let img = t.draw(2.0);
        let row = t.row(12);
        let elements = [row, t.frame()];
        let shot = render_pointed(&img, DISPLAY, pointer_on(row), &elements, Budget::STANDARD);
        let region = choose_region_in_frame(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &elements,
            Budget::STANDARD,
        );
        // Rows of the crop where the row sits: its top and bottom edges.
        let inner_h = shot.crop.height() as f64 - 6.0;
        let sy = inner_h / (region.rect.h * 2.0);
        let top = 3.0 + (row.y - region.rect.y) * 2.0 * sy;
        let bottom = top + row.h * 2.0 * sy;
        let cyan = interior_cyan(&shot.crop);
        assert!(!cyan.is_empty(), "the row is marked");
        for (_, y) in &cyan {
            let y = *y as f64;
            assert!(
                (y - top).abs() <= 3.0 || (y - bottom).abs() <= 3.0 || (top..bottom).contains(&y),
                "mark at y={y} is outside the pointed row {top}..{bottom}"
            );
        }
        // The text of the row stays readable: its middle is not painted over.
        let mid = ((top + bottom) / 2.0) as u32;
        let painted_mid = cyan
            .iter()
            .filter(|&&(x, y)| y == mid && x > 20 && x < 700)
            .count();
        assert_eq!(painted_mid, 0);
        // The context image has a band over the row and none on other rows.
        let ctx = shot.context.expect("context image");
        let s = ctx.width() as f64 / img.width() as f64;
        let in_row = |y: f64| y >= row.y * 2.0 * s && y < (row.y + row.h) * 2.0 * s;
        let tinted = |x: u32, y: u32| {
            let p = ctx.get_pixel(x, y);
            p.0[0] < 200 && p.0[2] > 200 && *p != BORDER_COLOR
        };
        let mid_x = (400.0 * 2.0 * s) as u32;
        let ry = ((row.y + row.h / 2.0) * 2.0 * s) as u32;
        assert!(tinted(mid_x, ry), "the pointed row is banded");
        let other = ((t.row(3).y + 6.0) * 2.0 * s) as u32;
        assert!(!in_row(other as f64) && !tinted(mid_x, other));
    }

    #[test]
    fn crops_that_are_not_table_rows_are_not_marked() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(12);
        // An element crop of the same frame carries only its frame.
        let card = Rect::new(t.x, row.y - 100.0, t.w, 240.0);
        let shot = render_pointed(
            &img,
            DISPLAY,
            pointer_on(row),
            &[row, card, t.frame()],
            Budget::STANDARD,
        );
        assert!(interior_cyan(&shot.crop).is_empty());
    }

    #[test]
    fn a_body_group_climbs_to_the_table_that_holds_the_header() {
        let t = Table::new(20);
        let img = t.draw(2.0);
        let row = t.row(10);
        // Body frame without the header row, then the table, then a wider page.
        let body = Rect::new(t.x, t.y + t.pitch, t.w, t.pitch * 20.0);
        let page = Rect::new(t.x - 20.0, t.y - 40.0, t.w + 40.0, 560.0);
        let hit = table_region(
            &img,
            DISPLAY,
            2.0,
            pointer_on(row),
            &[row, body, t.frame(), page],
            Budget::STANDARD,
        )
        .expect("table");
        assert_eq!(hit.region.y, t.y);
        assert_eq!(hit.region.x, t.x - ELEMENT_PADDING);
        assert_eq!(hit.row, row);
    }
}
