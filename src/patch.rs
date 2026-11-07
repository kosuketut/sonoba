//! What gets drawn over one paragraph: the translation in the original's colours, on a
//! rectangle that covers the original glyphs and nothing else.

use crate::ocr::Block;
use std::collections::HashMap;
use std::sync::Arc;

/// Overlay pixels of exactly this colour are see-through. COLORREF, as are `bg` and `fg`.
pub const KEY_COLOR: u32 = 0x00FF00FF;
const GLYPH_DISTANCE: i32 = 64; // how far from the background a pixel must be to count as text
const MARGIN: i32 = 2; // covers the antialiased fringe outside the glyphs
const GLYPH_STALE_PERCENT: usize = 30; // of the text's edges; compression noise moves a few
pub const WOBBLE_PERCENT: usize = 2; // edges that change without the text changing, around a thin outline
const EDGE_COLUMNS: usize = 2;
const EDGE_PX: usize = 2; // a text edge rises within this many pixels, most of a video's do not
const LETTER_FRINGE: usize = 2; // antialiasing around the letters goes with them
const SMOOTHING: usize = 4; // passes over a painted-out letter, so that no outline of it shows
const SAME_BG: i32 = 30;
const SAME_FG: i32 = 80; // thin text rarely reaches its true colour, so allow for that
const BOLD_STEM: usize = 8; // a stem wider than 1/8 of the line height, less one antialiased pixel
const MIN_BOLD_LINE: i32 = 16; // small text is all antialiasing, so weight cannot be told
const STATUS_BG: u32 = 0x00201C1C;
const STATUS_FG: u32 = 0x00F5F5F5;

/// The ink of one text line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyphs {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub bg: [i32; 3],
    pub fg: [i32; 3],
    pub stem: usize, // median width of a stretch of ink: most rows cross vertical stems
    pub icon: bool,  // an icon sat before the text and was left out of this rectangle
    pub picture: bool, // no common colour behind the text: a photo, a gradient, a video
}

impl Glyphs {
    pub fn bold(&self) -> bool {
        self.h >= MIN_BOLD_LINE && self.stem.saturating_sub(1) as i32 * BOLD_STEM as i32 > self.h
    }

    /// True when two lines sit on the same background in the same ink.
    pub fn same_style(&self, other: &Glyphs) -> bool {
        distance(self.bg, other.bg) <= SAME_BG && distance(self.fg, other.fg) <= SAME_FG
    }
}

/// The picture behind a patch with the letters painted out, as BGRA rows.
#[derive(Debug, PartialEq)]
pub struct Backdrop {
    pub w: i32,
    pub h: i32,
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub max_w: i32, // how far a single line may spread over empty background
    pub h: i32,     // 0 until laid out for a status message
    pub em: i32,    // the original's font size, which the translation tries to keep
    pub bg: u32,
    pub fg: u32,
    pub bold: bool,
    pub backdrop: Option<Arc<Backdrop>>, // shown instead of the flat `bg`
    pub text: String,
    pub source: String, // what OCR read
    pub px: i32,     // font size, set when laid out on the UI thread
    pub text_h: i32, // measured height, ditto
}

fn distance(a: [i32; 3], b: [i32; 3]) -> i32 {
    (0..3).map(|c| (a[c] - b[c]).abs()).sum()
}

fn pixel(bgra: &[u8], width: usize, x: usize, y: usize) -> [i32; 3] {
    let p = &bgra[(y * width + x) * 4..][..3];
    [p[0] as i32, p[1] as i32, p[2] as i32]
}

/// Finds the text line inside a detector box. The box is taller than the text and may reach
/// into the lines above and below or past a button's edge, so only the band of ink rows at
/// its middle counts, and the background comes from that middle too. Field borders and a
/// leading icon are left out, so that neither is painted over or mistaken for tall text.
/// `first_word` is what the recogniser read first, for telling a short word from an icon;
/// without it icons are not looked for.
pub fn glyphs(bgra: &[u8], width: usize, height: usize, (x, y, w, h): (i32, i32, i32, i32), first_word: Option<&str>) -> Option<Glyphs> {
    let x0 = x.clamp(0, width as i32) as usize;
    let y0 = y.clamp(0, height as i32) as usize;
    let x1 = (x + w).clamp(0, width as i32) as usize;
    let y1 = (y + h).clamp(0, height as i32) as usize;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }

    // The commonest colour is the background: glyphs are thin strokes on it.
    let quarter = (y1 - y0) / 4;
    let middle = y0 + quarter..y1 - quarter;
    let mut bins: HashMap<u16, [u64; 4]> = HashMap::new();
    for row in middle.clone() {
        for col in x0..x1 {
            let [b, g, r] = pixel(bgra, width, col, row);
            let bin = bins.entry(((b >> 3) << 10 | (g >> 3) << 5 | r >> 3) as u16).or_default();
            *bin = [bin[0] + 1, bin[1] + b as u64, bin[2] + g as u64, bin[3] + r as u64];
        }
    }
    let [n, b, g, r] = *bins.values().max_by_key(|bin| bin[0])?;
    let mut bg = [(b / n) as i32, (g / n) as i32, (r / n) as i32];
    // A picture has no common colour, so the letters of a thick subtitle can come first. The
    // box reaches past the text, so its two ends are the background then.
    let picture = n * 2 < (middle.len() * (x1 - x0)) as u64;
    if picture {
        let ends: Vec<usize> = (x0..x1).take(EDGE_COLUMNS).chain((x0..x1).rev().take(EDGE_COLUMNS)).collect();
        let around: Vec<[i32; 3]> = middle.clone().flat_map(|row| ends.iter().map(move |&col| (col, row))).map(|(col, row)| pixel(bgra, width, col, row)).collect();
        bg = [0, 1, 2].map(|c| around.iter().map(|p| p[c]).sum::<i32>() / around.len().max(1) as i32);
    }
    let is_ink = |col: usize, row: usize| distance(pixel(bgra, width, col, row), bg) > GLYPH_DISTANCE;

    // The box is much taller than any letter, so a column of ink in most rows is a rule.
    let rows = y1 - y0;
    let is_rule = |col: usize| !picture && (y0..y1).filter(|&row| is_ink(col, row)).count() * 10 >= rows * 7;
    let mut cols: Vec<usize> = (x0..x1).filter(|&col| !is_rule(col)).collect();

    let band = |cols: &[usize]| {
        let (mut found, mut best, mut run_top, mut blank) = (None, i32::MIN, None, 0);
        for row in y0..=y1 {
            // The curve of a rounded field leaves a pixel or two per row; letters leave more.
            if row < y1 && cols.iter().filter(|&&col| is_ink(col, row)).take(3).count() == 3 {
                run_top.get_or_insert(row);
                blank = 0;
                continue;
            }
            blank += 1; // one blank row can be the gap under the dot of an i
            if let Some(start) = run_top.filter(|_| blank > 1 || row == y1) {
                let end = row + 1 - blank;
                let in_middle = end.min(middle.end) as i32 - start.max(middle.start) as i32;
                if in_middle > best {
                    (found, best) = (Some((start, end)), in_middle);
                }
                run_top = None;
            }
        }
        found
    };
    let (mut top, mut bottom) = band(&cols)?;

    // An icon before the text: a blob about as wide as the line is tall, then a wide gap, and
    // not the width that the first word read from the box would have. A field's edge can
    // come before the icon, so look more than once.
    let first = first_word.unwrap_or("");
    let mut icon = false;
    while first_word.is_some() {
        let inked: Vec<usize> = cols.iter().copied().filter(|&col| (top..bottom).any(|row| is_ink(col, row))).collect();
        let tall = bottom - top;
        let word_w = first.chars().count() * tall * 9 / 20;
        let is_word = |w: usize| first.chars().all(char::is_alphanumeric) && w * 2 >= word_w && w * 10 <= word_w * 17;
        let gap = inked.windows(2).position(|pair| (pair[1] - pair[0]) * 5 > tall * 3);
        let Some(i) = gap.filter(|&i| (inked[i] - inked[0]) * 2 <= tall * 3 && !is_word(inked[i] - inked[0] + 1)) else { break };
        cols.retain(|&col| col > inked[i]);
        (top, bottom) = band(&cols)?;
        icon = true;
    }

    // A lone ascender or descender falls short of the three pixels asked of a row above.
    let reach = (bottom - top) / 2;
    for _ in 0..reach {
        if top > y0 && cols.iter().any(|&col| is_ink(col, top - 1)) {
            top -= 1;
        }
        if bottom < y1 && cols.iter().any(|&col| is_ink(col, bottom)) {
            bottom += 1;
        }
    }

    // Two lines with no blank row between them: a row far thinner than the solid rows on both
    // sides of it is the seam, and the line is the part holding the middle of the box.
    let ink: Vec<usize> = (top..bottom).map(|row| cols.iter().filter(|&&col| is_ink(col, row)).count()).collect();
    let peak = *ink.iter().max()?;
    let solid = |n: &usize| n * 2 >= peak;
    let (first, last) = (top + ink.iter().position(solid)?, top + ink.iter().rposition(solid)?);
    let centre = (middle.start + middle.end) / 2;
    let base = top;
    for row in (first..last).filter(|&row| ink[row - base] * 8 < peak) {
        if row < centre {
            top = row + 1;
        } else {
            bottom = row;
            break;
        }
    }

    if top >= bottom {
        return None; // the seam search can leave nothing between two seams
    }

    let (mut left, mut right) = (x1, x0);
    let (mut fg, mut farthest) = (bg, 0);
    let mut runs = [0u32; 32];
    for row in top..bottom {
        let mut run = 0;
        for &col in &cols {
            let p = pixel(bgra, width, col, row);
            let far = distance(p, bg);
            if far > GLYPH_DISTANCE {
                (left, right) = (left.min(col), right.max(col));
                run += 1;
            } else if run > 0 {
                runs[run.min(31)] += 1;
                run = 0;
            }
            if far > farthest {
                (fg, farthest) = (p, far);
            }
        }
    }
    if right < left {
        return None;
    }
    let half = runs.iter().sum::<u32>() / 2;
    let mut seen = 0;
    let stem = runs.iter().position(|&n| { seen += n; seen > half }).unwrap_or(0);
    let (x, y) = (left as i32, top as i32);
    Some(Glyphs { x, y, w: right as i32 + 1 - x, h: (bottom - top) as i32, bg, fg, stem, icon, picture })
}

/// Takes the letters off a picture and leaves the rest of it. The rows just outside the box say
/// what lies behind the text, and a pixel far from that is a letter.
fn paint_out_letters(bgra: &[u8], width: usize, height: usize, (x, y, w, h): (usize, usize, usize, usize)) -> Backdrop {
    let radius = h.max(8);
    let smooth_row = |row: usize| -> Vec<[i32; 3]> {
        (x..x + w)
            .map(|col| {
                let span = col.saturating_sub(radius)..(col + radius + 1).min(width);
                let n = span.len() as i32;
                let mut sum = [0; 3];
                for p in span.map(|c| pixel(bgra, width, c, row)) {
                    sum = [sum[0] + p[0], sum[1] + p[1], sum[2] + p[2]];
                }
                sum.map(|v| v / n)
            })
            .collect()
    };
    let (above, below) = (smooth_row(y.saturating_sub(1)), smooth_row((y + h).min(height - 1)));
    let behind = |col: usize, row: usize| {
        let (a, b) = (above[col], below[col]);
        let (down, up) = (row as i32 + 1, (h - row) as i32);
        [0, 1, 2].map(|c| (a[c] * up + b[c] * down) / (up + down))
    };

    let mut letter = vec![false; w * h];
    for row in 0..h {
        for col in 0..w {
            letter[row * w + col] = distance(pixel(bgra, width, x + col, y + row), behind(col, row)) > GLYPH_DISTANCE;
        }
    }
    let grown: Vec<bool> = (0..w * h)
        .map(|i| {
            let (row, col) = (i / w, i % w);
            let rows = row.saturating_sub(LETTER_FRINGE)..(row + LETTER_FRINGE + 1).min(h);
            rows.flat_map(|r| (col.saturating_sub(LETTER_FRINGE)..(col + LETTER_FRINGE + 1).min(w)).map(move |c| r * w + c)).any(|j| letter[j])
        })
        .collect();

    let mut out: Vec<[i32; 3]> = (0..w * h)
        .map(|i| if grown[i] { behind(i % w, i / w) } else { pixel(bgra, width, x + i % w, y + i / w) })
        .collect();
    for _ in 0..SMOOTHING {
        for i in (0..w * h).filter(|&i| grown[i]) {
            let (row, col) = (i / w, i % w);
            let around = [i.wrapping_sub(1).min(i - (col > 0) as usize), (i + (col + 1 < w) as usize), i - (row > 0) as usize * w, i + (row + 1 < h) as usize * w];
            out[i] = [0, 1, 2].map(|c| around.iter().map(|&j| out[j][c]).sum::<i32>() / 4);
        }
    }
    let mut pixels = Vec::with_capacity(w * h * 4);
    for p in out {
        // The overlay's see-through colour would punch a hole.
        let g = if (p[2], p[1], p[0]) == (255, 0, 255) { 1 } else { p[1] as u8 };
        pixels.extend([p[0] as u8, g, p[2] as u8, 255]);
    }
    Backdrop { w: w as i32, h: h as i32, bgra: pixels }
}

impl Patch {
    pub fn new(bgra: &[u8], width: usize, height: usize, block: &Block, text: String) -> Self {
        let g = &block.glyphs;
        let x = (g.x - MARGIN).max(0);
        let y = (g.y - MARGIN).max(0);
        let w = (g.x + g.w + MARGIN).min(width as i32) - x;
        let h = (g.y + g.h + MARGIN).min(height as i32) - y;
        // Japanese often runs wider than the English; on one line, spread before shrinking.
        let rows = g.y as usize..(g.y + g.h) as usize;
        let empty = |col: usize| rows.clone().all(|row| distance(pixel(bgra, width, col, row), g.bg) <= GLYPH_DISTANCE);
        let free = ((g.x + g.w) as usize..width).find(|&col| !empty(col)).unwrap_or(width) as i32;
        let max_w = if block.lines == 1 { (free - MARGIN - x).max(w) } else { w };
        let (bg, fg) = (colorref(g.bg), colorref(g.fg));
        Self { x, y, w, max_w, h, em: block.em, bg, fg, bold: g.bold(), backdrop: None, text, source: block.text.clone(), px: 0, text_h: 0 }
    }

    /// On a still picture the text goes without the picture going with it. A video would move on
    /// from whatever is painted here, so it keeps the flat box.
    pub fn keep_picture(mut self, bgra: &[u8], width: usize, height: usize) -> Self {
        let area = (self.x as usize, self.y as usize, self.w as usize, self.h as usize);
        self.backdrop = Some(Arc::new(paint_out_letters(bgra, width, height, area)));
        self
    }

    /// How much of the original text has gone, in percent. Only its sharpest edges count: a video
    /// playing behind a subtitle can change every other pixel, even the subtitle's outline.
    pub fn changed_percent(&self, frame: &[u8], basis: &[u8], width: usize) -> usize {
        let height = frame.len() / 4 / width;
        let rows = self.y.max(0) as usize..((self.y + self.h).max(0) as usize).min(height);
        let columns = self.x.max(0) as usize..((self.x + self.w).max(0) as usize).min(width.saturating_sub(EDGE_PX));
        let edge = |image: &[u8], x: usize, y: usize| {
            let (a, b) = (pixel(image, width, x, y), pixel(image, width, x + EDGE_PX, y));
            [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
        };
        let dot = |a: [i32; 3], b: [i32; 3]| (0..3).map(|c| a[c] * b[c]).sum::<i32>();
        let mut sharpest = 0;
        for y in rows.clone() {
            for x in columns.clone() {
                let was = edge(basis, x, y);
                sharpest = sharpest.max(dot(was, was));
            }
        }
        let (mut edges, mut gone) = (0, 0);
        for y in rows {
            for x in columns.clone() {
                let was = edge(basis, x, y);
                if dot(was, was) * 4 > sharpest {
                    edges += 1;
                    gone += (dot(edge(frame, x, y), was) * 4 < dot(was, was)) as usize;
                }
            }
        }
        gone * 100 / edges.max(1)
    }

    pub fn overlaps(&self, other: &Patch) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }

    /// True once the original text is no longer there.
    pub fn is_stale(&self, frame: &[u8], basis: &[u8], width: usize) -> bool {
        self.changed_percent(frame, basis, width) > GLYPH_STALE_PERCENT
    }

    /// A message in the region's top left corner, sized to its text.
    pub fn status(text: &str, width: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: width,
            max_w: width,
            h: 0,
            em: 0,
            bg: STATUS_BG,
            fg: STATUS_FG,
            bold: false,
            backdrop: None,
            text: text.to_string(),
            source: String::new(),
            px: 0,
            text_h: 0,
        }
    }
}

fn colorref([b, g, r]: [i32; 3]) -> u32 {
    let color = (b as u32) << 16 | (g as u32) << 8 | r as u32;
    if color == KEY_COLOR { color ^ 0x010000 } else { color } // would otherwise punch a hole
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];

    fn canvas(w: usize, h: usize, color: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [color[0], color[1], color[2], 0]).collect()
    }

    /// Paints a rectangle, every `step`th column: 3 looks like strokes of text, 1 is solid.
    fn paint(img: &mut [u8], w: usize, (x0, y0, x1, y1): (usize, usize, usize, usize), color: [u8; 3], step: usize) {
        for y in y0..y1 {
            for x in (x0..x1).step_by(step) {
                img[(y * w + x) * 4..][..3].copy_from_slice(&color);
            }
        }
    }

    #[test]
    fn dark_page_keeps_its_colours_and_finds_the_ink() {
        let mut img = canvas(200, 100, [0x10, 0x20, 0x30]);
        paint(&mut img, 200, (50, 40, 150, 60), [0xF0, 0xE0, 0xD0], 3);
        let g = glyphs(&img, 200, 100, (30, 20, 150, 60), Some("word")).unwrap();
        assert_eq!((g.bg, g.fg), ([0x10, 0x20, 0x30], [0xF0, 0xE0, 0xD0]));
        assert_eq!((g.x, g.y, g.w, g.h), (50, 40, 100, 20));
    }

    #[test]
    fn neighbours_caught_in_the_box_do_not_count() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (20, 12, 180, 28), BLACK, 3);
        paint(&mut img, 200, (0, 0, 200, 3), BLACK, 3); // the line above
        paint(&mut img, 200, (0, 37, 200, 40), BLACK, 3); // the line below
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.x, g.y, g.w, g.h), (20, 12, 160, 16));
    }

    #[test]
    fn a_button_keeps_its_own_colour_and_corners() {
        // White label on a blue button, on a white page that the box reaches into.
        let blue = [220, 110, 30];
        let mut img = canvas(120, 40, WHITE);
        paint(&mut img, 120, (0, 8, 120, 32), blue, 1);
        paint(&mut img, 120, (30, 16, 90, 24), WHITE, 3);
        let g = glyphs(&img, 120, 40, (0, 0, 120, 40), Some("word")).unwrap();
        assert_eq!(g.bg, [220, 110, 30]);
        assert_eq!((g.y, g.h), (16, 8));
    }

    #[test]
    fn a_field_border_is_not_tall_text() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (2, 0, 3, 40), BLACK, 1); // the input's left edge
        paint(&mut img, 200, (20, 14, 180, 26), BLACK, 3);
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.x, g.y, g.h), (20, 14, 12));
    }

    #[test]
    fn a_leading_icon_stays_out_of_the_rectangle() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (5, 8, 25, 32), BLACK, 1); // icon, taller than the text
        paint(&mut img, 200, (40, 14, 180, 26), BLACK, 3);
        for first in ["Projects", "Q", "\u{2461}"] {
            let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some(first)).unwrap();
            assert_eq!((g.x, g.y, g.h, g.icon), (40, 14, 12, true), "{first}");
        }
    }

    #[test]
    fn a_rounded_field_with_an_icon_leaves_only_the_text() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (0, 2, 200, 3), BLACK, 1); // top edge
        paint(&mut img, 200, (0, 37, 200, 38), BLACK, 1); // bottom edge
        for (x, y0, y1) in [(0, 8, 32), (1, 5, 8), (1, 32, 35), (2, 3, 5), (2, 35, 37)] {
            paint(&mut img, 200, (x, y0, x + 1, y1), BLACK, 1); // left edge, curving at the corners
        }
        paint(&mut img, 200, (10, 13, 22, 27), BLACK, 1); // search icon
        paint(&mut img, 200, (36, 14, 180, 26), BLACK, 3);
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("Q")).unwrap();
        assert_eq!((g.x, g.y, g.h, g.icon), (36, 14, 12, true));
    }

    #[test]
    fn words_are_not_icons() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (10, 14, 22, 26), BLACK, 3); // "to", set apart as in monospace
        paint(&mut img, 200, (31, 14, 180, 26), BLACK, 3);
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("to")).unwrap();
        assert_eq!((g.x, g.icon), (10, false));
    }

    #[test]
    fn a_lone_ascender_counts_as_part_of_the_line() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (20, 16, 180, 28), BLACK, 3);
        paint(&mut img, 200, (60, 11, 62, 16), BLACK, 1); // the l of "personal"
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.y, g.h), (11, 17));
    }

    #[test]
    fn lines_with_no_gap_are_told_apart_at_the_thin_row() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (10, 6, 190, 18), BLACK, 3); // this line
        paint(&mut img, 200, (60, 18, 62, 20), BLACK, 1); // a descender bridges the gap
        paint(&mut img, 200, (10, 20, 190, 32), BLACK, 3); // the next line
        let g = glyphs(&img, 200, 40, (0, 0, 200, 24), Some("word")).unwrap();
        assert_eq!((g.y, g.h), (6, 12));
    }

    #[test]
    fn all_capitals_are_not_split_at_their_thin_rows() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 8, 90, 10), BLACK, 1); // the bars of a row of E's
        paint(&mut img, 100, (10, 14, 90, 16), BLACK, 1);
        paint(&mut img, 100, (10, 20, 90, 22), BLACK, 1);
        paint(&mut img, 100, (10, 8, 90, 22), BLACK, 5); // their stems
        assert_eq!(glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")).unwrap().h, 14);
    }

    #[test]
    fn a_gap_of_one_row_stays_inside_the_line() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 8, 90, 11), BLACK, 15); // the dots of the i's
        paint(&mut img, 100, (10, 12, 90, 22), BLACK, 3);
        assert_eq!(glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")).unwrap().h, 14);
    }

    #[test]
    fn one_line_may_spread_over_empty_background_only() {
        let mut img = canvas(300, 40, WHITE);
        paint(&mut img, 300, (10, 10, 61, 30), BLACK, 3);
        paint(&mut img, 300, (200, 0, 201, 40), BLACK, 1); // a thin rule further along
        let g = glyphs(&img, 300, 40, (5, 5, 100, 30), Some("word")).unwrap();
        let mut one = Block { glyphs: g, lines: 1, em: 20, text: "x".into() };
        assert_eq!(Patch::new(&img, 300, 40, &one, "訳".into()).max_w, 190);
        one.lines = 2;
        let p = Patch::new(&img, 300, 40, &one, "訳".into());
        assert_eq!(p.max_w, p.w);
    }

    #[test]
    fn patch_never_leaves_the_image() {
        let mut img = canvas(100, 50, WHITE);
        paint(&mut img, 100, (0, 10, 40, 20), BLACK, 3);
        let g = glyphs(&img, 100, 50, (-10, 0, 60, 30), Some("word")).unwrap();
        let p = Patch::new(&img, 100, 50, &Block { glyphs: g, lines: 1, em: 10, text: "x".into() }, "訳".into());
        assert_eq!(p.x, 0);
    }

    #[test]
    fn flat_or_empty_boxes_have_no_glyphs() {
        let img = canvas(100, 50, [9, 9, 9]);
        assert!(glyphs(&img, 100, 50, (10, 10, 50, 20), Some("word")).is_none());
        assert!(glyphs(&img, 100, 50, (-20, -20, 10, 10), Some("word")).is_none());
    }

    #[test]
    fn style_separates_a_code_block_from_the_page() {
        let at = |bg, fg| Glyphs { x: 0, y: 0, w: 1, h: 1, bg, fg, stem: 1, icon: false, picture: false };
        let page = at([0, 0, 0], [236, 236, 236]);
        assert!(page.same_style(&at([1, 0, 0], [220, 230, 236])));
        assert!(!page.same_style(&at([35, 31, 27], [236, 236, 236])));
        assert!(!page.same_style(&at([0, 0, 0], [90, 170, 240])));
    }

    fn on_white(img: &[u8], width: usize, height: usize) -> Patch {
        let g = glyphs(img, width, height, (0, 0, width as i32, height as i32), None).unwrap();
        Patch::new(img, width, height, &Block { glyphs: g, lines: 1, em: 12, text: "x".into() }, "訳".into())
    }

    #[test]
    fn a_moving_background_does_not_make_text_stale() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut video = canvas(100, 40, [90, 140, 30]);
        paint(&mut video, 100, (10, 14, 90, 26), BLACK, 3);
        assert!(!patch.is_stale(&video, &basis, 100));
    }

    #[test]
    fn a_video_behind_an_outlined_subtitle_changes_nothing() {
        let subtitle = |back: [u8; 3]| {
            let mut img = canvas(100, 40, back);
            paint(&mut img, 100, (9, 13, 91, 27), BLACK, 1);
            paint(&mut img, 100, (10, 14, 90, 26), WHITE, 4);
            img
        };
        let basis = subtitle([60, 70, 80]);
        let patch = on_white(&basis, 100, 40);
        for back in [[20, 30, 40], [150, 130, 100], [230, 230, 230]] {
            assert!(patch.changed_percent(&subtitle(back), &basis, 100) <= WOBBLE_PERCENT);
        }
    }

    #[test]
    fn a_thick_subtitle_over_a_picture_is_covered_from_top_to_bottom() {
        let (w, h) = (300, 60);
        let mut img = canvas(w, h, WHITE);
        for y in 0..h {
            for x in 0..w {
                let v = (40 + x / 4 + y) as u8;
                img[(y * w + x) * 4..][..3].copy_from_slice(&[v, v + 10, v + 20]);
            }
        }
        for left in (20..270).step_by(16) {
            paint(&mut img, w, (left, 12, left + 12, 48), BLACK, 1);
            paint(&mut img, w, (left + 2, 14, left + 10, 46), WHITE, 1);
        }
        let g = glyphs(&img, w, h, (0, 0, w as i32, h as i32), None).unwrap();
        assert!(g.y <= 12 && g.y + g.h >= 48, "{} {}", g.y, g.h);
        assert!(g.bg.iter().sum::<i32>() < 3 * 170 && g.fg == [255, 255, 255], "{:?} {:?}", g.bg, g.fg);
    }

    #[test]
    fn letters_come_off_a_picture_and_the_picture_stays() {
        let (w, h) = (300usize, 60usize);
        let mut img = canvas(w, h, WHITE);
        for y in 0..h {
            for x in 0..w {
                let (v, grain) = ((40 + x / 4 + y) as u8, (x % 5 * 2) as u8);
                img[(y * w + x) * 4..][..3].copy_from_slice(&[v + grain, v + 10 + grain, v + 20 + grain]);
            }
        }
        let before = img.clone();
        for left in (20..270).step_by(16) {
            paint(&mut img, w, (left, 20, left + 6, 40), WHITE, 1);
        }
        let b = paint_out_letters(&img, w, h, (10, 16, 280, 28));
        let at = |x: usize, y: usize| pixel(&b.bgra, 280, x - 10, y - 16);
        assert!(distance(at(22, 30), pixel(&before, w, 22, 30)) < 30, "{:?}", at(22, 30));
        assert_eq!(at(32, 30), pixel(&before, w, 32, 30), "between the letters nothing changes");
    }

    #[test]
    fn the_see_through_colour_never_comes_out_of_a_picture() {
        let (w, h) = (40usize, 20usize);
        let mut img = canvas(w, h, WHITE);
        for p in img.chunks_mut(4) {
            p.copy_from_slice(&[255, 0, 255, 255]);
        }
        let b = paint_out_letters(&img, w, h, (5, 5, 20, 10));
        assert!(b.bgra.chunks(4).all(|p| p[..3] != [255, 0, 255]));
    }

    #[test]
    fn one_letter_changing_is_not_wobble() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut next = basis.clone();
        paint(&mut next, 100, (70, 14, 78, 26), WHITE, 1);
        let changed = patch.changed_percent(&next, &basis, 100);
        assert!(changed > WOBBLE_PERCENT && !patch.is_stale(&next, &basis, 100), "{changed}");
    }

    #[test]
    fn other_text_in_the_same_place_is_stale() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut other = canvas(100, 40, WHITE);
        paint(&mut other, 100, (11, 14, 90, 26), BLACK, 3);
        assert!(patch.is_stale(&other, &basis, 100));
    }

    #[test]
    fn seams_that_leave_nothing_are_no_line() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 0, 90, 12), BLACK, 3);
        paint(&mut img, 100, (40, 12, 41, 16), BLACK, 1); // thin rows right at the middle
        paint(&mut img, 100, (10, 16, 90, 30), BLACK, 3);
        if let Some(g) = glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")) {
            assert!(g.h > 0 && g.w > 0);
        }
    }

    #[test]
    fn key_colour_never_reaches_the_overlay() {
        assert_ne!(colorref([255, 0, 255]), KEY_COLOR);
    }
}
