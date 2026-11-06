//! PP-OCRv6 small: DB text detection, then CTC recognition, both on the CPU.
//! The values below match RapidOCR's PP-OCRv6 config so results stay comparable.

use rten::Model;
use rten_tensor::NdTensor;

static DET: &[u8] = include_bytes!("../models/PP-OCRv6_det_small.onnx");
static REC: &[u8] = include_bytes!("../models/PP-OCRv6_rec_small.onnx");
static DICT: &str = include_str!("../models/dict.txt");

// shortcut: long sides above this are scaled down, so a whole 4K screen of small text
// reads poorly; tile the image if full-screen regions turn out to matter.
const MAX_DET_SIDE: usize = 1600;
const PAD: usize = 16; // the detector misses text touching the image edge
const DET_THRESH: f32 = 0.3;
const BOX_THRESH: f32 = 0.5;
const UNCLIP_RATIO: f32 = 1.6;
const REC_HEIGHT: usize = 48;
const REC_MIN_WIDTH: usize = 320;
const REC_MAX_WIDTH: usize = 3200;
const TEXT_SCORE: f32 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
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
}

impl Ocr {
    pub fn new() -> Result<Self, String> {
        let load = |b| Model::load_static_slice(b).map_err(|e| e.to_string());
        let mut chars = vec![""]; // class 0 is the CTC blank
        chars.extend(DICT.lines().filter(|s| !s.is_empty())); // git may check dict.txt out as CRLF
        chars.push(" ");
        Ok(Self { det: load(DET)?, rec: load(REC)?, chars })
    }

    pub fn read(&self, bgra: &[u8], w: usize, h: usize) -> Result<Vec<Line>, String> {
        let img = Rgbf::from_bgra_padded(bgra, w, h, PAD);
        let mut lines = Vec::new();
        for (x, y, bw, bh) in self.detect(&img)? {
            let text = self.recognize(&img, (x, y, bw, bh))?;
            if !text.is_empty() {
                let (lx, ly) = (x as i32 - PAD as i32, y as i32 - PAD as i32);
                lines.push(Line { x: lx, y: ly, w: bw as i32, h: bh as i32, text });
            }
        }
        reading_order(&mut lines);
        Ok(lines)
    }

    fn detect(&self, img: &Rgbf) -> Result<Vec<(usize, usize, usize, usize)>, String> {
        let ratio = (MAX_DET_SIDE as f32 / img.w.max(img.h) as f32).min(1.0);
        let round32 = |v: usize| (((v as f32 * ratio) / 32.0).round() as usize).max(1) * 32;
        let (dw, dh) = (round32(img.w), round32(img.h));
        let input = NdTensor::from_data([1, 3, dh, dw], img.sample_chw((0, 0, img.w, img.h), dw, dh, dw));
        let out = self.det.run_one(input.into(), None).map_err(|e| e.to_string())?;
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
        let width = (bw * REC_HEIGHT).div_ceil(bh).clamp(1, REC_MAX_WIDTH);
        let stride = width.max(REC_MIN_WIDTH);
        let input = NdTensor::from_data([1, 3, REC_HEIGHT, stride], img.sample_chw(rect, width, REC_HEIGHT, stride));
        let out = self.rec.run_one(input.into(), None).map_err(|e| e.to_string())?;
        let (shape, probs) = out.into_shape_vec::<f32, 3>().map_err(|e| e.to_string())?;
        if shape[2] != self.chars.len() {
            return Err(format!("model has {} classes, dictionary {}", shape[2], self.chars.len()));
        }
        Ok(ctc_decode(&probs, shape[2], &self.chars))
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

fn ctc_decode(probs: &[f32], classes: usize, chars: &[&str]) -> String {
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
    if n == 0.0 || sum / n < TEXT_SCORE { String::new() } else { text.trim().to_string() }
}

/// Puts lines of a same-row group into left-to-right order, keeping rows top to bottom.
fn reading_order(lines: &mut [Line]) {
    lines.sort_by_key(|l| (l.y, l.x));
    for i in 1..lines.len() {
        let mut j = i;
        while j > 0 && same_row(&lines[j - 1], &lines[j]) && lines[j].x < lines[j - 1].x {
            lines.swap(j - 1, j);
            j -= 1;
        }
    }
}

fn same_row(a: &Line, b: &Line) -> bool {
    let overlap = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    overlap * 2 > a.h.min(b.h)
}

/// Joins wrapped lines so each paragraph reaches the translator as a whole sentence.
pub fn paragraphs(lines: &[Line]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<&Line> = None;
    let mut right = 0;
    for line in lines {
        match prev.filter(|a| continues(a, line, right)) {
            Some(_) => {
                let last = out.last_mut().unwrap();
                let hyphen = last.ends_with('-') && line.text.starts_with(|c: char| c.is_lowercase());
                if hyphen {
                    last.pop();
                } else {
                    last.push(' ');
                }
                last.push_str(&line.text);
                right = right.max(line.x + line.w);
            }
            None => {
                out.push(line.text.clone());
                right = line.x + line.w;
            }
        }
        prev = Some(line);
    }
    out
}

/// True when `b` carries on the paragraph that `a` ended, wrapped by the layout.
fn continues(a: &Line, b: &Line, para_right: i32) -> bool {
    let h = a.h.max(b.h);
    let gap = b.y - (a.y + a.h);
    let similar_height = (a.h - b.h).abs() * 3 <= h;
    let same_column = (a.x + a.w).min(b.x + b.w) > a.x.max(b.x);
    if same_row(a, b) || gap * 2 > h || !similar_height || !same_column {
        return false;
    }
    if b.text.starts_with(|c: char| c.is_lowercase()) {
        return true;
    }
    if a.text.split(' ').count() < 4 {
        return false; // menu items and labels, not wrapped prose
    }
    // A hard line break: b's first word would have fitted on a, so nothing forced a wrap.
    let first_word = b.text.split(' ').next().unwrap_or("").chars().count() + 1;
    let word_w = b.w * first_word as i32 / b.text.chars().count().max(1) as i32;
    a.x + a.w + word_w > para_right.max(b.x + b.w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(x: i32, y: i32, w: i32, text: &str) -> Line {
        Line { x, y, w, h: 20, text: text.into() }
    }

    #[test]
    fn wrapped_lines_join_and_labels_stay_apart() {
        let wrapped = [l(0, 0, 500, "The quick update changes how"), l(0, 24, 300, "tasks are scheduled.")];
        assert_eq!(paragraphs(&wrapped), ["The quick update changes how tasks are scheduled."]);

        let menu = [l(0, 0, 60, "Sign in"), l(0, 24, 80, "Settings"), l(0, 48, 300, "About this application")];
        assert_eq!(paragraphs(&menu).len(), 3);

        let far = [l(0, 0, 500, "First paragraph ends here."), l(0, 80, 400, "Second one starts.")];
        assert_eq!(paragraphs(&far).len(), 2);

        // Detector boxes are 30px tall here, so one blank line leaves a gap of only 20.
        let tall = |y, w, text: &str| Line { x: 0, y, w, h: 30, text: text.into() };
        let blank_line = [tall(123, 980, "before saving, otherwise the file may be incomplete."), tall(173, 975, "Press any key")];
        assert_eq!(paragraphs(&blank_line).len(), 2);

        let columns = [l(0, 0, 100, "Name"), l(400, 0, 100, "Value")];
        assert_eq!(paragraphs(&columns).len(), 2);
    }

    #[test]
    fn hyphenated_wrap_rejoins_the_word() {
        let lines = [l(0, 0, 500, "this is an inter-"), l(0, 24, 300, "national update")];
        assert_eq!(paragraphs(&lines), ["this is an international update"]);
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
        assert_eq!(ctc_decode(&probs, 3, &chars), "aab");
    }

    #[test]
    fn low_confidence_text_is_dropped() {
        assert_eq!(ctc_decode(&[0.3, 0.4, 0.3], 3, &["", "a", "b"]), "");
    }

    fn png_bgra(path: &str) -> (Vec<u8>, usize, usize) {
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

    /// Boxes are taller than the glyphs, so a blank line looks smaller than a line height.
    #[test]
    fn blank_line_separates_paragraphs() {
        let ocr = Ocr::new().unwrap();
        let (bgra, w, h) = png_bgra("testdata/paragraphs.png");
        let lines = ocr.read(&bgra, w, h).unwrap();
        let got = paragraphs(&lines);
        assert_eq!(got.len(), 4, "{got:#?}");
        assert_eq!(got[3], "Press any key to continue");
    }

    /// The same renders RapidOCR read without a single mistake.
    #[test]
    fn reads_screen_samples() {
        const WANT: &str = "The quick update changes how background tasks are scheduled. If your session expires, \
            you'll need to sign in again before saving. Don't close this window until the upload has finished, \
            otherwise the file may be incomplete.";
        let ocr = Ocr::new().unwrap();
        for name in ["dark-14px", "game-22px", "mono-13px", "ui-12px", "ui-14px", "web-16px"] {
            let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
            let start = std::time::Instant::now();
            let lines = ocr.read(&bgra, w, h).unwrap();
            let elapsed = start.elapsed();
            let got = paragraphs(&lines).join(" / ");
            println!("{name}: {elapsed:?} {got}");
            assert_eq!(got, WANT, "{name}");
        }
    }
}
