//! PP-OCRv6 small: DB text detection, then CTC recognition, both on the CPU.
//! The values below match RapidOCR's PP-OCRv6 config so results stay comparable.

use crate::patch::{self, Glyphs};
use rayon::prelude::*;
use rten::{Model, RunOptions, ThreadPool};
use rten_tensor::NdTensor;
use std::path::Path;
use std::sync::Arc;

static DET: &[u8] = include_bytes!("../models/PP-OCRv6_det_small.onnx");
static REC: &[u8] = include_bytes!("../models/PP-OCRv6_rec_small.onnx");
static DICT: &str = include_str!("../models/dict.txt");
static DICT_KOREAN: &str = include_str!("../models/dict_korean.txt");

// shortcut: long sides above this are scaled down, so a whole 4K screen of small text
// reads poorly; tile the image if full-screen regions turn out to matter.
const MAX_DET_SIDE: usize = 1600;
const PAD: usize = 16; // the detector misses text touching the image edge
const DET_THRESH: f32 = 0.3;
const BOX_THRESH: f32 = 0.5;
const UNCLIP_RATIO: f32 = 1.6;
const REC_HEIGHT: usize = 48;
const REC_MIN_WIDTH: usize = 160;
const REC_MAX_WIDTH: usize = 3200;
const TEXT_SCORE: f32 = 0.5;
const SURE_SCORE: f32 = 0.93; // a line read this well is no Hangul, and needs no second look

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub glyphs: Glyphs,
    pub text: String,
}

/// A BGR image with values already in the -1..1 range the models expect.
struct Rgbf {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

impl Rgbf {
    /// `bgra` is top-down, tightly packed.
    fn from_bgra_padded(bgra: &[u8], w: usize, h: usize, pad: usize) -> Self {
        let (pw, ph) = (w + pad * 2, h + pad * 2);
        // Padding with the corner colour keeps the edge from reading as a text stroke.
        let fill = [bgra[0], bgra[1], bgra[2]].map(norm);
        let mut px = Vec::with_capacity(pw * ph * 3);
        for _ in 0..ph * pw {
            px.extend_from_slice(&fill);
        }
        for y in 0..h {
            for x in 0..w {
                let (s, d) = ((y * w + x) * 4, ((y + pad) * pw + x + pad) * 3);
                for c in 0..3 {
                    px[d + c] = norm(bgra[s + c]);
                }
            }
        }
        Self { w: pw, h: ph, px }
    }

    /// Bilinear resample of the sub-rectangle into `dw`x`dh` CHW planes, leaving `stride`
    /// columns per row so a line can be right-padded with zeros.
    fn sample_chw(&self, (x0, y0, sw, sh): (usize, usize, usize, usize), dw: usize, dh: usize, stride: usize) -> Vec<f32> {
        let mut out = vec![0f32; 3 * dh * stride];
        let (fx, fy) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
        for dy in 0..dh {
            let sy = ((dy as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
            let (iy, ty) = (sy as usize, sy.fract());
            let iy1 = (iy + 1).min(sh - 1);
            for dx in 0..dw {
                let sx = ((dx as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
                let (ix, tx) = (sx as usize, sx.fract());
                let ix1 = (ix + 1).min(sw - 1);
                for c in 0..3 {
                    let at = |x: usize, y: usize| self.px[((y0 + y) * self.w + x0 + x) * 3 + c];
                    let top = at(ix, iy) * (1.0 - tx) + at(ix1, iy) * tx;
                    let bot = at(ix, iy1) * (1.0 - tx) + at(ix1, iy1) * tx;
                    out[(c * dh + dy) * stride + dx] = top * (1.0 - ty) + bot * ty;
                }
            }
        }
        out
    }
}

fn norm(v: u8) -> f32 {
    v as f32 / 127.5 - 1.0
}

pub struct Ocr {
    det: Model,
    rec: Model,
    chars: Vec<&'static str>,
    korean: Option<(Model, Vec<&'static str>)>,
    pool: Arc<ThreadPool>,
    threads: usize,
}

fn dictionary(lines: &'static str) -> Vec<&'static str> {
    let mut chars = vec![""]; // class 0 is the CTC blank
    chars.extend(lines.lines().filter(|s| !s.is_empty())); // git may check dict.txt out as CRLF
    chars.push(" ");
    chars
}

impl Ocr {
    /// PP-OCRv6 reads Chinese, English, Japanese and Latin scripts. Hangul takes a model of its
    /// own, which comes as a download.
    pub fn new(threads: usize, korean: Option<&Path>) -> Result<Self, String> {
        let load = |b| Model::load_static_slice(b).map_err(|e| e.to_string());
        let korean = match korean {
            Some(path) => {
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                Some((Model::load(bytes).map_err(|e| e.to_string())?, dictionary(DICT_KOREAN)))
            }
            None => None,
        };
        let pool = Arc::new(ThreadPool::with_num_threads(threads));
        Ok(Self { det: load(DET)?, rec: load(REC)?, chars: dictionary(DICT), korean, pool, threads })
    }

    pub fn matches(&self, threads: usize, korean: bool) -> bool {
        self.threads == threads && self.korean.is_some() == korean
    }

    fn run_options(&self) -> Option<RunOptions> {
        Some(RunOptions::default().with_thread_pool(Some(self.pool.clone())))
    }

    pub fn read(&self, bgra: &[u8], w: usize, h: usize) -> Result<Vec<Line>, String> {
        let img = Rgbf::from_bgra_padded(bgra, w, h, PAD);
        let boxes = self.detect(&img)?;
        let lines: Vec<Option<Line>> =
            self.pool.run(|| boxes.par_iter().map(|&rect| self.read_line(bgra, &img, (w, h), rect)).collect::<Result<_, _>>())?;
        let mut lines: Vec<Line> = lines.into_iter().flatten().collect();
        reading_order(&mut lines);
        Ok(lines)
    }

    fn read_line(&self, bgra: &[u8], img: &Rgbf, (w, h): (usize, usize), rect: (usize, usize, usize, usize)) -> Result<Option<Line>, String> {
        let (x, y, bw, bh) = rect;
        let in_frame = (x as i32 - PAD as i32, y as i32 - PAD as i32, bw as i32, bh as i32);
        let Some(ink) = patch::glyphs(bgra, w, h, in_frame, None) else { return Ok(None) };
        // Ink from the lines above and below sinks the recogniser's confidence, so crop it off.
        let margin = (ink.h / 2) as usize;
        let top = ((ink.y + PAD as i32) as usize).saturating_sub(margin).max(y);
        let bottom = ((ink.y + ink.h + PAD as i32) as usize + margin).min(y + bh);
        let text = self.recognize(img, (x, top, bw, bottom.saturating_sub(top)))?;
        let first_word = text.split(' ').next().unwrap_or("");
        Ok(patch::glyphs(bgra, w, h, in_frame, Some(first_word))
            .filter(|_| !text.is_empty())
            .map(|glyphs| Line { glyphs, text: without_icon(text.clone(), glyphs.icon) }))
    }

    fn detect(&self, img: &Rgbf) -> Result<Vec<(usize, usize, usize, usize)>, String> {
        let ratio = (MAX_DET_SIDE as f32 / img.w.max(img.h) as f32).min(1.0);
        let round32 = |v: usize| (((v as f32 * ratio) / 32.0).round() as usize).max(1) * 32;
        let (dw, dh) = (round32(img.w), round32(img.h));
        let input = NdTensor::from_data([1, 3, dh, dw], img.sample_chw((0, 0, img.w, img.h), dw, dh, dw));
        let out = self.det.run_one(input.into(), self.run_options()).map_err(|e| e.to_string())?;
        let (shape, prob) = out.into_shape_vec::<f32, 4>().map_err(|e| e.to_string())?;
        let (mh, mw) = (shape[2], shape[3]);
        let (sx, sy) = (img.w as f32 / mw as f32, img.h as f32 / mh as f32);
        let mut boxes: Vec<_> = text_boxes(&prob, mw, mh)
            .into_iter()
            .map(|(x, y, bw, bh)| {
                let x0 = (x * sx).max(0.0) as usize;
                let y0 = (y * sy).max(0.0) as usize;
                let x1 = ((x + bw) * sx).ceil().min(img.w as f32) as usize;
                let y1 = ((y + bh) * sy).ceil().min(img.h as f32) as usize;
                (x0, y0, x1 - x0, y1 - y0)
            })
            .filter(|&(_, _, bw, bh)| bw > 3 && bh > 3)
            .collect();
        boxes.sort_by_key(|&(x, y, _, _)| (y, x));
        Ok(boxes)
    }

    fn recognize(&self, img: &Rgbf, rect: (usize, usize, usize, usize)) -> Result<String, String> {
        let (_, _, bw, bh) = rect;
        if bw == 0 || bh == 0 {
            return Ok(String::new());
        }
        let (text, score) = self.read_with(&self.rec, &self.chars, img, rect)?;
        // Hangul reads badly in the main model, so doubt is what sends a line to the other one.
        if let Some((model, chars)) = self.korean.as_ref().filter(|_| score < SURE_SCORE) {
            let (hangul, hangul_score) = self.read_with(model, chars, img, rect)?;
            if hangul_score > score && hangul.contains(is_hangul) {
                return Ok(hangul);
            }
        }
        Ok(if score < TEXT_SCORE { String::new() } else { text })
    }

    fn read_with(&self, model: &Model, chars: &[&str], img: &Rgbf, rect: (usize, usize, usize, usize)) -> Result<(String, f32), String> {
        let (_, _, bw, bh) = rect;
        let width = (bw * REC_HEIGHT).div_ceil(bh).clamp(1, REC_MAX_WIDTH);
        let stride = width.max(REC_MIN_WIDTH);
        let input = NdTensor::from_data([1, 3, REC_HEIGHT, stride], img.sample_chw(rect, width, REC_HEIGHT, stride));
        let out = model.run_one(input.into(), self.run_options()).map_err(|e| e.to_string())?;
        let (shape, probs) = out.into_shape_vec::<f32, 3>().map_err(|e| e.to_string())?;
        if shape[2] != chars.len() {
            return Err(format!("model has {} classes, dictionary {}", shape[2], chars.len()));
        }
        Ok(ctc_decode(&probs, shape[2], chars))
    }
}

/// Connected regions of the detector's probability map, as (x, y, w, h) in map pixels,
/// grown by the DB unclip distance. Axis-aligned only: screen text is not rotated.
fn text_boxes(prob: &[f32], w: usize, h: usize) -> Vec<(f32, f32, f32, f32)> {
    let mut on: Vec<bool> = prob.iter().map(|&p| p > DET_THRESH).collect();
    // Same 2x2 dilation as RapidOCR's use_dilation, joining strokes split by the threshold.
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            on[i] = on[i]
                || (x > 0 && on[i - 1])
                || (y > 0 && on[i - w])
                || (x > 0 && y > 0 && on[i - w - 1]);
        }
    }
    let mut seen = vec![false; w * h];
    let mut stack = Vec::new();
    let mut boxes = Vec::new();
    for start in 0..w * h {
        if !on[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let (mut sum, mut n) = (0f32, 0f32);
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            sum += prob[i];
            n += 1.0;
            for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                if nx < w && ny < h && on[ny * w + nx] && !seen[ny * w + nx] {
                    seen[ny * w + nx] = true;
                    stack.push(ny * w + nx);
                }
            }
        }
        if sum / n < BOX_THRESH {
            continue;
        }
        let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
        let grow = bw * bh * UNCLIP_RATIO / (2.0 * (bw + bh));
        boxes.push((x0 as f32 - grow, y0 as f32 - grow, bw + grow * 2.0, bh + grow * 2.0));
    }
    boxes
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}')
}

/// The text, and how sure the model was of it.
fn ctc_decode(probs: &[f32], classes: usize, chars: &[&str]) -> (String, f32) {
    let (mut text, mut sum, mut n, mut prev) = (String::new(), 0f32, 0f32, 0);
    for step in probs.chunks_exact(classes) {
        let (best, &p) = step.iter().enumerate().fold((0, &step[0]), |a, b| if b.1 > a.1 { b } else { a });
        if best != 0 && best != prev {
            text.push_str(chars[best]);
            sum += p;
            n += 1.0;
        }
        prev = best;
    }
    if n == 0.0 { (String::new(), 0.0) } else { (text.trim().to_string(), sum / n) }
}

/// The recogniser reads an icon as a stray character or two, such as a circled digit.
fn without_icon(text: String, icon: bool) -> String {
    match text.split_once(' ') {
        Some((first, rest)) if icon && first.chars().count() <= 2 => rest.to_string(),
        _ => text,
    }
}

/// Puts lines of a same-row group into left-to-right order, keeping rows top to bottom.
fn reading_order(lines: &mut [Line]) {
    lines.sort_by_key(|l| (l.glyphs.y, l.glyphs.x));
    for i in 1..lines.len() {
        let mut j = i;
        while j > 0 && same_row(&lines[j - 1].glyphs, &lines[j].glyphs) && lines[j].glyphs.x < lines[j - 1].glyphs.x {
            lines.swap(j - 1, j);
            j -= 1;
        }
    }
}

fn same_row(a: &Glyphs, b: &Glyphs) -> bool {
    let overlap = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    overlap * 2 > a.h.min(b.h)
}

/// The font size that gives a line this much ink, going by which parts of the letters the
/// text uses: "same" fills only the x-height, "Sign" also reaches up and down.
fn font_size(ink_h: i32, text: &str) -> f32 {
    let tall = text.chars().any(|c| c.is_uppercase() || c.is_ascii_digit() || "bdfhiklt".contains(c) || !c.is_ascii());
    let deep = text.chars().any(|c| "gjpqy,;()".contains(c) || !c.is_ascii());
    ink_h as f32 / (if tall { 0.72 } else { 0.52 } + if deep { 0.21 } else { 0.0 })
}

/// One paragraph, with its lines' combined ink.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub glyphs: Glyphs,
    pub lines: i32,
    pub em: i32,
    pub text: String,
}

/// Joins wrapped lines so each paragraph reaches the translator as a whole sentence.
pub fn paragraphs(lines: &[Line]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut prev: Option<&Line> = None;
    let mut em_sum = 0.0;
    for line in lines {
        let (g, em) = (&line.glyphs, font_size(line.glyphs.h, &line.text));
        match out.last_mut().filter(|b| prev.is_some_and(|a| continues(a, line, b.glyphs.x + b.glyphs.w))) {
            Some(block) => {
                let hyphen = block.text.ends_with('-') && line.text.starts_with(|c: char| c.is_lowercase());
                if hyphen {
                    block.text.pop();
                } else if !(block.text.ends_with(is_cjk) && line.text.starts_with(is_cjk)) {
                    block.text.push(' ');
                }
                block.text.push_str(&line.text);
                let b = &mut block.glyphs;
                b.picture |= g.picture;
                let (right, bottom) = ((b.x + b.w).max(g.x + g.w), (b.y + b.h).max(g.y + g.h));
                (b.x, b.y) = (b.x.min(g.x), b.y.min(g.y));
                (b.w, b.h) = (right - b.x, bottom - b.y);
                block.lines += 1;
                em_sum += em;
                block.em = (em_sum / block.lines as f32).round() as i32;
            }
            None => {
                em_sum = em;
                out.push(Block { glyphs: *g, lines: 1, em: em.round() as i32, text: line.text.clone() });
            }
        }
        prev = Some(line);
    }
    out
}

/// Japanese and Chinese wrap anywhere, with no space to put back.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// Code stays as it is: the model would otherwise paraphrase it. Prose rarely spends one
/// character in twelve on these.
pub fn looks_like_code(text: &str) -> bool {
    let symbols = text.matches(['=', '(', ')', '{', '}', '[', ']', ';', '_', '<', '>']).count();
    let prose_brackets = text.matches(" (").count() * 2; // "Save (Ctrl+S)": code has them right after a name
    symbols.saturating_sub(prose_brackets) * 12 >= text.chars().count()
}

/// True when `b` carries on the paragraph that `a` ended, wrapped by the layout.
fn continues(line_a: &Line, line_b: &Line, para_right: i32) -> bool {
    let (a, b) = (&line_a.glyphs, &line_b.glyphs);
    let h = a.h.max(b.h);
    let gap = b.y - (a.y + a.h);
    let similar_height = (a.h - b.h).abs() * 3 <= h;
    let same_column = (a.x + a.w).min(b.x + b.w) > a.x.max(b.x);
    if same_row(a, b) || gap * 10 > h * 11 || !similar_height || !same_column || !a.same_style(b) {
        return false;
    }
    if line_b.text.starts_with(|c: char| c.is_lowercase()) {
        return true;
    }
    // Japanese has no words to go by: a line that ran to the right edge was wrapped.
    if line_a.text.ends_with(is_cjk) && line_b.text.starts_with(is_cjk) {
        return a.x + a.w + h > para_right.max(b.x + b.w);
    }
    if line_a.text.split(' ').count() < 4 {
        return false; // menu items and labels, not wrapped prose
    }
    // A hard line break: b's first word would have fitted on a, so nothing forced a wrap.
    let first_word = line_b.text.split(' ').next().unwrap_or("").chars().count() + 1;
    let word_w = b.w * first_word as i32 / line_b.text.chars().count().max(1) as i32;
    a.x + a.w + word_w > para_right.max(b.x + b.w)
}

#[cfg(test)]
pub fn png_bgra(path: &str) -> (Vec<u8>, usize, usize) {
    let mut reader = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let ch = info.color_type.samples();
    let bgra = buf[..info.buffer_size()]
        .chunks_exact(ch)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    (bgra, info.width as usize, info.height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: i32, y: i32, w: i32, h: i32, text: &str) -> Line {
        Line { glyphs: Glyphs { x, y, w, h, bg: [0; 3], fg: [255; 3], stem: 2, icon: false, picture: false }, text: text.into() }
    }

    fn l(x: i32, y: i32, w: i32, text: &str) -> Line {
        at(x, y, w, 18, text)
    }

    #[test]
    fn wrapped_lines_join_and_labels_stay_apart() {
        let wrapped = [l(0, 0, 500, "The quick update changes how"), l(0, 24, 300, "tasks are scheduled.")];
        let joined = paragraphs(&wrapped);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].text, "The quick update changes how tasks are scheduled.");
        let g = joined[0].glyphs;
        assert_eq!((g.x, g.y, g.w, g.h, joined[0].lines), (0, 0, 500, 42, 2));

        let menu = [l(0, 0, 60, "Sign in"), l(0, 24, 80, "Settings"), l(0, 48, 300, "About this application")];
        assert_eq!(paragraphs(&menu).len(), 3);

        let far = [l(0, 0, 500, "First paragraph ends here."), l(0, 48, 400, "Second one starts.")];
        assert_eq!(paragraphs(&far).len(), 2);

        let columns = [l(0, 0, 100, "Name"), l(400, 0, 100, "Value")];
        assert_eq!(paragraphs(&columns).len(), 2);

        // A lowercase start is no reason to join text onto a differently coloured panel.
        let mut code = l(0, 24, 300, "python");
        code.glyphs.bg = [35, 31, 27];
        assert_eq!(paragraphs(&[l(0, 0, 500, "pay more for the output."), code]).len(), 2);
    }

    #[test]
    fn an_icon_read_as_text_is_dropped() {
        assert_eq!(without_icon("② personal".into(), true), "personal");
        assert_eq!(without_icon("Q Search snippets".into(), true), "Search snippets");
        assert_eq!(without_icon("Projects".into(), true), "Projects");
        assert_eq!(without_icon("A shortcut".into(), false), "A shortcut");
    }

    #[test]
    fn font_size_allows_for_the_letters_in_the_line() {
        assert_eq!(font_size(14, "Sign in").round(), 15.0);
        assert_eq!(font_size(11, "HELLO").round(), 15.0);
        assert_eq!(font_size(8, "same").round(), 15.0);
    }

    #[test]
    fn code_is_told_from_prose() {
        assert!(looks_like_code("while not done: action = llm(context) result = run_tool(action) context += result"));
        assert!(looks_like_code("result = run_tool(action)"));
        assert!(!looks_like_code("Settings (Beta)"));
        assert!(!looks_like_code("Save (Ctrl+S)"));
        assert!(!looks_like_code("Tool calling lets a model request a function in a predictable shape (mostly)."));
        assert!(!looks_like_code("error: cannot borrow `x` as mutable because it is also borrowed as immutable"));
    }

    #[test]
    fn hyphenated_wrap_rejoins_the_word() {
        let lines = [l(0, 0, 500, "this is an inter-"), l(0, 24, 300, "national update")];
        assert_eq!(paragraphs(&lines)[0].text, "this is an international update");
    }

    #[test]
    fn same_row_reads_left_to_right() {
        let mut lines = [l(300, 3, 100, "right"), l(0, 0, 100, "left"), l(0, 30, 100, "below")];
        reading_order(&mut lines);
        let order: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(order, ["left", "right", "below"]);
    }

    #[test]
    fn ctc_collapses_repeats_and_blanks() {
        let chars = ["", "a", "b"];
        #[rustfmt::skip]
        let probs = [
            0.1, 0.8, 0.1, // a
            0.1, 0.8, 0.1, // repeat
            0.9, 0.0, 0.1, // blank
            0.1, 0.8, 0.1, // a again
            0.0, 0.1, 0.9, // b
        ];
        assert_eq!(ctc_decode(&probs, 3, &chars).0, "aab");
    }

    #[test]
    fn doubt_shows_in_the_score() {
        assert!(ctc_decode(&[0.3, 0.4, 0.3], 3, &["", "a", "b"]).1 < TEXT_SCORE);
    }

    /// Boxes are taller than the glyphs, so a blank line looks smaller than a line height.
    #[test]
    fn blank_line_separates_paragraphs() {
        let ocr = Ocr::new(4, None).unwrap();
        let (bgra, w, h) = png_bgra("testdata/paragraphs.png");
        let lines = ocr.read(&bgra, w, h).unwrap();
        let got = paragraphs(&lines);
        assert_eq!(got.len(), 4, "{got:#?}");
        assert_eq!(got[3].text, "Press any key to continue");
    }

    #[test]
    fn bold_is_recognised_at_heading_and_body_sizes() {
        let ocr = Ocr::new(4, None).unwrap();
        let (bgra, w, h) = png_bgra("testdata/weights.png");
        let blocks = paragraphs(&ocr.read(&bgra, w, h).unwrap());
        let got: Vec<_> = blocks
            .iter()
            .map(|b| (b.text.as_str(), b.glyphs.bold()))
            .collect();
        assert_eq!(
            got,
            [
                ("First the problem Jev is solving", true),
                ("First the problem Jev is solving", false),
                ("Tool calling lets a model request a function", true),
                ("Tool calling lets a model request a function", false),
                ("Press any key to continue", false),
            ]
        );
    }

    /// Video frames are noisy, and once brought the seam search down to a line of no height.
    #[test]
    fn noisy_frames_never_panic() {
        let ocr = Ocr::new(4, None).unwrap();
        let (clean, w, h) = png_bgra("testdata/paragraphs.png");
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..24 {
            let mut noisy = clean.clone();
            for _ in 0..round * 400 {
                let (x, y) = ((next() % w as u64) as usize, (next() % h as u64) as usize);
                let (dw, dh) = ((next() % 40 + 1) as usize, (next() % 6 + 1) as usize);
                let shade = (next() % 256) as u8;
                for row in y..(y + dh).min(h) {
                    for col in x..(x + dw).min(w) {
                        noisy[(row * w + col) * 4..][..3].fill(shade);
                    }
                }
            }
            let lines = ocr.read(&noisy, w, h).unwrap();
            for block in paragraphs(&lines) {
                let patch = patch::Patch::new(&noisy, w, h, &block, "訳".into());
                assert!(patch.w > 0 && patch.h > 0);
            }
        }
    }

    /// The same renders RapidOCR read without a single mistake.
    #[test]
    fn reads_screen_samples() {
        const WANT: &str = "The quick update changes how background tasks are scheduled. If your session expires, \
            you'll need to sign in again before saving. Don't close this window until the upload has finished, \
            otherwise the file may be incomplete.";
        let ocr = Ocr::new(4, None).unwrap();
        for name in ["dark-14px", "game-22px", "mono-13px", "ui-12px", "ui-14px", "web-16px"] {
            let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
            let start = std::time::Instant::now();
            let lines = ocr.read(&bgra, w, h).unwrap();
            let elapsed = start.elapsed();
            let got = paragraphs(&lines).iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" / ");
            println!("{name}: {elapsed:?} {got}");
            assert_eq!(got, WANT, "{name}");
        }
    }

    #[test]
    fn reads_hangul_with_its_own_model() {
        let (bgra, w, h) = png_bgra("testdata/korean.png");
        let lines = with_korean().read(&bgra, w, h).unwrap();
        let got = paragraphs(&lines).iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" / ");
        println!("{got}");
        assert!(got.contains("화면의 글자를 번역합니다") && got.contains("언어를 바꿀 수 있습니다"), "{got}");
    }

    fn with_korean() -> Ocr {
        Ocr::new(4, Some(Path::new("models/korean_PP-OCRv5_mobile_rec.onnx"))).unwrap()
    }

    #[test]
    fn the_korean_download_leaves_other_languages_alone() {
        let ocr = with_korean();
        for (name, want) in [("mixed-en", "Please read the manual"), ("mixed-ja", "取扱説明書をよく読んでください"), ("mixed-zh", "请仔细阅读说明书")] {
            let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
            let got = paragraphs(&ocr.read(&bgra, w, h).unwrap()).iter().map(|b| b.text.clone()).collect::<Vec<_>>().join(" / ");
            println!("{name}: {got}");
            assert!(got.contains(want), "{name}: {got}");
            assert!(!got.contains(is_hangul), "{name}: {got}");
        }
        let (bgra, w, h) = png_bgra("testdata/ui-14px.png");
        let got = paragraphs(&ocr.read(&bgra, w, h).unwrap()).iter().map(|b| b.text.clone()).collect::<Vec<_>>().join(" ");
        assert!(!got.contains(is_hangul), "{got}");
    }

    #[test]
    fn wrapped_japanese_takes_no_space() {
        let line = |y: i32, text: &str| Line { glyphs: Glyphs { x: 0, y, w: 400, h: 20, bg: [255; 3], fg: [0; 3], stem: 2, icon: false, picture: false }, text: text.to_string() };
        let blocks = paragraphs(&[line(0, "取扱説明書をよく読んで、"), line(26, "正しく使ってください。")]);
        assert_eq!(blocks[0].text, "取扱説明書をよく読んで、正しく使ってください。");
    }
}
