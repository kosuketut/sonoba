//! Screen OCR and English to Japanese translation, driven by global hotkeys.
//!
//! No window here ever takes the keyboard focus: the selection layer and the panel are
//! WS_EX_NOACTIVATE and Esc is a hotkey, so the active window stays active throughout.
#![windows_subsystem = "windows"]

mod capture;
mod ocr;
mod translate;

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
const PANEL_MIN_WIDTH: i32 = 420;
const FONT_PX: i32 = 20;
const PAD_PX: i32 = 12;

const HK_ONCE: i32 = 1;
const HK_LIVE: i32 = 2;
const HK_STOP: i32 = 3;
const HK_ESC: i32 = 4;
const WM_TEXT: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const MENU_EXIT: usize = 1;

const KEY_COLOR: u32 = 0x00FF00FF; // shows through the dim layer: the selected rectangle
const DIM_ALPHA: u8 = 110;
const PANEL_ALPHA: u8 = 238;
const PANEL_BG: u32 = 0x00201C1C;
const PANEL_FG: u32 = 0x00F5F5F5;

const NOT_FOUND: &str = "文字が見つかりません";
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

fn worker(panel: usize) {
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
        run(current, &mut models, &mut translator, panel);
    }
}

fn run(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, panel: usize) {
    let post = |text: String| unsafe {
        PostMessageW(panel as HWND, WM_TEXT, job.id as usize, Box::into_raw(Box::new(text)) as isize);
    };
    let Region { x, y, w, h } = job.region;
    let Some(grabber) = capture::Grabber::new(w, h) else {
        post("画面を取得できません".to_string());
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
            let text = result.unwrap_or_else(|e| format!("エラー: {e}"));
            if is_current(job.id) && shown.as_ref() != Some(&text) {
                post(text.clone());
                shown = Some(text);
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
    post: &impl Fn(String),
) -> Result<String, String> {
    if models.is_none() {
        *models = Some(ocr::Ocr::new()?);
    }
    let Region { w, h, .. } = job.region;
    let lines = models.as_ref().unwrap().read(frame, w as usize, h as usize)?;
    let paragraphs = ocr::paragraphs(&lines);
    if paragraphs.is_empty() {
        return Ok(if job.live { String::new() } else { NOT_FOUND.to_string() });
    }
    let (mut out, mut last_post) = (Vec::new(), Instant::now());
    for paragraph in &paragraphs {
        out.push(translator.translate(paragraph)?);
        if !is_current(job.id) {
            break;
        }
        if last_post.elapsed() > PARTIAL_AFTER && out.len() < paragraphs.len() {
            post(out.join("\n"));
            last_post = Instant::now();
        }
    }
    Ok(out.join("\n"))
}

// --- UI thread state. Cells only: window procedures re-enter while Win32 calls run.

struct Ui {
    main: Cell<HWND>,
    panel: Cell<HWND>,
    select: Cell<HWND>,
    live_next: Cell<bool>, // the selection in progress starts a live job
    drag_from: Cell<Option<(i32, i32)>>,
    drag_to: Cell<(i32, i32)>,
    region: Cell<Region>,
    job_id: Cell<u64>, // results from older jobs are dropped
    live: Cell<bool>,
    placed: Cell<bool>, // panel position chosen for this job; keep it so dragging sticks
    text: RefCell<Vec<u16>>,
}

thread_local! {
    static UI: Ui = Ui {
        main: Cell::new(null_mut()),
        panel: Cell::new(null_mut()),
        select: Cell::new(null_mut()),
        live_next: Cell::new(false),
        drag_from: Cell::new(None),
        drag_to: Cell::new((0, 0)),
        region: Cell::new(Region { x: 0, y: 0, w: 0, h: 0 }),
        job_id: Cell::new(0),
        live: Cell::new(false),
        placed: Cell::new(false),
        text: RefCell::new(Vec::new()),
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
        unsafe { ShowWindow(ui.panel.get(), SW_HIDE) };
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
        ui.placed.set(false);
        show_text(WORKING);
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

// --- Result panel

unsafe fn panel_font(hwnd: HWND) -> (HFONT, i32) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd) as i32;
        let px = FONT_PX * dpi / 96;
        let font = CreateFontW(
            -px,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            w!("Yu Gothic UI"),
        );
        (font, PAD_PX * dpi / 96)
    }
}

/// Stores `text` and sizes the panel to it. Empty text hides the panel.
fn show_text(text: &str) {
    UI.with(|ui| unsafe {
        let panel = ui.panel.get();
        if text.is_empty() {
            ShowWindow(panel, SW_HIDE);
            return;
        }
        *ui.text.borrow_mut() = wide(text);
        SetWindowTextW(panel, wide(&format!("{text}\0")).as_ptr()); // keeps it readable by screen readers

        let region = ui.region.get();
        let rect = RECT { left: region.x, top: region.y, right: region.x + region.w, bottom: region.y + region.h };
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let area = info.rcWork;

        let mut current: RECT = std::mem::zeroed();
        GetWindowRect(panel, &mut current);
        let placed = ui.placed.get();
        if !placed {
            SetWindowPos(panel, null_mut(), rect.left, rect.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        let (font, pad) = panel_font(panel); // after the move, so the monitor's DPI applies
        let width = if placed {
            current.right - current.left
        } else {
            let min = PANEL_MIN_WIDTH * GetDpiForWindow(panel) as i32 / 96;
            region.w.max(min).min(area.right - area.left)
        };
        let hdc = GetDC(panel);
        let old = SelectObject(hdc, font);
        let mut measure = RECT { left: 0, top: 0, right: width - pad * 2, bottom: 0 };
        let text16 = ui.text.borrow();
        DrawTextW(hdc, text16.as_ptr(), text16.len() as i32, &mut measure, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL);
        SelectObject(hdc, old);
        ReleaseDC(panel, hdc);
        DeleteObject(font);
        let height = (measure.bottom + pad * 2).min(area.bottom - area.top);

        let (mut x, mut y) = if placed {
            (current.left, current.top)
        } else if rect.bottom + height <= area.bottom {
            (rect.left, rect.bottom)
        } else if rect.top - height >= area.top {
            (rect.left, rect.top - height)
        } else {
            (rect.left, rect.bottom - height) // no room outside: lies over the region, which capture ignores
        };
        x = x.clamp(area.left, (area.right - width).max(area.left));
        y = y.clamp(area.top, (area.bottom - height).max(area.top));
        ui.placed.set(true);
        SetWindowPos(panel, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        InvalidateRect(panel, null(), 0);
    });
}

unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_NCHITTEST => HTCAPTION as LRESULT, // drag anywhere to move
            WM_NCRBUTTONUP => {
                stop();
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut client);
                let brush = CreateSolidBrush(PANEL_BG);
                FillRect(hdc, &client, brush);
                DeleteObject(brush);
                let (font, pad) = panel_font(hwnd);
                let old = SelectObject(hdc, font);
                SetBkMode(hdc, TRANSPARENT as i32);
                SetTextColor(hdc, PANEL_FG);
                let mut area = RECT { left: pad, top: pad, right: client.right - pad, bottom: client.bottom - pad };
                UI.with(|ui| {
                    let text = ui.text.borrow();
                    if !text.is_empty() {
                        // Before the first result the pointer is dangling, and DrawTextW reads it even for length 0.
                        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut area, DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL);
                    }
                });
                SelectObject(hdc, old);
                DeleteObject(font);
                EndPaint(hwnd, &ps);
                0
            }
            WM_TEXT => {
                let text = Box::from_raw(l as *mut String);
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    show_text(&text);
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
        register_class(w!("ScreenTranslatePanel"), Some(panel_proc), IDC_ARROW);

        let module = GetModuleHandleW(null());
        let main = CreateWindowExW(0, w!("ScreenTranslateMain"), w!("screen-translate"), 0, 0, 0, 0, 0, null_mut(), null_mut(), module, null());
        let panel = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            w!("ScreenTranslatePanel"),
            null(),
            WS_POPUP,
            0,
            0,
            PANEL_MIN_WIDTH,
            80,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        SetLayeredWindowAttributes(panel, 0, PANEL_ALPHA, LWA_ALPHA);
        SetWindowDisplayAffinity(panel, WDA_EXCLUDEFROMCAPTURE); // never OCR our own output
        UI.with(|ui| {
            ui.main.set(main);
            ui.panel.set(panel);
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

        let panel_handle = panel as usize;
        std::thread::spawn(move || worker(panel_handle));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
