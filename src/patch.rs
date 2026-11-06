//! What gets drawn over one paragraph: the translation in the original's colours, on a
//! rectangle that covers the original glyphs and nothing else.

use crate::ocr::Block;
use std::collections::HashMap;

/// Overlay pixels of exactly this colour are see-through. COLORREF, as are `bg` and `fg`.
pub const KEY_COLOR: u32 = 0x00FF00FF;
const GLYPH_DISTANCE: i32 = 64; // how far from the background a pixel must be to count as text
const MARGIN: i32 = 2; // covers the antialiased fringe outside the glyphs
const SAME_BG: i32 = 30;
const SAME_FG: i32 = 80; // thin text rarely reaches its true colour, so allow for that
const BOLD_STEM: usize = 8; // a stem wider than 1/8 of the line height, less one antialiased pixel
const MIN_BOLD_LINE: usize = 16; // small text is all antialiasing, so weight cannot be told
const STATUS_BG: u32 = 0x00201C1C;
const STATUS_FG: u32 = 0x00F5F5F5;

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub max_w: i32, // how far a single line may spread over empty background
    pub h: i32,     // 0 until laid out for a status message
    pub lines: i32,
    pub bg: u32,
    pub fg: u32,
    pub bold: bool,
    pub text: String,
    pub px: i32,     // font size, set when laid out on the UI thread
    pub text_h: i32, // measured height, ditto
}

struct Glyphs {
    bg: [i32; 3],
    fg: [i32; 3],
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    stem: usize, // median width of a horizontal stretch of ink: most rows cross vertical stems
}

fn distance(a: [i32; 3], b: [i32; 3]) -> i32 {
    (0..3).map(|c| (a[c] - b[c]).abs()).sum()
}

fn pixel(bgra: &[u8], width: usize, x: usize, y: usize) -> [i32; 3] {
    let p = &bgra[(y * width + x) * 4..][..3];
    [p[0] as i32, p[1] as i32, p[2] as i32]
}

/// Background, text colour and ink extent inside the rectangle, clipped to the image.
fn analyse(bgra: &[u8], width: usize, height: usize, x: i32, y: i32, w: i32, h: i32) -> Option<Glyphs> {
    let x0 = x.clamp(0, width as i32) as usize;
    let y0 = y.clamp(0, height as i32) as usize;
    let x1 = (x + w).clamp(0, width as i32) as usize;
    let y1 = (y + h).clamp(0, height as i32) as usize;

    // The commonest colour is the background: glyphs are thin strokes on it.
    let mut bins: HashMap<u16, [u64; 4]> = HashMap::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let [b, g, r] = pixel(bgra, width, x, y);
            let bin = bins.entry(((b >> 3) << 10 | (g >> 3) << 5 | r >> 3) as u16).or_default();
            *bin = [bin[0] + 1, bin[1] + b as u64, bin[2] + g as u64, bin[3] + r as u64];
        }
    }
    let [n, b, g, r] = *bins.values().max_by_key(|bin| bin[0])?;
    let bg = [(b / n) as i32, (g / n) as i32, (r / n) as i32];

    let (mut left, mut top, mut right, mut bottom) = (x1, y1, x0, y0);
    let (mut fg, mut farthest) = (bg, 0);
    let mut runs = [0u32; 32];
    for y in y0..y1 {
        let mut run = 0;
        for x in x0..x1 {
            let p = pixel(bgra, width, x, y);
            let far = distance(p, bg);
            let glyph = far > GLYPH_DISTANCE;
            if glyph {
                (left, top, right, bottom) = (left.min(x), top.min(y), right.max(x), bottom.max(y));
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
    let half = runs.iter().sum::<u32>() / 2;
    let mut seen = 0;
    let stem = runs.iter().position(|&n| { seen += n; seen > half }).unwrap_or(0);
    (right >= left).then_some(Glyphs { bg, fg, left, top, right, bottom, stem })
}

/// (background, text colour) of a line, for telling paragraphs apart.
pub fn palette(bgra: &[u8], width: usize, height: usize, x: i32, y: i32, w: i32, h: i32) -> ([i32; 3], [i32; 3]) {
    analyse(bgra, width, height, x, y, w, h).map_or(([0; 3], [0; 3]), |g| (g.bg, g.fg))
}

/// True when two lines sit on the same background in the same ink.
pub fn same_style(a: &([i32; 3], [i32; 3]), b: &([i32; 3], [i32; 3])) -> bool {
    distance(a.0, b.0) <= SAME_BG && distance(a.1, b.1) <= SAME_FG
}

impl Patch {
    /// `None` when the block has no pixels that stand out, such as a box in the padded edge.
    // shortcut: a flat fill, so a photo or gradient behind the text shows as a solid box;
    // inpaint from the surrounding pixels if that turns out to matter.
    pub fn new(bgra: &[u8], width: usize, height: usize, block: &Block, text: String) -> Option<Self> {
        let g = analyse(bgra, width, height, block.x, block.y, block.w, block.h)?;
        let (x0, y0) = (block.x.max(0), block.y.max(0));
        let (x1, y1) = (block.x + block.w, block.y + block.h);
        let x = (g.left as i32 - MARGIN).max(x0);
        let y = (g.top as i32 - MARGIN).max(y0);
        let w = (g.right as i32 + 1 + MARGIN).min(x1).min(width as i32) - x;
        let h = (g.bottom as i32 + 1 + MARGIN).min(y1).min(height as i32) - y;
        // Japanese often runs wider than the English; on one line, spread before shrinking.
        let empty = |col: usize| (g.top..=g.bottom).all(|row| distance(pixel(bgra, width, col, row), g.bg) <= GLYPH_DISTANCE);
        let free = (g.right + 1..width).find(|&col| !empty(col)).unwrap_or(width) as i32;
        let max_w = if block.lines == 1 { (free - MARGIN - x).max(w) } else { w };
        // Stroke width relative to line height; -1 discounts the antialiased edge pixel.
        let line_h = (g.bottom - g.top + 1) / block.lines as usize;
        let bold = line_h >= MIN_BOLD_LINE && g.stem.saturating_sub(1) * BOLD_STEM > line_h;
        Some(Self { x, y, w, max_w, h, lines: block.lines, bg: colorref(g.bg), fg: colorref(g.fg), bold, text, px: 0, text_h: 0 })
    }

    /// A message in the region's top left corner, sized to its text.
    pub fn status(text: &str, width: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: width,
            max_w: width,
            h: 0,
            lines: 1,
            bg: STATUS_BG,
            fg: STATUS_FG,
            bold: false,
            text: text.to_string(),
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

    /// A `w`x`h` image in `bg` with a filled rectangle standing in for glyphs.
    fn image(w: usize, h: usize, bg: [u8; 3], text: [u8; 3], glyphs: (usize, usize, usize, usize)) -> Vec<u8> {
        let (x0, y0, x1, y1) = glyphs;
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let c = if (x0..x1).contains(&x) && (y0..y1).contains(&y) { text } else { bg };
                [c[0], c[1], c[2], 0]
            })
            .collect()
    }

    fn block(x: i32, y: i32, w: i32, h: i32) -> Block {
        Block { x, y, w, h, lines: 1, text: "x".into() }
    }

    #[test]
    fn dark_page_keeps_its_colours_and_covers_only_the_glyphs() {
        let bgra = image(200, 100, [0x10, 0x20, 0x30], [0xF0, 0xE0, 0xD0], (50, 40, 150, 60));
        let p = Patch::new(&bgra, 200, 100, &block(30, 20, 150, 60), "訳".into()).unwrap();
        assert_eq!((p.bg, p.fg), (0x00102030, 0x00F0E0D0));
        assert_eq!((p.x, p.y, p.w, p.h), (48, 38, 104, 24));
    }

    #[test]
    fn light_page_works_the_same() {
        let bgra = image(200, 100, [255, 255, 255], [0, 0, 0], (50, 40, 150, 60));
        let p = Patch::new(&bgra, 200, 100, &block(30, 20, 150, 60), "訳".into()).unwrap();
        assert_eq!((p.bg, p.fg), (0x00FFFFFF, 0));
    }

    #[test]
    fn patch_never_leaves_the_block_or_the_image() {
        let bgra = image(100, 50, [255, 255, 255], [0, 0, 0], (0, 0, 49, 29));
        let p = Patch::new(&bgra, 100, 50, &block(-10, -10, 60, 40), "訳".into()).unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (0, 0, 50, 30));
    }

    #[test]
    fn flat_or_empty_blocks_are_skipped() {
        let bgra = image(100, 50, [9, 9, 9], [9, 9, 9], (0, 0, 0, 0));
        assert!(Patch::new(&bgra, 100, 50, &block(10, 10, 50, 20), "訳".into()).is_none());
        assert!(Patch::new(&bgra, 100, 50, &block(-20, -20, 10, 10), "訳".into()).is_none());
    }

    #[test]
    fn one_line_may_spread_over_empty_background_only() {
        let mut bgra = image(300, 40, [255, 255, 255], [0, 0, 0], (10, 10, 60, 30));
        for y in 0..40 {
            bgra[(y * 300 + 200) * 4] = 0; // a thin rule further along the row
            bgra[(y * 300 + 200) * 4 + 1] = 0;
            bgra[(y * 300 + 200) * 4 + 2] = 0;
        }
        let mut one = block(5, 5, 100, 34);
        let p = Patch::new(&bgra, 300, 40, &one, "訳".into()).unwrap();
        assert_eq!(p.x + p.max_w, 198);
        one.lines = 2;
        let p = Patch::new(&bgra, 300, 40, &one, "訳".into()).unwrap();
        assert_eq!(p.max_w, p.w);
    }

    #[test]
    fn style_separates_a_code_block_from_the_page() {
        let page = ([0, 0, 0], [236, 236, 236]);
        assert!(same_style(&page, &([1, 0, 0], [220, 230, 236])));
        assert!(!same_style(&page, &([35, 31, 27], [236, 236, 236])));
        assert!(!same_style(&page, &([0, 0, 0], [90, 170, 240])));
    }

    #[test]
    fn key_colour_never_reaches_the_overlay() {
        assert_ne!(colorref([255, 0, 255]), KEY_COLOR);
    }
}
