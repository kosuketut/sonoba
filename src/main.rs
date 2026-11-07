//! Screen OCR and English to Japanese translation, driven by global hotkeys.
//!
//! No window here ever takes the keyboard focus: the selection layer and the overlay are
//! WS_EX_NOACTIVATE and Esc is a hotkey, so the active window stays active throughout.
//! The overlay also lets the mouse through, and capture skips it, so OCR keeps seeing the
//! original text underneath the translation.
#![windows_subsystem = "windows"]

mod bar;
mod capture;
mod config;
mod engine;
mod i18n;
mod models;
mod ocr;
mod patch;
mod settings;
mod translate;

use i18n::t;
use patch::{Backdrop, KEY_COLOR, Patch, WOBBLE_PERCENT};
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const POLL: Duration = Duration::from_millis(33); // a screen refresh or two
const WATCH: Duration = Duration::from_millis(100); // one-shot results, which take a capture trick
const PARTIAL_EVERY: Duration = Duration::from_millis(30); // repaints while a page fills in
const MOVE_SETTLE: Duration = Duration::from_millis(120); // no frames are judged until a drag pauses
const STALE_PERCENT: usize = 8; // of the pixels: a scrolled page, not a hover highlight or a caret
const OCR_IDLE: Duration = Duration::from_secs(60); // free the OCR models this long after the last job
const ENGINE_IDLE: Duration = Duration::from_secs(15 * 60); // and the model after this long
const MAX_REST: Duration = Duration::from_millis(500); // the longest pause between frames that keep coming
const READING_SLIP: usize = 2; // letters OCR may read differently from one frame to the next
const SAME_PLACE_PX: i32 = 3; // a patch this close to the last one with the same text is the same patch
const MIN_SELECTION: i32 = 8;
const MIN_TEXT_PX: i32 = 9; // below this Japanese stops being legible; the patch grows instead
const STATUS_PX: i32 = 16;

const HK_ONCE: i32 = 1;
const HK_LIVE: i32 = 2;
const HK_STOP: i32 = 3;
const HK_ESC: i32 = 4;
const WM_PATCHES: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const WM_JOB_OVER: u32 = WM_APP + 3;
const WM_TRANSLATED: u32 = WM_APP + 4;
const FLASH_TIMER: usize = 1;
const FLASH_MS: u32 = 450;
const FRAME_PX: i32 = 2;
const FLASH_PX: i32 = 4;
const OUTLINE_EVERY_PX: i32 = 18; // of font size, for the outline of text on a picture
const FRAME_COLOR: u32 = 0x0071CC2E; // green, as COLORREF
const FLASH_COLOR: u32 = 0x0088FF39;
const MENU_EXIT: usize = 1;
const MENU_SETTINGS: usize = 2;
const HK_PROBE: i32 = 5;

const DIM_ALPHA: u8 = 110;
const HOVER_FRAME: i32 = 4;
const HOVER_COLOR: u32 = 0x00FFFFFF;

const NOT_FOUND: &str = "翻訳する文字が見つかりません";
const WORKING: &str = "翻訳中…";

#[derive(Clone, Copy, PartialEq)]
struct Region {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Clone, Copy)]
struct Job {
    region: Region,
    live: bool,
    id: u64,
    window: usize, // the window the region belongs to, if any
}

/// How a region hangs on the window under it, so that it can go wherever the window goes.
#[derive(Clone, Copy)]
struct Anchor {
    window: HWND,
    whole: bool, // the region is the window itself; otherwise a part at (dx, dy) from its corner
    dx: i32,
    dy: i32,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static WARM_UP: AtomicBool = AtomicBool::new(false);
static UNLOAD: AtomicBool = AtomicBool::new(false);
static MOVED_AT: Mutex<Option<Instant>> = Mutex::new(None); // the last time the anchor window moved
static PAUSED: AtomicBool = AtomicBool::new(false); // the anchor window is minimised
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub fn request_load() {
    WARM_UP.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

pub fn request_unload() {
    UNLOAD.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

fn set_job(job: Option<Job>) {
    *JOB.lock().unwrap() = job;
    WAKE.notify_one();
}

fn current_job(id: u64) -> Option<Job> {
    JOB.lock().unwrap().filter(|job| job.id == id)
}

fn is_current(id: u64) -> bool {
    current_job(id).is_some()
}

// --- Worker: a watcher notices what changes on screen, however busy OCR is, and hands the newest
// frame over to the worker. So work never piles up, and stale text goes as soon as it is stale.

struct Frame {
    pixels: Vec<u8>,
    w: usize,
    h: usize,
}

struct Shared {
    frame: Mutex<Option<Frame>>, // the newest change, until the worker takes it
    fresh: Condvar,
    obsolete: AtomicBool, // the text being translated has left the screen
    done: AtomicBool,     // the current frame's translation is on show
    shown: Mutex<(Vec<u8>, Vec<Patch>)>, // what is on show, and the pixels it was made from
    working: Mutex<(Vec<u8>, Vec<Patch>)>, // the same for the text being translated
}

fn worker(overlay: usize) {
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
            Some(job) => run(job, &mut models, &mut translator, overlay),
            None => serve_requests(&mut models, &mut translator, &|| JOB.lock().unwrap().is_none()),
        }
        idle_since = Instant::now();
    }
}

/// Loads the OCR models, or reloads them if the thread setting has changed.
fn ensure_ocr(models: &mut Option<ocr::Ocr>) -> Result<(), String> {
    let config = config::get();
    let (threads, korean) = (config.threads(), config.source == "ko");
    if !models.as_ref().is_some_and(|m| m.matches(threads, korean)) {
        *models = Some(ocr::Ocr::new(threads, korean)?);
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

fn new_shared() -> Shared {
    Shared {
        frame: Mutex::new(None),
        fresh: Condvar::new(),
        obsolete: AtomicBool::new(false),
        done: AtomicBool::new(false),
        shown: Mutex::new((Vec::new(), Vec::new())),
        working: Mutex::new((Vec::new(), Vec::new())),
    }
}

fn run(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, overlay: usize) {
    let shared = new_shared();
    std::thread::scope(|scope| {
        scope.spawn(|| watch(job, overlay, &shared));
        translate_frames(job, models, translator, overlay, &shared);
    });
}

fn post_patches(overlay: usize, id: u64, patches: Vec<Patch>) {
    unsafe {
        PostMessageW(overlay as HWND, WM_PATCHES, id as usize, Box::into_raw(Box::new(patches)) as isize);
    }
}

fn translate_frames(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, overlay: usize, shared: &Shared) {
    let post = |patches: Vec<Patch>| post_patches(overlay, job.id, patches);
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
                unsafe { PostMessageW(overlay as HWND, WM_TRANSLATED, job.id as usize, 0) };
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
fn watch(job: Job, overlay: usize, shared: &Shared) {
    let post = |patches: Vec<Patch>| post_patches(overlay, job.id, patches);
    // Never OCR our own output. The last watcher may have just lifted this, and what was on show
    // takes a frame or two to leave the screen.
    unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_EXCLUDEFROMCAPTURE) };
    std::thread::sleep(POLL * 2);
    let finish = || unsafe {
        set_job(None);
        PostMessageW(overlay as HWND, WM_JOB_OVER, job.id as usize, 0);
    };
    let mut grabber: Option<capture::Grabber> = None;
    let mut size = (0, 0);
    let mut last: Vec<u8> = Vec::new();
    while let Some(current) = current_job(job.id) {
        let Region { x, y, w, h } = current.region;
        if w <= 0 || h <= 0 {
            std::thread::sleep(WATCH); // the window is off every screen for now
            continue;
        }
        if (w, h) != size {
            grabber = capture::Grabber::new(w, h);
            size = (w, h);
            last.clear();
            shared.done.store(false, Ordering::Relaxed);
            shared.obsolete.store(true, Ordering::Relaxed);
        }
        let Some(grabber) = &grabber else {
            post(vec![Patch::status("画面を取得できません", w)]);
            return finish();
        };
        if current.window != 0 && window_is_gone(current.window as HWND) {
            return finish();
        }
        let moving = MOVED_AT.lock().unwrap().is_some_and(|at| at.elapsed() < MOVE_SETTLE);
        let settled = last.is_empty() || job.live || shared.done.load(Ordering::Relaxed);
        if !moving && settled && !PAUSED.load(Ordering::Relaxed) {
            // A one-shot result stays visible to screenshots, so it leaves the capture only here.
            let trick = !job.live && !last.is_empty();
            if trick {
                unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_EXCLUDEFROMCAPTURE) };
            }
            let frame = grabber.grab(x, y);
            if trick {
                unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_NONE) };
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
        std::thread::sleep(if job.live || last.is_empty() { POLL } else { WATCH });
    }
}

/// Closed, hidden, or on another virtual desktop, which is what a cloaked window is.
fn window_is_gone(window: HWND) -> bool {
    unsafe {
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(window, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
        IsWindow(window) == 0 || (IsWindowVisible(window) == 0 && IsIconic(window) == 0) || cloaked != 0
    }
}

fn changed_pixels(a: &[u8], b: &[u8]) -> usize {
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

fn translate_frame(
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
            if block.glyphs.picture && !live { patch.keep_picture(frame, w, h) } else { patch }
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

// --- UI thread state. Cells only: window procedures re-enter while Win32 calls run.

struct Ui {
    main: Cell<HWND>,
    overlay: Cell<HWND>,
    frame: Cell<HWND>,
    selecting: Cell<Option<Region>>, // the drag so far, in screen pixels
    flashing: Cell<bool>,
    select: Cell<HWND>,
    drag_from: Cell<Option<(i32, i32)>>,
    drag_to: Cell<(i32, i32)>,
    hover: Cell<Option<(i32, i32, i32, i32)>>, // the window a click would select, in client pixels
    hover_window: Cell<HWND>,
    region: Cell<Region>,
    anchor: Cell<Option<Anchor>>,
    hook: Cell<HWINEVENTHOOK>,
    job_id: Cell<u64>, // results from older jobs are dropped
    live: Cell<bool>,
    patches: RefCell<Vec<Patch>>,
}

thread_local! {
    static UI: Ui = Ui {
        main: Cell::new(null_mut()),
        overlay: Cell::new(null_mut()),
        frame: Cell::new(null_mut()),
        selecting: Cell::new(None),
        flashing: Cell::new(false),
        select: Cell::new(null_mut()),
        drag_from: Cell::new(None),
        drag_to: Cell::new((0, 0)),
        hover: Cell::new(None),
        hover_window: Cell::new(null_mut()),
        region: Cell::new(Region { x: 0, y: 0, w: 0, h: 0 }),
        anchor: Cell::new(None),
        hook: Cell::new(null_mut()),
        job_id: Cell::new(0),
        live: Cell::new(false),
        patches: RefCell::new(Vec::new()),
    };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn lparam_xy(l: LPARAM) -> (i32, i32) {
    ((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// The model leaves memory, and a job that is running would only load it again.
pub fn unload_now() {
    stop();
    request_unload();
}

fn stop() {
    set_job(None);
    UI.with(|ui| unsafe {
        ui.live.set(false);
        ui.job_id.set(0);
        ui.flashing.set(false);
        ShowWindow(ui.overlay.get(), SW_HIDE);
        ShowWindow(ui.frame.get(), SW_HIDE);
        if !ui.hook.get().is_null() {
            UnhookWinEvent(ui.hook.replace(null_mut()));
        }
    });
}

/// Shows the frame around the region, or hides it, as the job and the settings have it.
pub fn update_frame() {
    UI.with(|ui| unsafe {
        let frame = ui.frame.get();
        let config = config::get();
        let flashing = ui.flashing.get();
        let selecting = ui.selecting.get();
        let wanted = selecting.is_some() || ui.job_id.get() != 0 && !PAUSED.load(Ordering::Relaxed) && (config.show_frame || flashing);
        let region = selecting.unwrap_or(ui.region.get());
        if !wanted || region.w <= 0 || region.h <= 0 {
            ShowWindow(frame, SW_HIDE);
            return;
        }
        let t = (if flashing { FLASH_PX } else { FRAME_PX }) * GetDpiForWindow(frame) as i32 / 96;
        let outer = RECT { left: region.x - t, top: region.y - t, right: region.x + region.w + t, bottom: region.y + region.h + t };
        let r = frame_rect(region, outer, on_its_monitor(&outer));
        SetWindowPos(frame, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        InvalidateRect(frame, null(), 0);
        UpdateWindow(frame); // a drag resizes it on every move
    });
}

/// `visible` is the part of `outer` its monitor shows. A maximised window has no outside, so the
/// frame comes in over its edge; a region across monitors keeps the whole frame.
fn frame_rect(region: Region, outer: RECT, visible: RECT) -> RECT {
    let fits = visible.left <= region.x && visible.top <= region.y && visible.right >= region.x + region.w && visible.bottom >= region.y + region.h;
    if fits { visible } else { outer }
}

unsafe fn paint_frame(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut r);
        let flashing = UI.with(|ui| ui.flashing.get());
        let t = (if flashing { FLASH_PX } else { FRAME_PX }) * GetDpiForWindow(hwnd) as i32 / 96;
        let color = if flashing { FLASH_COLOR } else { FRAME_COLOR };
        fill(hdc, &r, KEY_COLOR);
        fill(hdc, &RECT { bottom: t, ..r }, color);
        fill(hdc, &RECT { top: r.bottom - t, ..r }, color);
        fill(hdc, &RECT { right: t, ..r }, color);
        fill(hdc, &RECT { left: r.right - t, ..r }, color);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint_frame(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

fn frame_bounds(window: HWND) -> Option<RECT> {
    unsafe {
        let mut bounds: RECT = std::mem::zeroed();
        let size = size_of::<RECT>() as u32;
        let ok = DwmGetWindowAttribute(window, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut bounds as *mut _ as *mut _, size) == 0;
        ok.then_some(bounds)
    }
}

/// `bounds` cut to the monitor it mostly sits on: a window can hang off the edge.
fn on_its_monitor(bounds: &RECT) -> RECT {
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromRect(bounds, MONITOR_DEFAULTTONEAREST), &mut info);
        let mut visible: RECT = std::mem::zeroed();
        IntersectRect(&mut visible, bounds, &info.rcMonitor);
        visible
    }
}

fn anchored_region(anchor: Anchor, size: Region) -> Option<Region> {
    let bounds = frame_bounds(anchor.window)?;
    if anchor.whole {
        let r = on_its_monitor(&bounds);
        return Some(Region { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
    }
    Some(Region { x: bounds.left + anchor.dx, y: bounds.top + anchor.dy, ..size })
}

/// The window moved: the overlay goes with it at once, which needs no OCR. A new size means new
/// content, so the old text comes off and the watcher takes the job from there.
unsafe extern "system" fn on_location(_: HWINEVENTHOOK, _: u32, window: HWND, object: i32, child: i32, _: u32, _: u32) {
    if object != OBJID_WINDOW || child != 0 {
        return;
    }
    UI.with(|ui| unsafe {
        let Some(anchor) = ui.anchor.get().filter(|a| a.window == window) else { return };
        let overlay = ui.overlay.get();
        let minimised = IsIconic(window) != 0;
        PAUSED.store(minimised, Ordering::Relaxed);
        if minimised {
            ShowWindow(overlay, SW_HIDE);
            update_frame();
            return;
        }
        let old = ui.region.get();
        let Some(new) = anchored_region(anchor, old) else { return };
        *MOVED_AT.lock().unwrap() = Some(Instant::now());
        if new != old {
            ui.region.set(new);
            if let Some(job) = JOB.lock().unwrap().as_mut().filter(|job| job.id == ui.job_id.get()) {
                job.region = new;
            }
            let resized = (new.w, new.h) != (old.w, old.h);
            if resized {
                ShowWindow(overlay, SW_HIDE);
                ui.patches.borrow_mut().clear();
            } else {
                SetWindowPos(overlay, null_mut(), new.x, new.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        if !ui.patches.borrow().is_empty() {
            ShowWindow(overlay, SW_SHOWNOACTIVATE);
        }
        update_frame();
    });
}

// --- Selection layer

unsafe fn begin_select(live: bool) {
    UI.with(|ui| unsafe {
        if !ui.select.get().is_null() {
            // The hotkey again, instead of a drag: the same region as last time.
            let keep_live = live || bar::live();
            end_select();
            let region = ui.region.get();
            // The window may have gone since: what is left is the place on the screen.
            ui.anchor.set(ui.anchor.get().filter(|a| !window_is_gone(a.window) && IsIconic(a.window) == 0));
            let followed = ui.anchor.get().and_then(|anchor| anchored_region(anchor, region));
            if region.w > 0 {
                start_job(followed.unwrap_or(region), keep_live);
            }
            return;
        }
        request_load(); // ready by the time the drag ends
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let select = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            w!("ScreenTranslateSelect"),
            null(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        SetLayeredWindowAttributes(select, KEY_COLOR, DIM_ALPHA, LWA_ALPHA | LWA_COLORKEY);
        SetWindowDisplayAffinity(select, WDA_EXCLUDEFROMCAPTURE); // the first frame may race its removal
        ui.select.set(select);
        ui.drag_from.set(None);
        ui.hover.set(None);
        RegisterHotKey(ui.main.get(), HK_ESC, 0, VK_ESCAPE as u32);
        ShowWindow(select, SW_SHOWNOACTIVATE);
        update_hover(select);
        bar::show(live);
    });
}

/// The frontmost window at the point, below the selection layer, and its bounds.
unsafe fn window_at(select: HWND, at: POINT) -> Option<(HWND, RECT)> {
    unsafe {
        let mut hwnd = GetWindow(select, GW_HWNDNEXT);
        while !hwnd.is_null() {
            let click_through = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT != 0;
            let mut cloaked: u32 = 0;
            DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
            if IsWindowVisible(hwnd) != 0 && !click_through && cloaked == 0 {
                if let Some(bounds) = frame_bounds(hwnd).filter(|bounds| PtInRect(bounds, at) != 0) {
                    return Some((hwnd, bounds));
                }
            }
            hwnd = GetWindow(hwnd, GW_HWNDNEXT);
        }
        None
    }
}

/// A click with no drag selects the window under it, so that window gets a frame.
unsafe fn update_hover(select: HWND) {
    UI.with(|ui| unsafe {
        let (mut at, mut origin) = (POINT { x: 0, y: 0 }, POINT { x: 0, y: 0 });
        GetCursorPos(&mut at);
        ClientToScreen(select, &mut origin);
        let found = window_at(select, at);
        ui.hover_window.set(found.map_or(null_mut(), |(window, _)| window));
        let visible = found.map(|(_, bounds)| on_its_monitor(&bounds));
        let new = visible.map(|r| (r.left - origin.x, r.top - origin.y, r.right - origin.x, r.bottom - origin.y));
        let old = ui.hover.replace(new);
        if old != new {
            for (left, top, right, bottom) in [old, new].into_iter().flatten() {
                InvalidateRect(select, &RECT { left, top, right, bottom }, 0);
            }
        }
    });
}

unsafe fn end_select() {
    UI.with(|ui| unsafe {
        UnregisterHotKey(ui.main.get(), HK_ESC);
        let select = ui.select.replace(null_mut());
        if !select.is_null() {
            ReleaseCapture();
            DestroyWindow(select);
            bar::hide();
        }
        ui.selecting.set(None);
        update_frame();
    });
}

fn selection_rect(from: (i32, i32), to: (i32, i32)) -> RECT {
    RECT { left: from.0.min(to.0), top: from.1.min(to.1), right: from.0.max(to.0), bottom: from.1.max(to.1) }
}

unsafe fn finish_select(hwnd: HWND) {
    UI.with(|ui| unsafe {
        let Some(from) = ui.drag_from.take() else { return };
        let mut rect = selection_rect(from, ui.drag_to.get());
        let mut origin = POINT { x: 0, y: 0 };
        ClientToScreen(hwnd, &mut origin);
        let live = bar::live();
        let clicked = is_small(&rect);
        let mut anchor = None;
        if clicked {
            let Some((left, top, right, bottom)) = ui.hover.get() else { return end_select() };
            rect = RECT { left, top, right, bottom };
            anchor = Some(Anchor { window: ui.hover_window.get(), whole: true, dx: 0, dy: 0 });
        } else {
            // A part of one window moves with that window; a region across several stays put.
            let centre = POINT { x: (rect.left + rect.right) / 2 + origin.x, y: (rect.top + rect.bottom) / 2 + origin.y };
            if let Some((window, bounds)) = window_at(hwnd, centre) {
                let inside = rect.left + origin.x >= bounds.left
                    && rect.top + origin.y >= bounds.top
                    && rect.right + origin.x <= bounds.right
                    && rect.bottom + origin.y <= bounds.bottom;
                if inside {
                    anchor = Some(Anchor { window, whole: false, dx: rect.left + origin.x - bounds.left, dy: rect.top + origin.y - bounds.top });
                }
            }
        }
        end_select();
        ui.anchor.set(anchor);
        let region = Region {
            x: rect.left + origin.x,
            y: rect.top + origin.y,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        };
        start_job(region, live);
    });
}

fn is_small(rect: &RECT) -> bool {
    rect.right - rect.left < MIN_SELECTION || rect.bottom - rect.top < MIN_SELECTION
}

fn start_job(region: Region, live: bool) {
    stop();
    UI.with(|ui| unsafe {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let anchor = ui.anchor.get();
        ui.region.set(region);
        ui.job_id.set(id);
        ui.live.set(live);
        PAUSED.store(false, Ordering::Relaxed);
        if let Some(anchor) = anchor {
            let mut process = 0;
            let thread = GetWindowThreadProcessId(anchor.window, &mut process);
            ui.hook.set(SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, null_mut(), Some(on_location), process, thread, WINEVENT_OUTOFCONTEXT));
        }
        show_patches(vec![Patch::status(&t(WORKING), region.w)]);
        set_job(Some(Job { region, live, id, window: anchor.map_or(0, |a| a.window as usize) }));
        update_frame();
    });
}

unsafe extern "system" fn select_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                FillRect(hdc, &ps.rcPaint, GetStockObject(BLACK_BRUSH));
                let (from, to, hover) = UI.with(|ui| (ui.drag_from.get(), ui.drag_to.get(), ui.hover.get()));
                let dragged = from.map(|from| selection_rect(from, to)).filter(|rect| !is_small(rect));
                match (dragged, hover) {
                    (Some(rect), _) => fill(hdc, &rect, KEY_COLOR),
                    // A frame, not a clear area: clicks fall straight through the key colour.
                    (None, Some((left, top, right, bottom))) => {
                        let t = HOVER_FRAME;
                        fill(hdc, &RECT { left, top, right, bottom: top + t }, HOVER_COLOR);
                        fill(hdc, &RECT { left, top: bottom - t, right, bottom }, HOVER_COLOR);
                        fill(hdc, &RECT { left, top, right: left + t, bottom }, HOVER_COLOR);
                        fill(hdc, &RECT { left: right - t, top, right, bottom }, HOVER_COLOR);
                    }
                    (None, None) => {}
                }
                EndPaint(hwnd, &ps);
                0
            }
            WM_LBUTTONDOWN => {
                let at = lparam_xy(l);
                UI.with(|ui| {
                    ui.drag_from.set(Some(at));
                    ui.drag_to.set(at);
                });
                SetCapture(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                let at = lparam_xy(l);
                let dragging = UI.with(|ui| {
                    ui.drag_from.get().inspect(|&from| {
                        let old = selection_rect(from, ui.drag_to.replace(at));
                        if let Some((left, top, right, bottom)) = ui.hover.get().filter(|_| !is_small(&selection_rect(from, at))) {
                            InvalidateRect(hwnd, &RECT { left, top, right, bottom }, 0);
                        }
                        InvalidateRect(hwnd, &old, 0);
                        InvalidateRect(hwnd, &selection_rect(from, at), 0);
                        let rect = selection_rect(from, at);
                        let mut origin = POINT { x: 0, y: 0 };
                        ClientToScreen(hwnd, &mut origin);
                        ui.selecting.set((!is_small(&rect)).then(|| Region { x: rect.left + origin.x, y: rect.top + origin.y, w: rect.right - rect.left, h: rect.bottom - rect.top }));
                    })
                });
                if dragging.is_some() {
                    update_frame();
                }
                if dragging.is_none() {
                    update_hover(hwnd);
                }
                0
            }
            WM_LBUTTONUP => {
                UI.with(|ui| ui.drag_to.set(lparam_xy(l)));
                finish_select(hwnd);
                0
            }
            // On the way up: the layer gone, the release would open the menu of the app below.
            WM_RBUTTONUP => {
                end_select();
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

// --- Overlay: everything is the see-through key colour except the patches

unsafe fn text_font(px: i32, bold: bool) -> HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            if bold { FW_BOLD } else { FW_NORMAL } as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            w!("Yu Gothic UI"),
        )
    }
}

const TEXT_FLAGS: DRAW_TEXT_FORMAT = DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL;

/// Keeps the original font size if the text fits, spreading one line over empty background
/// first and shrinking only after that. Overhanging leading is empty, so it does not count,
/// and the patch grows only when even MIN_TEXT_PX does not fit.
unsafe fn fit(hdc: HDC, patch: &mut Patch, status_px: i32) {
    let text = wide(&patch.text);
    let measure = |px: i32, width: i32| unsafe {
        let font = text_font(px, patch.bold);
        let old = SelectObject(hdc, font);
        let mut rect = RECT { left: 0, top: 0, right: width, bottom: 0 };
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut rect, TEXT_FLAGS | DT_CALCRECT);
        SelectObject(hdc, old);
        DeleteObject(font);
        rect
    };
    let status = patch.h == 0;
    let mut px = if status { status_px } else { patch.em.max(1) };
    let smallest = MIN_TEXT_PX.min(px);
    let (size, width) = loop {
        let room = patch.h + px / 2;
        let normal = measure(px, patch.w);
        if normal.bottom <= room {
            break (normal, patch.w);
        }
        let spread = measure(px, patch.max_w);
        if spread.bottom <= room {
            break (spread, patch.w.max(spread.right + 1));
        }
        if status || px <= smallest {
            patch.h = normal.bottom;
            break (normal, patch.w);
        }
        px -= 1;
    };
    (patch.px, patch.text_h) = (px, size.bottom);
    patch.w = if status { size.right } else { width };
}

/// Lays the patches out and shows them over the region. No patches hides the overlay.
fn show_patches(mut patches: Vec<Patch>) {
    UI.with(|ui| unsafe {
        let overlay = ui.overlay.get();
        if patches.is_empty() {
            ShowWindow(overlay, SW_HIDE);
            ui.patches.borrow_mut().clear();
            return;
        }
        let region = ui.region.get();
        SetWindowPos(overlay, null_mut(), region.x, region.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let status_px = STATUS_PX * GetDpiForWindow(overlay) as i32 / 96; // after the move: per monitor
        let hdc = GetDC(overlay);
        for patch in &mut patches {
            fit(hdc, patch, status_px);
        }
        ReleaseDC(overlay, hdc);

        let height = patches.iter().map(|p| p.y + p.h).max().unwrap_or(0).max(region.h);
        let text = patches.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        SetWindowTextW(overlay, wide(&format!("{text}\0")).as_ptr()); // keeps it readable by screen readers
        *ui.patches.borrow_mut() = patches;
        // Behind a minimised window the result waits for it to come back.
        let show = if PAUSED.load(Ordering::Relaxed) { 0 } else { SWP_SHOWWINDOW };
        SetWindowPos(overlay, HWND_TOPMOST, region.x, region.y, region.w, height, SWP_NOACTIVATE | show);
        InvalidateRect(overlay, null(), 0);
    });
}

unsafe fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, rect, brush);
        DeleteObject(brush);
    }
}

/// Black around light text and white around dark, whatever the picture does behind it.
fn outline_color(fg: u32) -> u32 {
    let (r, g, b) = (fg & 0xFF, fg >> 8 & 0xFF, fg >> 16 & 0xFF);
    if r * 3 + g * 6 + b > 1275 { 0x00000000 } else { 0x00FFFFFF }
}

unsafe fn draw_backdrop(hdc: HDC, p: &Patch, b: &Backdrop) {
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = b.w;
        info.bmiHeader.biHeight = -b.h; // top-down rows
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        SetDIBitsToDevice(hdc, p.x, p.y, b.w as u32, b.h as u32, 0, 0, 0, b.h as u32, b.bgra.as_ptr() as *const _, &info, DIB_RGB_COLORS);
    }
}

unsafe fn draw_patches(hdc: HDC, patches: &[Patch]) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        for p in patches {
            match &p.backdrop {
                Some(b) => draw_backdrop(hdc, p, b),
                None => fill(hdc, &RECT { left: p.x, top: p.y, right: p.x + p.w, bottom: p.y + p.h }, p.bg),
            }
        }
        // A second pass, so no patch can paint over the text of the one before it.
        for p in patches {
            let font = text_font(p.px, p.bold);
            let old_font = SelectObject(hdc, font);
            let top = p.y + (p.h - p.text_h) / 2;
            // DT_EDITCONTROL drops a last line that overhangs, so the box must hold all of it.
            let area = RECT { left: p.x, top, right: p.x + p.w, bottom: top + p.text_h };
            let text = wide(&p.text);
            let write = |dx: i32, dy: i32, color: u32| {
                let mut at = RECT { left: area.left + dx, top: area.top + dy, right: area.right + dx, bottom: area.bottom + dy };
                SetTextColor(hdc, color);
                DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut at, TEXT_FLAGS);
            };
            if p.backdrop.is_some() {
                let reach = (p.px / OUTLINE_EVERY_PX).max(1);
                let outline = outline_color(p.fg);
                for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                    write(dx * reach, dy * reach, outline);
                }
            }
            write(0, 0, p.fg);
            SelectObject(hdc, old_font);
            DeleteObject(font);
        }
    }
}

unsafe fn paint_overlay(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        // Off screen first: drawing straight to the window would flash the original through.
        let mem = CreateCompatibleDC(hdc);
        let bitmap = CreateCompatibleBitmap(hdc, client.right, client.bottom);
        let old_bitmap = SelectObject(mem, bitmap);
        fill(mem, &client, KEY_COLOR);
        UI.with(|ui| draw_patches(mem, &ui.patches.borrow()));
        BitBlt(hdc, 0, 0, client.right, client.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bitmap);
        DeleteObject(bitmap);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint_overlay(hwnd);
                0
            }
            WM_JOB_OVER => {
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    stop();
                }
                0
            }
            WM_TRANSLATED => {
                if UI.with(|ui| ui.job_id.get()) == w as u64 && config::get().flash_frame {
                    UI.with(|ui| {
                        ui.flashing.set(true);
                        SetTimer(ui.main.get(), FLASH_TIMER, FLASH_MS, None);
                    });
                    update_frame();
                }
                0
            }
            WM_PATCHES => {
                let patches = Box::from_raw(l as *mut Vec<Patch>);
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    show_patches(*patches);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

// --- Hidden main window: hotkeys and tray

unsafe fn tray_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, wide(&t("設定")).as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, wide(&t("終了")).as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        SetForegroundWindow(hwnd); // Windows only dismisses a tray menu properly this way
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, at.x, at.y, 0, hwnd, null());
        DestroyMenu(menu);
        match chosen as usize {
            MENU_SETTINGS => settings::open(),
            MENU_EXIT => {
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

/// The icon in app.res, at the size given or Windows' size for a tray icon.
pub fn app_icon(size: Option<i32>) -> HICON {
    unsafe {
        let side = size.unwrap_or_else(|| GetSystemMetrics(SM_CXSMICON));
        LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, side, side, LR_DEFAULTCOLOR) as HICON
    }
}

/// The tray's text follows the language. The rest is written when next shown.
pub fn language_changed() {
    UI.with(|ui| unsafe { tray_icon(ui.main.get(), NIM_MODIFY) });
}

unsafe fn tray_icon(hwnd: HWND, action: u32) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = app_icon(None);
        let tip = wide(&format!("screen-translate\n{}", t("ダブルクリックで設定を開きます")));
        data.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(action, &data);
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                match w as i32 {
                    HK_ONCE => begin_select(false),
                    HK_LIVE if UI.with(|ui| ui.live.get() && ui.select.get().is_null()) => stop(),
                    HK_LIVE => begin_select(true),
                    HK_STOP => {
                        end_select();
                        stop();
                    }
                    HK_ESC => end_select(),
                    _ => {}
                }
                0
            }
            WM_TIMER if w == FLASH_TIMER => {
                KillTimer(hwnd, FLASH_TIMER);
                UI.with(|ui| ui.flashing.set(false));
                update_frame();
                0
            }
            WM_DISPLAYCHANGE => {
                stop(); // the region may no longer be where it was
                0
            }
            WM_TRAY => {
                match l as u32 {
                    WM_RBUTTONUP => tray_menu(hwnd),
                    WM_LBUTTONDBLCLK => settings::open(),
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                tray_icon(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                0
            }
            // Explorer restarted and lost every tray icon.
            _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                tray_icon(hwnd, NIM_ADD);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// Registers the saved hotkeys. Returns false if another app holds one of them.
fn register_hotkeys() -> bool {
    let main = UI.with(|ui| ui.main.get());
    let hotkeys = config::get().hotkeys;
    let mut all = true;
    for (id, hotkey) in [HK_ONCE, HK_LIVE, HK_STOP].into_iter().zip(hotkeys) {
        all &= unsafe { RegisterHotKey(main, id, hotkey.mods | MOD_NOREPEAT, hotkey.key) } != 0;
    }
    all
}

/// The settings window needs the keys in use, to let them be typed into its boxes.
pub fn pause_hotkeys() {
    let main = UI.with(|ui| ui.main.get());
    for id in [HK_ONCE, HK_LIVE, HK_STOP] {
        unsafe { UnregisterHotKey(main, id) };
    }
}

pub fn resume_hotkeys() {
    register_hotkeys();
}

/// True if the key combination is free for the taking.
pub fn try_hotkeys(hotkey: config::Hotkey) -> bool {
    let main = UI.with(|ui| ui.main.get());
    unsafe {
        let free = RegisterHotKey(main, HK_PROBE, hotkey.mods, hotkey.key) != 0;
        UnregisterHotKey(main, HK_PROBE);
        free
    }
}

unsafe fn register_class(name: *const u16, proc: WNDPROC, cursor: *const u16) {
    unsafe {
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = proc;
        class.hInstance = GetModuleHandleW(null());
        class.lpszClassName = name;
        class.hCursor = LoadCursorW(null_mut(), cursor);
        RegisterClassExW(&class);
    }
}

/// Release builds abort on a panic and have no console, so this is the only trace one leaves.
fn log_panics() {
    let path = models::dir().parent().map(|dir| dir.join("crash.log"));
    std::panic::set_hook(Box::new(move |info| {
        let Some(path) = &path else { return };
        let when = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(file, "{when}: {info}");
        }
    }));
}

/// A second start opens the settings of the first, which is usually what was wanted.
fn already_running() -> bool {
    unsafe {
        CreateMutexW(null(), 0, w!("screen-translate.single-instance"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        let first = FindWindowW(w!("ScreenTranslateMain"), null());
        PostMessageW(first, WM_TRAY, 0, WM_LBUTTONDBLCLK as isize);
        true
    }
}

fn main() {
    log_panics();
    if already_running() {
        return;
    }
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32); // WebView2 asks for it
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        register_class(w!("ScreenTranslateMain"), Some(main_proc), IDC_ARROW);
        register_class(w!("ScreenTranslateSelect"), Some(select_proc), IDC_CROSS);
        register_class(w!("ScreenTranslateOverlay"), Some(overlay_proc), IDC_ARROW);
        register_class(w!("ScreenTranslateFrame"), Some(frame_proc), IDC_ARROW);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let module = GetModuleHandleW(null());
        let main = CreateWindowExW(0, w!("ScreenTranslateMain"), w!("screen-translate"), 0, 0, 0, 0, 0, null_mut(), null_mut(), module, null());
        let overlay = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("ScreenTranslateOverlay"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        SetLayeredWindowAttributes(overlay, KEY_COLOR, 255, LWA_COLORKEY);
        let frame = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("ScreenTranslateFrame"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        SetLayeredWindowAttributes(frame, KEY_COLOR, 255, LWA_COLORKEY);
        SetWindowDisplayAffinity(frame, WDA_EXCLUDEFROMCAPTURE); // it can sit on the region's edge
        UI.with(|ui| {
            ui.main.set(main);
            ui.overlay.set(overlay);
            ui.frame.set(frame);
        });

        tray_icon(main, NIM_ADD);
        settings::register_class();
        bar::register_class();
        let hotkeys_ok = register_hotkeys();
        if !hotkeys_ok {
            let warning = wide(&t("ホットキーを登録できません。別のアプリが使っています。設定で変更してください。"));
            MessageBoxW(null_mut(), warning.as_ptr(), w!("screen-translate"), MB_OK | MB_ICONWARNING);
        }
        if !hotkeys_ok || !models::is_present(&config::get().model) {
            settings::open();
        }

        let overlay_handle = overlay as usize;
        std::thread::spawn(move || worker(overlay_handle));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_stays_on_the_screen() {
        let screen = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let around = |x, y, w, h| {
            let region = Region { x, y, w, h };
            let outer = RECT { left: x - 2, top: y - 2, right: x + w + 2, bottom: y + h + 2 };
            let visible = RECT { left: outer.left.max(0), top: outer.top.max(0), right: outer.right.min(1920), bottom: outer.bottom.min(1080) };
            frame_rect(region, outer, visible)
        };
        let r = around(100, 100, 300, 200);
        assert_eq!((r.left, r.top, r.right, r.bottom), (98, 98, 402, 302));
        let r = around(0, 0, 1920, 1080);
        assert_eq!((r.left, r.top, r.right, r.bottom), (screen.left, screen.top, screen.right, screen.bottom));
        let r = around(1800, 100, 300, 200); // into the next monitor
        assert_eq!((r.left, r.right), (1798, 2102));
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

    /// Draws the overlay onto a picture, to judge a layout without touching the real screen:
    /// RENDER_IN=in.png RENDER_OUT=out.png cargo test render_picture -- --ignored --nocapture
    #[test]
    #[ignore = "needs Ollama and a picture"]
    fn render_picture() {
        let (input, output) = (std::env::var("RENDER_IN").unwrap(), std::env::var("RENDER_OUT").unwrap());
        let (mut bgra, w, h) = ocr::png_bgra(&input);
        let region = Region { x: 0, y: 0, w: w as i32, h: h as i32 };
        let live = std::env::var("RENDER_LIVE").is_ok(); // live jobs keep the flat box
        let job = Job { region, live, id: 1, window: 0 };
        set_job(Some(job));
        let frame = Frame { pixels: bgra.clone(), w, h };
        let (mut models, mut translator) = (None, translate::Translator::new());
        let keep_going = || true;
        let shared = new_shared();
        let start = Instant::now();
        let mut patches = translate_frame(&frame, live, &mut models, &mut translator, &keep_going, &|_| {}, &shared).unwrap();
        println!("first pass {:?}", start.elapsed());
        let start = Instant::now();
        translate_frame(&frame, live, &mut models, &mut translator, &keep_going, &|_| {}, &shared).unwrap();
        println!("second pass, translations cached {:?}", start.elapsed());
        unsafe {
            let mem = CreateCompatibleDC(null_mut());
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w as i32;
            info.bmiHeader.biHeight = -(h as i32);
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut bits = null_mut();
            let dib = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            SelectObject(mem, dib);
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
            for patch in &mut patches {
                fit(mem, patch, STATUS_PX);
            }
            for p in &patches {
                println!("{},{} {}x{} em={} px={} bold={} {}", p.x, p.y, p.w, p.h, p.em, p.px, p.bold, p.text);
            }
            draw_patches(mem, &patches);
            GdiFlush();
            std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), bgra.len());
        }
        let rgb: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        let file = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
        let mut encoder = png::Encoder::new(file, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
    }
}
