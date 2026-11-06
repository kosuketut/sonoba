//! Screen OCR and English to Japanese translation, driven by global hotkeys.
//!
//! No window here ever takes the keyboard focus: the selection layer and the overlay are
//! WS_EX_NOACTIVATE and Esc is a hotkey, so the active window stays active throughout.
//! The overlay also lets the mouse through, and capture skips it, so OCR keeps seeing the
//! original text underneath the translation.
#![windows_subsystem = "windows"]

mod capture;
mod ocr;
mod patch;
mod translate;

use patch::{KEY_COLOR, Patch};
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const POLL: Duration = Duration::from_millis(150);
const PARTIAL_AFTER: Duration = Duration::from_millis(300); // show finished paragraphs of long pages
const OCR_IDLE: Duration = Duration::from_secs(60); // free the OCR models this long after the last job
const MIN_SELECTION: i32 = 8;
const MIN_TEXT_PX: i32 = 11; // below this Japanese stops being legible; the patch grows instead
const STATUS_PX: i32 = 16;

const HK_ONCE: i32 = 1;
const HK_LIVE: i32 = 2;
const HK_STOP: i32 = 3;
const HK_ESC: i32 = 4;
const WM_PATCHES: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const MENU_EXIT: usize = 1;

const DIM_ALPHA: u8 = 110;

const NOT_FOUND: &str = "翻訳する文字が見つかりません";
const WORKING: &str = "翻訳中…";

#[derive(Clone, Copy)]
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
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn set_job(job: Option<Job>) {
    *JOB.lock().unwrap() = job;
    WAKE.notify_one();
}

fn is_current(id: u64) -> bool {
    JOB.lock().unwrap().is_some_and(|j| j.id == id)
}

// --- Worker: capture, OCR, translate. One thread, so it can never fall behind the screen.

fn worker(overlay: usize) {
    let mut models: Option<ocr::Ocr> = None;
    let mut translator = translate::Translator::new();
    loop {
        let mut job = JOB.lock().unwrap();
        while job.is_none() {
            let (guard, wait) = WAKE.wait_timeout(job, OCR_IDLE).unwrap();
            job = guard;
            if wait.timed_out() {
                models = None;
            }
        }
        let current = job.unwrap();
        drop(job);
        run(current, &mut models, &mut translator, overlay);
    }
}

fn run(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, overlay: usize) {
    let post = |patches: Vec<Patch>| unsafe {
        PostMessageW(overlay as HWND, WM_PATCHES, job.id as usize, Box::into_raw(Box::new(patches)) as isize);
    };
    let Region { x, y, w, h } = job.region;
    let Some(grabber) = capture::Grabber::new(w, h) else {
        post(vec![Patch::status("画面を取得できません", w)]);
        return set_job(None);
    };
    let mut last: Vec<u8> = Vec::new();
    let mut shown = None;
    while is_current(job.id) {
        let frame = grabber.grab(x, y);
        if frame != last {
            last.clear();
            last.extend_from_slice(frame);
            let result = translate_frame(frame, job, models, translator, &post);
            let patches = result.unwrap_or_else(|e| vec![Patch::status(&format!("エラー: {e}"), w)]);
            if is_current(job.id) && shown.as_ref() != Some(&patches) {
                post(patches.clone());
                shown = Some(patches);
            }
        }
        if !job.live {
            let mut current = JOB.lock().unwrap();
            if current.is_some_and(|j| j.id == job.id) {
                *current = None;
            }
            return;
        }
        std::thread::sleep(POLL);
    }
}

fn translate_frame(
    frame: &[u8],
    job: Job,
    models: &mut Option<ocr::Ocr>,
    translator: &mut translate::Translator,
    post: &impl Fn(Vec<Patch>),
) -> Result<Vec<Patch>, String> {
    if models.is_none() {
        *models = Some(ocr::Ocr::new()?);
    }
    let (w, h) = (job.region.w as usize, job.region.h as usize);
    let lines = models.as_ref().unwrap().read(frame, w, h)?;
    let blocks = ocr::paragraphs(&lines);
    let (mut out, mut history, mut last_post) = (Vec::new(), Vec::new(), Instant::now());
    for (i, block) in blocks.iter().enumerate() {
        if ocr::looks_like_code(&block.text) {
            continue;
        }
        let text = translator.translate(&block.text, &history[history.len().saturating_sub(translate::HISTORY)..])?;
        if !is_current(job.id) {
            break;
        }
        // Numbers, code and the like come back unchanged: leave the original on show.
        if text.to_lowercase() != block.text.to_lowercase() {
            history.push((block.text.clone(), text.clone()));
            out.extend(Patch::new(frame, w, h, block, text));
        }
        if last_post.elapsed() > PARTIAL_AFTER && i + 1 < blocks.len() {
            post(out.clone());
            last_post = Instant::now();
        }
    }
    if out.is_empty() && !job.live {
        out.push(Patch::status(NOT_FOUND, job.region.w));
    }
    Ok(out)
}

// --- UI thread state. Cells only: window procedures re-enter while Win32 calls run.

struct Ui {
    main: Cell<HWND>,
    overlay: Cell<HWND>,
    select: Cell<HWND>,
    live_next: Cell<bool>, // the selection in progress starts a live job
    drag_from: Cell<Option<(i32, i32)>>,
    drag_to: Cell<(i32, i32)>,
    region: Cell<Region>,
    job_id: Cell<u64>, // results from older jobs are dropped
    live: Cell<bool>,
    patches: RefCell<Vec<Patch>>,
}

thread_local! {
    static UI: Ui = Ui {
        main: Cell::new(null_mut()),
        overlay: Cell::new(null_mut()),
        select: Cell::new(null_mut()),
        live_next: Cell::new(false),
        drag_from: Cell::new(None),
        drag_to: Cell::new((0, 0)),
        region: Cell::new(Region { x: 0, y: 0, w: 0, h: 0 }),
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

fn stop() {
    set_job(None);
    UI.with(|ui| {
        ui.live.set(false);
        ui.job_id.set(0);
        unsafe { ShowWindow(ui.overlay.get(), SW_HIDE) };
    });
}

// --- Selection layer

unsafe fn begin_select(live: bool) {
    UI.with(|ui| unsafe {
        if !ui.select.get().is_null() {
            return;
        }
        stop();
        ui.live_next.set(live);
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
        RegisterHotKey(ui.main.get(), HK_ESC, 0, VK_ESCAPE as u32);
        ShowWindow(select, SW_SHOWNOACTIVATE);
    });
}

unsafe fn end_select() {
    UI.with(|ui| unsafe {
        UnregisterHotKey(ui.main.get(), HK_ESC);
        let select = ui.select.replace(null_mut());
        if !select.is_null() {
            ReleaseCapture();
            DestroyWindow(select);
        }
    });
}

fn selection_rect(from: (i32, i32), to: (i32, i32)) -> RECT {
    RECT { left: from.0.min(to.0), top: from.1.min(to.1), right: from.0.max(to.0), bottom: from.1.max(to.1) }
}

unsafe fn finish_select(hwnd: HWND) {
    UI.with(|ui| unsafe {
        let Some(from) = ui.drag_from.take() else { return };
        let rect = selection_rect(from, ui.drag_to.get());
        let mut origin = POINT { x: 0, y: 0 };
        ClientToScreen(hwnd, &mut origin);
        end_select();
        if rect.right - rect.left < MIN_SELECTION || rect.bottom - rect.top < MIN_SELECTION {
            return;
        }
        let region = Region {
            x: rect.left + origin.x,
            y: rect.top + origin.y,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        };
        let live = ui.live_next.get();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        ui.region.set(region);
        ui.job_id.set(id);
        ui.live.set(live);
        show_patches(vec![Patch::status(WORKING, region.w)]);
        set_job(Some(Job { region, live, id }));
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
                let (from, to) = UI.with(|ui| (ui.drag_from.get(), ui.drag_to.get()));
                if let Some(from) = from {
                    let brush = CreateSolidBrush(KEY_COLOR);
                    FillRect(hdc, &selection_rect(from, to), brush);
                    DeleteObject(brush);
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
                UI.with(|ui| {
                    if let Some(from) = ui.drag_from.get() {
                        let old = selection_rect(from, ui.drag_to.replace(at));
                        InvalidateRect(hwnd, &old, 0);
                        InvalidateRect(hwnd, &selection_rect(from, at), 0);
                    }
                });
                0
            }
            WM_LBUTTONUP => {
                UI.with(|ui| ui.drag_to.set(lparam_xy(l)));
                finish_select(hwnd);
                0
            }
            WM_RBUTTONDOWN => {
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

/// Picks the largest font that keeps the text inside the patch, starting from the height of
/// the original lines, and grows the patch only when even MIN_TEXT_PX does not fit.
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
    let mut px = if status { status_px } else { (patch.h / patch.lines).max(MIN_TEXT_PX) };
    let (size, width) = loop {
        let room = patch.h + px / 3; // the last line's leading is empty, so it may stick out
        let normal = measure(px, patch.w);
        if normal.bottom <= room {
            break (normal, patch.w);
        }
        let spread = measure(px, patch.max_w);
        if spread.bottom <= room {
            break (spread, patch.w.max(spread.right + 1));
        }
        if status || px <= MIN_TEXT_PX {
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
        SetWindowPos(overlay, HWND_TOPMOST, region.x, region.y, region.w, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
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
        SetBkMode(mem, TRANSPARENT as i32);
        UI.with(|ui| {
            for p in ui.patches.borrow().iter() {
                fill(mem, &RECT { left: p.x, top: p.y, right: p.x + p.w, bottom: p.y + p.h }, p.bg);
                let font = text_font(p.px, p.bold);
                let old_font = SelectObject(mem, font);
                SetTextColor(mem, p.fg);
                let top = p.y + (p.h - p.text_h) / 2;
                // DT_EDITCONTROL drops a last line that overhangs, so the box must hold all of it.
                let mut area = RECT { left: p.x, top, right: p.x + p.w, bottom: top + p.text_h };
                let text = wide(&p.text);
                DrawTextW(mem, text.as_ptr(), text.len() as i32, &mut area, TEXT_FLAGS);
                SelectObject(mem, old_font);
                DeleteObject(font);
            }
        });
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
        AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("終了"));
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        SetForegroundWindow(hwnd); // Windows only dismisses a tray menu properly this way
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, at.x, at.y, 0, hwnd, null());
        DestroyMenu(menu);
        if chosen as usize == MENU_EXIT {
            DestroyWindow(hwnd);
        }
    }
}

unsafe fn tray_icon(hwnd: HWND, action: u32) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        let tip = wide("画面翻訳\nCtrl+Alt+T 1回 / Ctrl+Alt+R リアルタイム / Ctrl+Alt+X 閉じる");
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
                    HK_LIVE if UI.with(|ui| ui.live.get()) => stop(),
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
            WM_TRAY => {
                if l as u32 == WM_RBUTTONUP {
                    tray_menu(hwnd);
                }
                0
            }
            WM_DESTROY => {
                tray_icon(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
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

fn main() {
    // shortcut: a fixed four threads; raise it if OCR must keep up with whole-screen regions.
    unsafe { std::env::set_var("RTEN_NUM_THREADS", "4") }; // no other thread exists yet
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        register_class(w!("ScreenTranslateMain"), Some(main_proc), IDC_ARROW);
        register_class(w!("ScreenTranslateSelect"), Some(select_proc), IDC_CROSS);
        register_class(w!("ScreenTranslateOverlay"), Some(overlay_proc), IDC_ARROW);

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
        SetWindowDisplayAffinity(overlay, WDA_EXCLUDEFROMCAPTURE); // never OCR our own output
        UI.with(|ui| {
            ui.main.set(main);
            ui.overlay.set(overlay);
        });

        let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
        for (id, key) in [(HK_ONCE, 'T'), (HK_LIVE, 'R'), (HK_STOP, 'X')] {
            if RegisterHotKey(main, id, mods, key as u32) == 0 {
                let text = wide(&format!("Ctrl+Alt+{key} を登録できません。すでに起動しているか、別のアプリが使っています。\0"));
                MessageBoxW(null_mut(), text.as_ptr(), w!("screen-translate"), MB_OK | MB_ICONERROR);
                return;
            }
        }
        tray_icon(main, NIM_ADD);

        let overlay_handle = overlay as usize;
        std::thread::spawn(move || worker(overlay_handle));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
