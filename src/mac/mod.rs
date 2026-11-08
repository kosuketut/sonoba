//! The Mac side: a menu bar item, global hotkeys, a layer to select a region with, and the
//! overlay that shows the translation over it.
//!
//! No window here ever takes the keyboard: every one of them is a panel that refuses to become
//! the key window, and Esc is a hotkey, so the app in use stays in use throughout. The overlay
//! lets the mouse through, and the capture leaves this app's windows out, so OCR keeps seeing the
//! original text under the translation.
//!
//! Positions on screen are global points, y downwards (`sys::Area`). A job's region is in pixels:
//! points times the backing scale of the screen it was selected on, which `scale` gives.

mod bar;
mod capture;
mod draw;
mod keys;
mod settings_window;
mod sys;
mod toast;
mod views;

pub use capture::Grabber;
pub use settings_window::{close_settings, post_to_page};

use crate::config;
use crate::i18n::t;
use crate::job::*;
use crate::models;
use crate::patch::Patch;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSApplicationDidChangeScreenParametersNotification, NSFontManager, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypeString, NSScreen, NSScreenSaverWindowLevel, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::{NSDistributedNotificationCenter, NSLocale, NSNotification, NSNotificationCenter, NSOperationQueue, NSString, NSURL, ns_string};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use sys::Area;
use views::{Delegate, FrameView, OverlayView, Panel, SelectView};

pub const CAPTURE_FAILED: &str = "画面収録が許可されていません。システム設定の「プライバシーとセキュリティ」>「画面収録」で Sonoba を許可してから、Sonoba を開き直してください";

const HK_ONCE: i32 = 1;
const HK_LIVE: i32 = 2;
const HK_STOP: i32 = 3;
const HK_ESC: i32 = 4;
const HK_PROBE: i32 = 5;
const HK_PEEK: i32 = 6;
const HK_COPY: i32 = 7;
const ACTIONS: [i32; config::ACTIONS] = [HK_ONCE, HK_LIVE, HK_STOP, HK_PEEK, HK_COPY];
const ESSENTIAL_ACTIONS: usize = 3; // the app is no use without these

const MIN_SELECTION: f64 = 6.0; // points: anything smaller is a click
const STATUS_PT: f64 = 15.0;
const FRAME_PT: f64 = 2.0;
const FLASH_PT: f64 = 4.0;
const FRAME_COLOR: u32 = 0x0071CC2E; // green, as COLORREF
const FLASH_COLOR: u32 = 0x0088FF39;
const FLASH: Duration = Duration::from_millis(450);
const FOLLOW_EVERY: Duration = Duration::from_millis(50); // how often a window's place is checked
const DIM_ALPHA: f64 = 110.0 / 255.0;
const HOVER_PT: f64 = 4.0;
const OPEN_SETTINGS: &str = "jp.sonoba.open-settings";
const AGENT: &str = "jp.sonoba.app";

/// The windows of the overlay sit above everything, the selection layer above them, the bar on top.
fn level(above: isize) -> isize {
    NSScreenSaverWindowLevel + above
}

/// Actions whose key another app holds, for the settings page.
pub static TAKEN_KEYS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The scale of the running job: pixels to a point on the screen it was selected on.
static SCALE: AtomicU64 = AtomicU64::new(2.0f64.to_bits());

pub fn scale() -> f64 {
    f64::from_bits(SCALE.load(Ordering::Relaxed))
}

fn to_region(area: &Area, scale: f64) -> Region {
    let px = |v: f64| (v * scale).round() as i32;
    Region { x: px(area.x), y: px(area.y), w: px(area.w), h: px(area.h) }
}

fn to_area(region: Region, scale: f64) -> Area {
    Area { x: region.x as f64 / scale, y: region.y as f64 / scale, w: region.w as f64 / scale, h: region.h as f64 / scale }
}

/// The backing scale of the screen at the point, in global points.
fn scale_at(at: (f64, f64), mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let found = screens.iter().find(|s| sys::from_cocoa(s.frame()).contains(at)).or_else(|| screens.firstObject());
    found.map_or(2.0, |s| s.backingScaleFactor())
}

/// How a region hangs on the window under it, so that it can go wherever the window goes.
#[derive(Clone, Copy)]
struct Anchor {
    window: u32,
    whole: bool, // the region is the window itself; otherwise a part at (dx, dy) from its corner
    dx: f64,
    dy: f64,
}

struct Ui {
    mtm: MainThreadMarker,
    delegate: Retained<Delegate>,
    overlay: Retained<Panel>,
    overlay_view: Retained<OverlayView>,
    frame: Retained<Panel>,
    frame_view: Retained<FrameView>,
    status: RefCell<Option<Retained<NSStatusItem>>>,
    selects: RefCell<Vec<(Retained<Panel>, Retained<SelectView>)>>,
    drag_from: Cell<Option<(f64, f64)>>,
    selecting: Cell<Option<Area>>, // the drag so far
    hover: Cell<Option<(u32, Area)>>, // the window a click would select, and what of it shows
    shift_seen: Cell<bool>,           // Shift was down during the drag in progress
    peeking: Cell<bool>,              // the original is on show while a key is held
    resume: Cell<Option<Region>>,     // the live job to start again after a pause
    flashing: Cell<u64>,              // the flash in progress, by number; 0 for none
    region: Cell<Region>,
    anchor: Cell<Option<Anchor>>,
    job_id: Cell<u64>, // results from older jobs are dropped
    live: Cell<bool>,
    patches: RefCell<Vec<Patch>>,
    held: RefCell<HashSet<i32>>, // hotkeys down, so that a key held does not repeat its action
}

thread_local! {
    static UI: OnceCell<Ui> = const { OnceCell::new() };
}

fn with_ui<R>(work: impl FnOnce(&Ui) -> R) -> R {
    UI.with(|ui| work(ui.get().expect("the UI exists from the start")))
}

pub fn delegate() -> Retained<Delegate> {
    with_ui(|ui| ui.delegate.clone())
}

// --- What the worker and the settings page call

fn on_main(work: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(work);
}

fn after(wait: Duration, work: impl FnOnce() + Send + 'static) {
    let when = DispatchTime::try_from(wait).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, work);
}

fn is_job(id: u64) -> bool {
    with_ui(|ui| ui.job_id.get() == id)
}

pub fn post_patches(id: u64, patches: Vec<Patch>) {
    on_main(move || {
        if is_job(id) {
            show_patches(patches);
        }
    });
}

pub fn post_translated(id: u64) {
    on_main(move || {
        if is_job(id) && config::get().flash_frame {
            flash();
        }
    });
}

pub fn post_job_over(id: u64) {
    on_main(move || {
        if is_job(id) {
            stop();
        }
    });
}

pub fn post_parts(id: u64) {
    on_main(move || {
        if is_job(id) {
            let live = PARTS.lock().unwrap().iter().any(|p| p.live);
            with_ui(|ui| {
                ui.live.set(live);
                ui.frame_view.setNeedsDisplay(true);
            });
        }
    });
}

/// The capture never sees this app's windows, so there is nothing to hide.
pub fn hide_overlay_from_capture(_hidden: bool) {}

/// Closed. A window that is minimised, hidden, or on another space only pauses the job.
pub fn window_is_gone(window: usize) -> bool {
    sys::window(window as u32).is_none()
}

/// Japanese if the Mac's first language is, English otherwise.
pub fn system_language() -> &'static str {
    let first = NSLocale::preferredLanguages().firstObject().map(|l| l.to_string()).unwrap_or_default();
    if first.starts_with("ja") { "ja" } else { "en" }
}

/// The model leaves memory, and a job that is running would only load it again.
pub fn unload_now() {
    stop();
    request_unload();
}

pub fn language_changed() {
    update_status_menu();
}

pub fn fonts() -> Vec<String> {
    let mtm = with_ui(|ui| ui.mtm);
    let families = NSFontManager::sharedFontManager(mtm).availableFontFamilies();
    let mut list: Vec<String> = families.iter().map(|f| f.to_string()).filter(|f| !f.starts_with('.')).collect();
    list.sort();
    list
}

pub fn open_link(url: &str) {
    if url.starts_with("https://")
        && let Some(url) = NSURL::URLWithString(&NSString::from_str(url))
    {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

pub fn copy_to_clipboard(text: &str) -> bool {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
}

// --- Starting at login: a launch agent, which needs no permission and only affects this user

fn agent_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/LaunchAgents").join(format!("{AGENT}.plist"))
}

pub fn autostart() -> bool {
    agent_path().exists()
}

pub fn set_autostart(on: bool) {
    let path = agent_path();
    if !on {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
"#,
        escape(&exe.display().to_string())
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, plist);
}

// --- Jobs

fn stop() {
    set_job(None);
    with_ui(|ui| {
        ui.live.set(false);
        ui.job_id.set(0);
        ui.flashing.set(0);
        ui.peeking.set(false);
        ui.patches.borrow_mut().clear();
        ui.overlay.orderOut(None);
        ui.frame.orderOut(None);
    });
}

fn start_job(region: Region, live: bool) {
    stop();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let anchor = with_ui(|ui| {
        ui.region.set(region);
        ui.job_id.set(id);
        // With several regions the job runs as live, and keep_parts holds the one-shot ones still.
        let parts = PARTS.lock().unwrap().clone();
        ui.live.set(if parts.is_empty() { live } else { parts.iter().any(|p| p.live) });
        ui.anchor.get()
    });
    let live = live || !PARTS.lock().unwrap().is_empty();
    PAUSED.store(false, Ordering::Relaxed);
    show_patches(vec![Patch::status(&t(WORKING), region.w)]);
    set_job(Some(Job { region, live, id, window: anchor.map_or(0, |a| a.window as usize) }));
    update_frame();
    if anchor.is_some() {
        follow(id);
    }
}

/// `area` cut to the display it mostly sits on: a window can hang off the edge.
fn on_its_display(area: &Area) -> Area {
    sys::display_at(area.centre()).map_or(*area, |d| area.intersect(&d.bounds))
}

fn anchored_region(anchor: Anchor, size: Region, bounds: &Area) -> Region {
    let scale = scale();
    if anchor.whole {
        return to_region(&on_its_display(bounds), scale);
    }
    let corner = to_region(&Area { x: bounds.x + anchor.dx, y: bounds.y + anchor.dy, w: 0.0, h: 0.0 }, scale);
    Region { x: corner.x, y: corner.y, ..size }
}

/// The overlay goes wherever the anchor window goes, which needs no OCR. A new size means new
/// content, so the old text comes off and the watcher takes the job from there. Nothing tells an
/// app that another app's window moved, so this looks every so often.
fn follow(id: u64) {
    if !is_job(id) {
        return;
    }
    if let Some(anchor) = with_ui(|ui| ui.anchor.get())
        && let Some(window) = sys::window(anchor.window)
    {
        with_ui(|ui| {
            let hidden = !window.on_screen;
            let was_hidden = PAUSED.swap(hidden, Ordering::Relaxed);
            if hidden {
                if !was_hidden {
                    ui.overlay.orderOut(None);
                    ui.frame.orderOut(None);
                }
                return;
            }
            let old = ui.region.get();
            let new = anchored_region(anchor, old, &window.bounds);
            let moved = new != old;
            if moved {
                *MOVED_AT.lock().unwrap() = Some(Instant::now());
                ui.region.set(new);
                if let Some(job) = JOB.lock().unwrap().as_mut().filter(|job| job.id == id) {
                    job.region = new;
                }
                if (new.w, new.h) != (old.w, old.h) {
                    ui.overlay.orderOut(None);
                    ui.patches.borrow_mut().clear();
                } else {
                    let frame = ui.overlay.frame();
                    let at = sys::to_cocoa(&Area { h: frame.size.height, ..to_area(new, scale()) });
                    ui.overlay.setFrameOrigin(at.origin);
                }
            }
            if was_hidden && !ui.patches.borrow().is_empty() && !ui.peeking.get() {
                ui.overlay.orderFrontRegardless();
            }
            if moved || was_hidden {
                update_frame();
            }
        });
    }
    after(FOLLOW_EVERY, move || follow(id));
}

// --- The frame around the region

/// Shows the frame around the region, or hides it, as the job and the settings have it.
pub fn update_frame() {
    with_ui(|ui| {
        let config = config::get();
        let flashing = ui.flashing.get() != 0;
        let selecting = ui.selecting.get();
        let wanted = selecting.is_some() || ui.job_id.get() != 0 && !PAUSED.load(Ordering::Relaxed) && (config.show_frame || flashing);
        let area = selecting.unwrap_or_else(|| to_area(ui.region.get(), scale()));
        if !wanted || area.w <= 0.0 || area.h <= 0.0 {
            ui.frame.orderOut(None);
            return;
        }
        let t = if flashing { FLASH_PT } else { FRAME_PT };
        let outer = area.grown(t);
        // A maximised window has no outside, so the frame comes in over its edge.
        let visible = on_its_display(&outer);
        let shown = if visible.holds(&area) { visible } else { outer };
        ui.frame.setFrame_display(sys::to_cocoa(&shown), true);
        ui.frame_view.setNeedsDisplay(true);
        ui.frame.orderFrontRegardless();
    });
}

fn paint_frame(view: &FrameView) {
    with_ui(|ui| {
        let flashing = ui.flashing.get() != 0;
        let t = if flashing { FLASH_PT } else { FRAME_PT };
        let color = if flashing { FLASH_COLOR } else { FRAME_COLOR };
        let size = view.bounds().size;
        let corner = sys::from_cocoa(ui.frame.frame());
        let parts = if ui.selecting.get().is_some() { Vec::new() } else { PARTS.lock().unwrap().clone() };
        let boxes: Vec<Area> = if parts.is_empty() {
            vec![Area { x: 0.0, y: 0.0, w: size.width, h: size.height }]
        } else {
            parts.iter().map(|p| to_area(p.region, scale()).grown(t)).map(|a| Area { x: a.x - corner.x, y: a.y - corner.y, ..a }).collect()
        };
        for b in boxes {
            for side in [Area { h: t, ..b }, Area { y: b.bottom() - t, h: t, ..b }, Area { w: t, ..b }, Area { x: b.right() - t, w: t, ..b }] {
                draw::fill(side.cg(), color);
            }
        }
    });
}

/// The frame for a moment, thicker, whenever a translation finishes.
fn flash() {
    let number = with_ui(|ui| {
        let number = ui.flashing.get() + 1;
        ui.flashing.set(number);
        number
    });
    update_frame();
    after(FLASH, move || {
        if with_ui(|ui| ui.flashing.get() == number) {
            with_ui(|ui| ui.flashing.set(0));
            update_frame();
        }
    });
}

// --- The overlay

/// Lays the patches out and shows them over the region. No patches hides the overlay.
fn show_patches(mut patches: Vec<Patch>) {
    with_ui(|ui| {
        if patches.is_empty() {
            ui.overlay.orderOut(None);
            ui.patches.borrow_mut().clear();
            return;
        }
        let region = ui.region.get();
        let scale = scale();
        let status_px = (STATUS_PT * scale).round() as i32;
        for patch in &mut patches {
            draw::fit(patch, status_px);
        }
        let height = patches.iter().map(|p| p.y + p.h).max().unwrap_or(0).max(region.h);
        let text = patches.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        ui.overlay.setTitle(&NSString::from_str(&text)); // keeps it readable by VoiceOver
        *ui.patches.borrow_mut() = patches;
        ui.overlay.setFrame_display(sys::to_cocoa(&to_area(Region { h: height, ..region }, scale)), false);
        ui.overlay_view.setNeedsDisplay(true);
        // Behind a minimised window the result waits for it to come back.
        if !PAUSED.load(Ordering::Relaxed) && !ui.peeking.get() {
            ui.overlay.orderFrontRegardless();
        }
    });
}

fn paint_overlay() {
    with_ui(|ui| draw::draw(&ui.patches.borrow(), scale()));
}

/// Applies the font and strength from the settings to what is on show.
pub fn look_changed() {
    let patches = with_ui(|ui| {
        ui.overlay.setAlphaValue(config::get().opacity as f64 / 100.0);
        ui.patches.borrow().clone()
    });
    if !patches.is_empty() {
        show_patches(patches);
    }
}

/// The original comes back for as long as the key is held.
fn peek_start() {
    with_ui(|ui| {
        if ui.job_id.get() != 0 {
            ui.peeking.set(true);
            ui.overlay.orderOut(None);
        }
    });
}

fn peek_end() {
    with_ui(|ui| {
        if ui.peeking.replace(false) && !ui.patches.borrow().is_empty() && !PAUSED.load(Ordering::Relaxed) {
            ui.overlay.orderFrontRegardless();
        }
    });
}

/// Says how it went beside the cursor: nothing else on screen changes.
fn copy_translation() {
    let text = with_ui(|ui| ui.patches.borrow().iter().filter(|p| !p.source.is_empty()).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n"));
    let result = if text.is_empty() {
        "コピーする訳文がありません"
    } else if copy_to_clipboard(&text) {
        "訳文をコピーしました"
    } else {
        "クリップボードが使えません"
    };
    toast::show(&t(result));
}

/// Stops everything for a while, keys included, for when they clash with what is on screen. A
/// live job picks up again afterwards.
fn toggle_pause() {
    let pausing = !SUSPENDED.fetch_xor(true, Ordering::Relaxed);
    if pausing {
        end_select();
        with_ui(|ui| ui.resume.set(ui.live.get().then(|| ui.region.get())));
        stop();
        pause_hotkeys();
    } else {
        register_hotkeys();
        if let Some(region) = with_ui(|ui| ui.resume.take()) {
            start_job(region, true);
        }
    }
    update_status_menu();
}

// --- The selection layer

fn is_selecting() -> bool {
    with_ui(|ui| !ui.selects.borrow().is_empty())
}

fn begin_select(live: bool) {
    if !config::get().license_accepted {
        return settings_window::open(); // which asks
    }
    if is_selecting() {
        // The hotkey again, instead of a drag: the same region as last time. For what is
        // already live that can only mean enough of it.
        let keep_live = live || bar::live();
        end_select();
        if live && with_ui(|ui| ui.live.get()) {
            return stop();
        }
        let (region, anchor) = with_ui(|ui| {
            // The window may have gone since: what is left is the place on the screen.
            let anchor = ui.anchor.get().and_then(|a| sys::window(a.window).filter(|w| w.on_screen).map(|w| (a, w)));
            ui.anchor.set(anchor.as_ref().map(|(a, _)| *a));
            (ui.region.get(), anchor)
        });
        let followed = anchor.map(|(a, w)| anchored_region(a, region, &w.bounds));
        if region.w > 0 {
            start_job(followed.unwrap_or(region), keep_live);
        }
        return;
    }
    request_load(); // ready by the time the drag ends
    with_ui(|ui| {
        ui.drag_from.set(None);
        ui.hover.set(None);
        let mut layers = Vec::new();
        for screen in NSScreen::screens(ui.mtm).iter() {
            let area = sys::from_cocoa(screen.frame());
            let view = SelectView::new(ui.mtm, area);
            let panel = Panel::new(ui.mtm, &area, level(1), &view, false);
            panel.orderFrontRegardless();
            layers.push((panel, view));
        }
        *ui.selects.borrow_mut() = layers;
    });
    keys::register_escape(HK_ESC);
    update_hover();
    bar::show(live, with_ui(|ui| ui.job_id.get() != 0));
}

fn end_select() {
    keys::unregister(HK_ESC);
    let layers = with_ui(|ui| ui.selects.take());
    if !layers.is_empty() {
        for (panel, _) in &layers {
            panel.orderOut(None);
        }
        bar::hide();
    }
    with_ui(|ui| {
        ui.selecting.set(None);
        ui.drag_from.set(None);
    });
    update_frame();
}

fn redraw_selection() {
    with_ui(|ui| {
        for (_, view) in ui.selects.borrow().iter() {
            view.setNeedsDisplay(true);
        }
    });
}

/// A click with no drag selects the window under it, so that window gets a frame.
fn update_hover() {
    let found = sys::window_at(sys::mouse()).map(|w| (w.id, on_its_display(&w.bounds)));
    let changed = with_ui(|ui| ui.hover.replace(found) != found);
    if changed {
        redraw_selection();
    }
}

fn select_down() {
    with_ui(|ui| {
        ui.drag_from.set(Some(sys::mouse()));
        ui.shift_seen.set(keys::shift_down());
    });
}

fn is_small(area: &Area) -> bool {
    area.w < MIN_SELECTION || area.h < MIN_SELECTION
}

fn select_drag() {
    let dragging = with_ui(|ui| {
        let from = ui.drag_from.get()?;
        ui.shift_seen.set(ui.shift_seen.get() || keys::shift_down());
        let area = Area::from_corners(from, sys::mouse());
        ui.selecting.set((!is_small(&area)).then_some(area));
        Some(())
    });
    if dragging.is_some() {
        redraw_selection();
        update_frame();
    }
}

fn paint_select(screen: &Area) {
    let (dragged, hover) = with_ui(|ui| (ui.selecting.get(), ui.hover.get()));
    let local = |a: &Area| Area { x: a.x - screen.x, y: a.y - screen.y, ..*a };
    let whole = Area { x: 0.0, y: 0.0, w: screen.w, h: screen.h };
    objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, DIM_ALPHA).setFill();
    objc2_app_kit::NSBezierPath::fillRect(whole.cg());
    match (dragged, hover) {
        (Some(area), _) => objc2_app_kit::NSRectFillUsingOperation(local(&area).cg(), objc2_app_kit::NSCompositingOperation::Clear),
        (None, Some((_, area))) => {
            let b = local(&area);
            let t = HOVER_PT;
            for side in [Area { h: t, ..b }, Area { y: b.bottom() - t, h: t, ..b }, Area { w: t, ..b }, Area { x: b.right() - t, w: t, ..b }] {
                draw::fill(side.cg(), 0x00FFFFFF);
            }
        }
        (None, None) => {}
    }
}

fn finish_select() {
    let Some(from) = with_ui(|ui| ui.drag_from.take()) else { return };
    let mut area = Area::from_corners(from, sys::mouse());
    let live = bar::live();
    // Shift adds to what is running. A job with several regions stays where it is on screen.
    let adding = with_ui(|ui| (ui.shift_seen.get() || keys::shift_down()) && ui.job_id.get() != 0);
    let mut anchor = None;
    if is_small(&area) {
        let Some((window, visible)) = with_ui(|ui| ui.hover.get()) else { return end_select() };
        area = visible;
        anchor = Some(Anchor { window, whole: true, dx: 0.0, dy: 0.0 });
    } else if let Some(window) = sys::window_at(area.centre()).filter(|w| w.bounds.holds(&area)) {
        // A part of one window moves with that window; a region across several stays put.
        anchor = Some(Anchor { window: window.id, whole: false, dx: area.x - window.bounds.x, dy: area.y - window.bounds.y });
    }
    end_select();
    if adding {
        let region = to_region(&area, scale());
        let mut parts = PARTS.lock().unwrap().clone();
        if parts.is_empty() {
            parts.push(with_ui(|ui| Part { region: ui.region.get(), live: ui.live.get() }));
        }
        parts.push(Part { region, live });
        let around = around_all(&parts);
        *PARTS.lock().unwrap() = parts;
        with_ui(|ui| ui.anchor.set(None));
        start_job(around, live);
    } else {
        let mtm = with_ui(|ui| ui.mtm);
        SCALE.store(scale_at(area.centre(), mtm).to_bits(), Ordering::Relaxed);
        PARTS.lock().unwrap().clear();
        with_ui(|ui| ui.anchor.set(anchor));
        start_job(to_region(&area, scale()), live);
    }
}

// --- Hotkeys

fn on_hotkey(id: i32, pressed: bool) {
    if !pressed {
        with_ui(|ui| ui.held.borrow_mut().remove(&id));
        if id == HK_PEEK {
            peek_end();
        }
        return;
    }
    // A key held down repeats; only the first press counts. Esc goes away while it is down.
    if id != HK_ESC && !with_ui(|ui| ui.held.borrow_mut().insert(id)) {
        return;
    }
    match id {
        HK_ONCE => begin_select(false),
        HK_LIVE => begin_select(true),
        HK_STOP => {
            end_select();
            stop();
        }
        HK_ESC => end_select(),
        HK_PEEK => peek_start(),
        HK_COPY => copy_translation(),
        _ => {}
    }
}

/// Registers the saved hotkeys. Returns false if another app holds one of the essential ones.
fn register_hotkeys() -> bool {
    if SUSPENDED.load(Ordering::Relaxed) {
        return true;
    }
    let mut mouse = Vec::new();
    let mut taken = Vec::new();
    for (index, (id, hotkey)) in ACTIONS.into_iter().zip(config::get().hotkeys).enumerate() {
        if hotkey == config::Hotkey::OFF {
            continue;
        }
        if hotkey.is_mouse() {
            mouse.push((id, hotkey));
        } else if !keys::register(id, hotkey) {
            taken.push(index);
        }
    }
    keys::set_mouse_keys(mouse, on_hotkey);
    // The extras are conveniences: one that is taken shows in the settings and stops nothing.
    let all = !taken.iter().any(|&index| index < ESSENTIAL_ACTIONS);
    *TAKEN_KEYS.lock().unwrap() = taken;
    all
}

/// Choosing a key in the settings needs the keys free, so that they can be typed.
pub fn pause_hotkeys() {
    for id in ACTIONS {
        keys::unregister(id);
    }
    keys::set_mouse_keys(Vec::new(), on_hotkey);
    with_ui(|ui| ui.held.borrow_mut().clear());
}

pub fn resume_hotkeys() {
    pause_hotkeys(); // registering a key twice would look like another app holding it
    register_hotkeys();
}

/// True if the key combination is free for the taking.
pub fn try_hotkeys(hotkey: config::Hotkey) -> bool {
    keys::is_free(HK_PROBE, hotkey)
}

// --- The menu bar

fn menu_item(title: &str, action: objc2::runtime::Sel, key: &str, target: Option<&AnyObject>, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key)) };
    unsafe { item.setTarget(target) };
    item
}

fn update_status_menu() {
    with_ui(|ui| {
        let mtm = ui.mtm;
        let menu = NSMenu::new(mtm);
        let delegate: &AnyObject = &ui.delegate;
        menu.addItem(&menu_item(&format!("{}…", t("設定")), sel!(openSettings:), ",", Some(delegate), mtm));
        let pause = menu_item(&t("一時停止"), sel!(togglePause:), "", Some(delegate), mtm);
        pause.setState(if SUSPENDED.load(Ordering::Relaxed) { 1 } else { 0 });
        menu.addItem(&pause);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let app: Retained<AnyObject> = NSApplication::sharedApplication(mtm).into_super().into_super().into_super();
        menu.addItem(&menu_item(&t("終了"), sel!(terminate:), "q", Some(&app), mtm));
        let mut status = ui.status.borrow_mut();
        let item = status.get_or_insert_with(|| NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength));
        if let Some(button) = item.button(mtm) {
            let image = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("translate"), Some(ns_string!("Sonoba")));
            match image {
                Some(image) => {
                    image.setTemplate(true);
                    button.setImage(Some(&image));
                }
                None => button.setTitle(ns_string!("Sonoba")),
            }
            let state = if SUSPENDED.load(Ordering::Relaxed) { t("一時停止中") } else { "Sonoba".to_string() };
            button.setToolTip(Some(&NSString::from_str(&state)));
        }
        item.setMenu(Some(&menu));
    });
}

/// The menus an app needs for the keyboard shortcuts of text fields: the settings page has some.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let main = NSMenu::new(mtm);
    let submenu = |title: &str, items: &[(&str, objc2::runtime::Sel, &str)]| {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for (name, action, key) in items {
            menu.addItem(&menu_item(name, *action, key, None, mtm));
        }
        let item = NSMenuItem::new(mtm);
        item.setSubmenu(Some(&menu));
        main.addItem(&item);
    };
    submenu("Sonoba", &[("Quit Sonoba", sel!(terminate:), "q")]);
    submenu(
        "Edit",
        &[
            ("Undo", sel!(undo:), "z"),
            ("Redo", sel!(redo:), "Z"),
            ("Cut", sel!(cut:), "x"),
            ("Copy", sel!(copy:), "c"),
            ("Paste", sel!(paste:), "v"),
            ("Select All", sel!(selectAll:), "a"),
        ],
    );
    submenu("Window", &[("Close", sel!(performClose:), "w"), ("Minimize", sel!(performMiniaturize:), "m")]);
    main
}

// --- Starting up

/// A second start opens the settings of the first, which is usually what was wanted. The lock
/// is the open file, so it goes with the app however the app ends.
fn single_instance() -> Option<std::fs::File> {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let dir = models::dir().parent().map(PathBuf::from).unwrap_or_default();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock")) else { return Some(tempfile_fallback()) };
    use std::os::fd::AsRawFd;
    (unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0).then_some(file)
}

/// Without a folder to lock in, the app runs rather than not at all.
fn tempfile_fallback() -> std::fs::File {
    std::fs::File::open("/dev/null").expect("/dev/null")
}

fn ask_first_to_open_settings() {
    let center = NSDistributedNotificationCenter::defaultCenter();
    unsafe { center.postNotificationName_object_userInfo_deliverImmediately(&NSString::from_str(OPEN_SETTINGS), None, None, true) };
}

fn observe(center: &NSNotificationCenter, name: &NSString, work: fn()) {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| work());
    let observer = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, Some(&NSOperationQueue::mainQueue()), &block) };
    std::mem::forget(observer); // for as long as the app runs
}

/// Brings the app forward. `activate` only came with macOS 14, and the older call still works there.
#[allow(deprecated)]
fn activate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn warn(text: &str) {
    let mtm = with_ui(|ui| ui.mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Sonoba"));
    alert.setInformativeText(&NSString::from_str(text));
    activate(mtm);
    alert.runModal();
}

pub fn main() {
    crate::log_panics();
    let mtm = MainThreadMarker::new().expect("the app starts on the main thread");
    let Some(_lock) = single_instance() else {
        ask_first_to_open_settings();
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.setMainMenu(Some(&main_menu(mtm)));

    let overlay_view: Retained<OverlayView> = views::plain(mtm);
    let overlay = Panel::new(mtm, &Area::default(), level(0), &overlay_view, true);
    let frame_view: Retained<FrameView> = views::plain(mtm);
    let frame = Panel::new(mtm, &Area::default(), level(0), &frame_view, true);
    let ui = Ui {
        mtm,
        delegate: Delegate::new(mtm),
        overlay,
        overlay_view,
        frame,
        frame_view,
        status: RefCell::new(None),
        selects: RefCell::new(Vec::new()),
        drag_from: Cell::new(None),
        selecting: Cell::new(None),
        hover: Cell::new(None),
        shift_seen: Cell::new(false),
        peeking: Cell::new(false),
        resume: Cell::new(None),
        flashing: Cell::new(0),
        region: Cell::new(Region { x: 0, y: 0, w: 0, h: 0 }),
        anchor: Cell::new(None),
        job_id: Cell::new(0),
        live: Cell::new(false),
        patches: RefCell::new(Vec::new()),
        held: RefCell::new(HashSet::new()),
    };
    UI.with(|cell| {
        let _ = cell.set(ui);
    });
    look_changed();
    update_status_menu();

    sys::listen_for_hotkeys(on_hotkey);
    // The region may no longer be where it was.
    observe(&NSNotificationCenter::defaultCenter(), unsafe { NSApplicationDidChangeScreenParametersNotification }, stop);
    observe(&NSDistributedNotificationCenter::defaultCenter(), &NSString::from_str(OPEN_SETTINGS), settings_window::open);
    std::thread::spawn(worker);

    let hotkeys_ok = register_hotkeys();
    let config = config::get();
    on_main(move || {
        if !hotkeys_ok {
            warn(&t("ホットキーを登録できません。別のアプリが使っています。設定で変更してください。"));
        }
        if !hotkeys_ok || !config.license_accepted || !models::is_present(&config.model) {
            settings_window::open();
        }
    });
    if let Ok(region) = std::env::var("SONOBA_REGION") {
        // For trying the app out without a mouse: "x,y,w,h" in points, and "live" to keep watching.
        let n: Vec<f64> = region.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if let [x, y, w, h] = n[..] {
            let area = Area { x, y, w, h };
            SCALE.store(scale_at(area.centre(), mtm).to_bits(), Ordering::Relaxed);
            let live = std::env::var_os("SONOBA_LIVE").is_some();
            on_main(move || start_job(to_region(&area, scale()), live));
        }
    }
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_and_areas_convert_both_ways() {
        let area = Area { x: 10.5, y: 20.0, w: 100.0, h: 30.25 };
        let region = to_region(&area, 2.0);
        assert_eq!(region, Region { x: 21, y: 40, w: 200, h: 61 });
        assert_eq!(to_area(Region { x: 20, y: 40, w: 200, h: 60 }, 2.0), Area { x: 10.0, y: 20.0, w: 100.0, h: 30.0 });
    }

    #[test]
    fn a_region_on_a_window_keeps_its_place_on_it() {
        SCALE.store(2.0f64.to_bits(), Ordering::Relaxed);
        let anchor = Anchor { window: 1, whole: false, dx: 15.0, dy: 30.0 };
        let size = Region { x: 0, y: 0, w: 400, h: 80 };
        let moved = anchored_region(anchor, size, &Area { x: 100.0, y: 200.0, w: 800.0, h: 600.0 });
        assert_eq!(moved, Region { x: 230, y: 460, w: 400, h: 80 });
    }
}
