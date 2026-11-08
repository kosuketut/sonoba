//! The work behind a region: a watcher that notices what changes on screen, OCR, and translation.
//! Nothing here touches a window; the platform shows what comes out and supplies the pixels.

use crate::i18n::t;
use crate::patch::{self, Patch, WOBBLE_PERCENT};
use crate::platform::{self, Grabber};
use crate::{config, engine, models, ocr, translate};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(33); // a screen refresh or two
const WATCH: Duration = Duration::from_millis(100); // one-shot results, which take a capture trick
const PARTIAL_EVERY: Duration = Duration::from_millis(30); // repaints while a page fills in
const MOVE_SETTLE: Duration = Duration::from_millis(120); // no frames are judged until a drag pauses
const STALE_PERCENT: usize = 8; // of the pixels: a scrolled page, not a hover highlight or a caret
const SETTLE: Duration = Duration::from_millis(300);
const OCR_IDLE: Duration = Duration::from_secs(60); // free the OCR models this long after the last job
const ENGINE_IDLE: Duration = Duration::from_secs(15 * 60); // and the model after this long
const MAX_REST: Duration = Duration::from_millis(500); // the longest pause between frames that keep coming
const READING_SLIP: usize = 2; // letters OCR may read differently from one frame to the next
const SAME_PLACE_PX: i32 = 3; // a patch this close to the last one with the same text is the same patch

const NOT_FOUND: &str = "翻訳する文字が見つかりません";
pub const WORKING: &str = "翻訳中…";

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One of several regions. Each keeps the mode it was selected in, as if it were alone.
#[derive(Clone, Copy, PartialEq)]
pub struct Part {
    pub region: Region,
    pub live: bool,
}

#[derive(Clone, Copy)]
pub struct Job {
    pub region: Region,
    pub live: bool,
    pub id: u64,
    pub window: usize, // the window the region belongs to, if any
}

pub static JOB: Mutex<Option<Job>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
pub static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static WARM_UP: AtomicBool = AtomicBool::new(false);
static UNLOAD: AtomicBool = AtomicBool::new(false);
pub static MOVED_AT: Mutex<Option<Instant>> = Mutex::new(None); // the last time the anchor window moved
pub static PAUSED: AtomicBool = AtomicBool::new(false); // the anchor window is minimised
pub static SUSPENDED: AtomicBool = AtomicBool::new(false); // paused from the tray: no keys, no jobs
pub static PARTS: Mutex<Vec<Part>> = Mutex::new(Vec::new()); // the regions of a job with several

pub fn request_load() {
    WARM_UP.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

pub fn request_unload() {
    UNLOAD.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

pub fn set_job(job: Option<Job>) {
    *JOB.lock().unwrap() = job;
    WAKE.notify_one();
}

pub fn current_job(id: u64) -> Option<Job> {
    JOB.lock().unwrap().filter(|job| job.id == id)
}

fn is_current(id: u64) -> bool {
    current_job(id).is_some()
}

// --- Worker: a watcher notices what changes on screen, however busy OCR is, and hands the newest
// frame over to the worker. So work never piles up, and stale text goes as soon as it is stale.

pub struct Frame {
    pub pixels: Vec<u8>,
    pub w: usize,
    pub h: usize,
}

pub struct Shared {
    frame: Mutex<Option<Frame>>, // the newest change, until the worker takes it
    fresh: Condvar,
    obsolete: AtomicBool, // the text being translated has left the screen
    done: AtomicBool,     // the current frame's translation is on show
    shown: Mutex<(Vec<u8>, Vec<Patch>)>, // what is on show, and the pixels it was made from
    working: Mutex<(Vec<u8>, Vec<Patch>)>, // the same for the text being translated
}

pub fn worker() {
    let mut models: Option<ocr::Ocr> = None;
    let mut translator = translate::Translator::new();
    let mut idle_since = Instant::now();
    loop {
        let mut job = JOB.lock().unwrap();
        let requested = || WARM_UP.load(Ordering::Relaxed) || UNLOAD.load(Ordering::Relaxed) || engine::STATE.load(Ordering::Relaxed) == engine::LOADING;
        while job.is_none() && !requested() {
            let (guard, wait) = WAKE.wait_timeout(job, OCR_IDLE).unwrap();
            job = guard;
            if wait.timed_out() {
                free_when_idle(idle_since, &mut models, &mut translator);
            }
        }
        let current = *job;
        drop(job);
        match current {
            Some(job) => run(job, &mut models, &mut translator),
            None => serve_requests(&mut models, &mut translator, &|| JOB.lock().unwrap().is_none()),
        }
        idle_since = Instant::now();
    }
}

/// Loads the OCR models, or reloads them if the thread setting has changed.
fn ensure_ocr(ocr_models: &mut Option<ocr::Ocr>) -> Result<(), String> {
    let threads = config::get().threads();
    let korean = models::is_present(models::KOREAN_OCR).then(|| models::path(models::KOREAN_OCR));
    if !ocr_models.as_ref().is_some_and(|m| m.matches(threads, korean.is_some())) {
        *ocr_models = Some(ocr::Ocr::new(threads, korean.as_deref())?);
    }
    Ok(())
}

/// Does what the settings window asked for. A load gives way once `keep_going` turns false, and
/// comes back here to finish, so that the state on show is never left at "loading".
fn serve_requests(models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, keep_going: &impl Fn() -> bool) {
    if UNLOAD.swap(false, Ordering::Relaxed) {
        translator.unload();
    }
    if WARM_UP.swap(false, Ordering::Relaxed) || engine::STATE.load(Ordering::Relaxed) == engine::LOADING {
        let _ = ensure_ocr(models);
        translator.warm_up(&|| keep_going() && !UNLOAD.load(Ordering::Relaxed));
    }
}

fn free_when_idle(since: Instant, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator) {
    translator.drop_dead_engine();
    if since.elapsed() >= OCR_IDLE {
        *models = None;
    }
    if since.elapsed() >= ENGINE_IDLE {
        translator.unload();
    }
}

pub fn new_shared() -> Shared {
    Shared {
        frame: Mutex::new(None),
        fresh: Condvar::new(),
        obsolete: AtomicBool::new(false),
        done: AtomicBool::new(false),
        shown: Mutex::new((Vec::new(), Vec::new())),
        working: Mutex::new((Vec::new(), Vec::new())),
    }
}

fn run(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator) {
    let shared = new_shared();
    std::thread::scope(|scope| {
        scope.spawn(|| watch(job, &shared));
        translate_frames(job, models, translator, &shared);
    });
}

fn translate_frames(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, shared: &Shared) {
    let post = |patches: Vec<Patch>| platform::post_patches(job.id, patches);
    let mut idle_since = Instant::now();
    loop {
        let frame = loop {
            if !is_current(job.id) {
                return;
            }
            let (mut pending, _) = shared.fresh.wait_timeout_while(shared.frame.lock().unwrap(), POLL * 3, |frame| frame.is_none()).unwrap();
            if let Some(frame) = pending.take() {
                break frame;
            }
            drop(pending); // a load takes a while, and the watcher needs this
            serve_requests(models, translator, &|| is_current(job.id));
            free_when_idle(idle_since, models, translator); // a one-shot result can stay up for hours
        };
        shared.obsolete.store(false, Ordering::Relaxed);

        let start = Instant::now();
        // Not "has the screen changed": behind a subtitle, a video changes it all the time.
        let keep_going = || is_current(job.id) && !shared.obsolete.load(Ordering::Relaxed);
        let result = translate_frame(&frame, job.live, models, translator, &keep_going, &post, shared);
        let mut patches = result.unwrap_or_else(|e| vec![Patch::status(&format!("エラー: {e}"), frame.w as i32)]);
        if keep_going() {
            let mut shown = shared.shown.lock().unwrap();
            hold_still(&mut patches, &shown, &frame);
            let changed = patches != shown.1;
            *shown = (frame.pixels, patches.clone());
            drop(shown);
            shared.done.store(true, Ordering::Relaxed);
            post(patches);
            if changed {
                platform::post_translated(job.id);
            }
        }
        // A video in the region changes every frame: this keeps OCR to half of one's time,
        // until the text in it changes.
        let rest = start.elapsed().min(MAX_REST);
        if shared.frame.lock().unwrap().is_some() {
            let resting = Instant::now();
            while resting.elapsed() < rest && !shared.obsolete.load(Ordering::Relaxed) {
                std::thread::sleep(POLL / 3);
            }
        }
        idle_since = Instant::now();
    }
}

/// Two readings of one line: all but a letter or two around a part they share.
fn same_reading(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let start = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let end = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    start + end + READING_SLIP >= a.len().max(b.len())
}

/// OCR of a moving picture wobbles: a box moves by a pixel or two, a letter reads differently, a
/// line goes unread for a frame. Text that stayed keeps its old patch.
// shortcut: one letter changing in over fifty counts as wobble; a window into the text would tell.
fn hold_still(patches: &mut Vec<Patch>, (basis, shown): &(Vec<u8>, Vec<Patch>), frame: &Frame) {
    let unchanged = |old: &Patch| basis.len() == frame.pixels.len() && old.changed_percent(&frame.pixels, basis, frame.w) <= WOBBLE_PERCENT;
    for patch in patches.iter_mut() {
        let near = |a: i32, b: i32| (a - b).abs() <= SAME_PLACE_PX;
        let same = shown.iter().find(|old| {
            let place = near(old.x, patch.x) && near(old.y, patch.y) && near(old.w, patch.w) && near(old.h, patch.h);
            (old.text == patch.text && place) || (old.overlaps(patch) && same_reading(&old.source, &patch.source) && unchanged(old))
        });
        if let Some(old) = same {
            *patch = old.clone();
        }
    }
    let unread = shown.iter().filter(|old| !old.source.is_empty() && unchanged(old) && !patches.iter().any(|p| p.overlaps(old)));
    let unread: Vec<Patch> = unread.cloned().collect();
    patches.extend(unread);
    patches.sort_by_key(|p| (p.y, p.x)); // what comes and goes must not look like a change
}

/// Hands changed frames to the worker. Live, every change counts, and text whose pixels have
/// changed comes off straight away. One-shot, the job ends once most of the page has changed.
/// Several regions are read as one picture of the area around them, blank between them: a live
/// region as it is now, a one-shot region as it first was, so that only live ones get translated
/// again. Returns the one-shot regions whose page has changed, which are done with.
// shortcut: regions far apart make a large picture, which OCR scales down; read them one by one
// if small text across the screen turns out to matter.
pub fn keep_parts(frame: &[u8], out: &mut Vec<u8>, width: usize, (x, y): (i32, i32), parts: &[Part], still: &mut Vec<(Region, Vec<u8>)>) -> Vec<Region> {
    let height = frame.len() / 4 / width;
    let mut gone = Vec::new();
    out.clear();
    out.resize(frame.len(), 0);
    for part in parts {
        let region = part.region;
        let span = |from: i32, length: i32, at: i32, limit: usize| ((from - at).clamp(0, limit as i32) as usize, (from + length - at).clamp(0, limit as i32) as usize);
        let (left, right) = span(region.x, region.w, x, width);
        let (top, bottom) = span(region.y, region.h, y, height);
        let rows = || (top..bottom).map(|row| (row * width + left) * 4..(row * width + right) * 4);
        let now: Vec<u8> = rows().flat_map(|cells| frame[cells].iter().copied()).collect();
        let shown = if part.live {
            now
        } else {
            match still.iter().find(|(r, _)| *r == region) {
                Some((_, first)) if changed_pixels(&now, first) * 100 > first.len() / 4 * STALE_PERCENT => {
                    gone.push(region);
                    continue;
                }
                Some((_, first)) => first.clone(),
                None => {
                    still.push((region, now.clone()));
                    now
                }
            }
        };
        let line = (right - left) * 4;
        if line > 0 {
            for (cells, row) in rows().zip(shown.chunks_exact(line)) {
                out[cells].copy_from_slice(row);
            }
        }
    }
    gone
}

/// A one-shot region of a job that also has live ones is as still as a one-shot job.
fn in_still_part(glyphs: &patch::Glyphs) -> bool {
    let Some(job) = *JOB.lock().unwrap() else { return false };
    let (x, y) = (job.region.x + glyphs.x, job.region.y + glyphs.y);
    PARTS.lock().unwrap().iter().any(|p| !p.live && x >= p.region.x && x < p.region.x + p.region.w && y >= p.region.y && y < p.region.y + p.region.h)
}

fn watch(job: Job, shared: &Shared) {
    let post = |patches: Vec<Patch>| platform::post_patches(job.id, patches);
    // Never OCR our own output. The last watcher may have just lifted this, and what was on show
    // takes a frame or two to leave the screen.
    platform::hide_overlay_from_capture(true);
    std::thread::sleep(POLL * 2);
    let finish = || {
        set_job(None);
        platform::post_job_over(job.id);
    };
    let mut grabber: Option<Grabber> = None;
    let mut size = (0, 0);
    let mut last: Vec<u8> = Vec::new();
    let mut parted: Vec<u8> = Vec::new();
    let mut still: Vec<(Region, Vec<u8>)> = Vec::new();
    let started = Instant::now();
    while let Some(current) = current_job(job.id) {
        let Region { x, y, w, h } = current.region;
        if w <= 0 || h <= 0 {
            std::thread::sleep(WATCH); // the window is off every screen for now
            continue;
        }
        if (w, h) != size {
            grabber = Grabber::new(w, h);
            size = (w, h);
            last.clear();
            shared.done.store(false, Ordering::Relaxed);
            shared.obsolete.store(true, Ordering::Relaxed);
        }
        let Some(grabber) = &mut grabber else {
            post(vec![Patch::status(&t(platform::CAPTURE_FAILED), w)]);
            return finish();
        };
        if current.window != 0 && platform::window_is_gone(current.window) {
            return finish();
        }
        let moving = MOVED_AT.lock().unwrap().is_some_and(|at| at.elapsed() < MOVE_SETTLE);
        let settled = last.is_empty() || job.live || shared.done.load(Ordering::Relaxed);
        if !moving && settled && !PAUSED.load(Ordering::Relaxed) {
            // A one-shot result stays visible to screenshots, so it leaves the capture only here.
            let trick = !job.live && !last.is_empty();
            if trick {
                platform::hide_overlay_from_capture(true);
            }
            let mut frame = grabber.grab(x, y);
            if trick {
                platform::hide_overlay_from_capture(false);
            }
            let parts = PARTS.lock().unwrap().clone();
            if !parts.is_empty() {
                // The selection layer takes a moment to leave the screen, so the first looks count for nothing.
                if started.elapsed() < SETTLE {
                    still.clear();
                }
                let gone = keep_parts(frame, &mut parted, w as usize, (x, y), &parts, &mut still);
                frame = &parted;
                if !gone.is_empty() {
                    PARTS.lock().unwrap().retain(|p| !gone.contains(&p.region));
                    if gone.len() == parts.len() {
                        return finish();
                    }
                    platform::post_parts(job.id);
                }
            }
            if frame != last {
                let first = last.is_empty();
                if job.live || first {
                    if job.live {
                        drop_stale_patches(frame, w as usize, shared, &post);
                        give_up_on_stale_work(frame, w as usize, shared);
                    }
                    last.clear();
                    last.extend_from_slice(frame);
                    *shared.frame.lock().unwrap() = Some(Frame { pixels: last.clone(), w: w as usize, h: h as usize });
                    shared.fresh.notify_one();
                } else if changed_pixels(frame, &last) * 100 > last.len() / 4 * STALE_PERCENT {
                    return finish();
                }
            }
        }
        let watching = job.live && (PARTS.lock().unwrap().is_empty() || PARTS.lock().unwrap().iter().any(|p| p.live));
        std::thread::sleep(if watching || last.is_empty() { POLL } else { WATCH });
    }
}

pub fn changed_pixels(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(p, q)| p != q).count()
}

fn drop_stale_patches(frame: &[u8], width: usize, shared: &Shared, post: &impl Fn(Vec<Patch>)) {
    let (basis, patches) = &mut *shared.shown.lock().unwrap();
    if basis.len() != frame.len() {
        return;
    }
    let before = patches.len();
    patches.retain(|p| !p.is_stale(frame, basis, width));
    if patches.len() != before {
        post(patches.clone());
    }
}

/// Translating a page that has gone would only paint its text over the next one.
fn give_up_on_stale_work(frame: &[u8], width: usize, shared: &Shared) {
    let (basis, patches) = &*shared.working.lock().unwrap();
    if basis.len() == frame.len() {
        let stale = patches.iter().filter(|p| p.is_stale(frame, basis, width)).count();
        if stale * 2 > patches.len() {
            shared.obsolete.store(true, Ordering::Relaxed);
        }
    }
}

pub fn translate_frame(
    frame: &Frame,
    live: bool,
    models: &mut Option<ocr::Ocr>,
    translator: &mut translate::Translator,
    keep_going: &(impl Fn() -> bool + Sync),
    post: &(impl Fn(Vec<Patch>) + Sync),
    shared: &Shared,
) -> Result<Vec<Patch>, String> {
    ensure_ocr(models)?;
    translator.follow_languages();
    let Frame { pixels, w, h } = frame;
    let (w, h) = (*w, *h);
    let frame = pixels.as_slice();
    let lines = models.as_ref().unwrap().read(frame, w, h)?;
    let blocks = ocr::paragraphs(&lines);
    let blocks: Vec<&ocr::Block> = blocks.iter().filter(|b| !ocr::looks_like_code(&b.text)).collect();
    let everything = blocks.iter().map(|block| Patch::new(frame, w, h, block, String::new())).collect();
    *shared.working.lock().unwrap() = (frame.to_vec(), everything);
    // Numbers, code and the like come back unchanged: leave the original on show.
    let patch = |block: &ocr::Block, text: &str| {
        (text.to_lowercase() != block.text.to_lowercase()).then(|| {
            let patch = Patch::new(frame, w, h, block, text.to_string());
            if block.glyphs.picture && (!live || in_still_part(&block.glyphs)) { patch.keep_picture(frame, w, h) } else { patch }
        })
    };
    // Whatever is known already goes up with the first new line, so nothing flickers.
    let mut slots: Vec<Option<Patch>> = vec![None; blocks.len()];
    let mut pending: Vec<&str> = Vec::new();
    for (slot, block) in slots.iter_mut().zip(&blocks) {
        match translator.known(&block.text) {
            Some(text) => *slot = patch(block, &text),
            None if !pending.contains(&block.text.as_str()) => pending.push(&block.text),
            None => {}
        }
    }
    let shared = Mutex::new((slots, Instant::now()));
    let result = translator.translate_all(&pending, keep_going, &|source, translation| {
        let (slots, last_post) = &mut *shared.lock().unwrap();
        for (slot, block) in slots.iter_mut().zip(&blocks) {
            if block.text == source {
                *slot = patch(block, translation);
            }
        }
        if last_post.elapsed() > PARTIAL_EVERY {
            *last_post = Instant::now();
            post(slots.iter().flatten().cloned().collect());
        }
    });
    let mut out: Vec<Patch> = shared.into_inner().unwrap().0.into_iter().flatten().collect();
    // What did finish stays on show: one failed reply is no reason to lose the page.
    if let Err(e) = result {
        out.push(Patch::status(&format!("{}{}", t("エラー: "), t(&e)), w as i32));
    } else if out.is_empty() && !live {
        out.push(Patch::status(&t(NOT_FOUND), w as i32));
    }
    Ok(out)
}

pub fn around_all(parts: &[Part]) -> Region {
    let left = parts.iter().map(|p| p.region.x).min().unwrap_or(0);
    let top = parts.iter().map(|p| p.region.y).min().unwrap_or(0);
    let right = parts.iter().map(|p| p.region.x + p.region.w).max().unwrap_or(0);
    let bottom = parts.iter().map(|p| p.region.y + p.region.h).max().unwrap_or(0);
    Region { x: left, y: top, w: right - left, h: bottom - top }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_parts_of_a_picture_are_kept() {
        let (width, height) = (10usize, 4usize);
        let frame = vec![9u8; width * height * 4];
        let mut out = Vec::new();
        let parts = [Part { region: Region { x: 102, y: 201, w: 2, h: 1 }, live: true }, Part { region: Region { x: 108, y: 203, w: 5, h: 5 }, live: false }];
        keep_parts(&frame, &mut out, width, (100, 200), &parts, &mut Vec::new());
        let kept: Vec<(usize, usize)> = (0..width * height).filter(|i| out[i * 4] == 9).map(|i| (i % width, i / width)).collect();
        assert_eq!(kept, [(2, 1), (3, 1), (8, 3), (9, 3)]);
    }

    #[test]
    fn a_one_shot_region_holds_still_beside_a_live_one_until_its_page_changes() {
        let width = 40usize;
        let region = |x, live| Part { region: Region { x, y: 0, w: 20, h: 4 }, live };
        let parts = [region(0, true), region(20, false)];
        let (mut out, mut still) = (Vec::new(), Vec::new());
        let mut frame = vec![1u8; width * 4 * 4];
        assert!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).is_empty());

        // A pixel changes in each: the live region shows it, the one-shot one does not.
        for at in [0, 39] {
            frame[at * 4] = 200;
        }
        assert!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).is_empty());
        assert_eq!((out[0], out[39 * 4]), (200, 1));

        // Most of the one-shot region changes, so it is done with.
        frame.iter_mut().for_each(|v| *v = 200);
        assert_eq!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).len(), 1);
    }

    #[test]
    fn the_box_around_regions_holds_them_all() {
        let part = |x, y, w, h| Part { region: Region { x, y, w, h }, live: false };
        let parts = [part(10, 50, 5, 5), part(-4, 60, 2, 2)];
        let b = around_all(&parts);
        assert_eq!((b.x, b.y, b.w, b.h), (-4, 50, 19, 12));
    }

    #[test]
    fn readings_that_differ_by_a_letter_are_one_reading() {
        assert!(same_reading("the guardian before dawn", "the quardian before dawn"));
        assert!(same_reading("answer shape", "answer shape."));
        assert!(!same_reading("You must defeat the guardian", "The bank was muddy"));
    }

    fn page(columns: &[usize]) -> Vec<u8> {
        let (mut pixels, width) = (vec![255u8; 100 * 20 * 4], 100);
        for y in 5..15 {
            for x in columns {
                pixels[(y * width + x) * 4..][..3].copy_from_slice(&[0, 0, 0]);
            }
        }
        pixels
    }

    fn shown_patch(source: &str, text: &str) -> Patch {
        Patch { x: 0, y: 0, w: 100, h: 20, text: text.to_string(), source: source.to_string(), ..Patch::status("", 100) }
    }

    #[test]
    fn a_letter_read_differently_keeps_the_translation_until_the_pixels_change() {
        let columns: Vec<usize> = (10..90).step_by(6).collect();
        let shown = (page(&columns), vec![shown_patch("guardian", "守護者")]);
        let still = Frame { pixels: shown.0.clone(), w: 100, h: 20 };
        let mut patches = vec![shown_patch("quardian", "クァルディアン")];
        hold_still(&mut patches, &shown, &still);
        assert_eq!(patches[0].text, "守護者");

        let changed = Frame { pixels: page(&columns[..8]), w: 100, h: 20 };
        let mut patches = vec![shown_patch("guardians", "守護者たち")];
        hold_still(&mut patches, &shown, &changed);
        assert_eq!(patches[0].text, "守護者たち");
    }

    #[test]
    fn a_line_unread_for_a_frame_stays_until_its_pixels_go() {
        let columns: Vec<usize> = (10..90).step_by(6).collect();
        let shown = (page(&columns), vec![shown_patch("guardian", "守護者")]);
        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: shown.0.clone(), w: 100, h: 20 });
        assert_eq!(patches.len(), 1);

        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: page(&[]), w: 100, h: 20 });
        assert!(patches.is_empty());
    }

    #[test]
    fn a_status_message_is_not_held() {
        let shown = (page(&[]), vec![Patch::status("エラー", 100)]);
        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: shown.0.clone(), w: 100, h: 20 });
        assert!(patches.is_empty());
    }
}
