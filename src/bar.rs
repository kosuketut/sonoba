//! The bar shown over the selection layer: what to translate from and into, and whether to keep
//! watching the region. Like the layer under it, it never takes the focus from the app in use.

use crate::config;
use crate::i18n::t;
use std::cell::Cell;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const SOURCES: [(&str, &str); 2] = [("auto", "自動"), ("ja", "日本語も翻訳")];
const TARGETS: [(&str, &str); 6] =
    [("ja", "日本語"), ("ja(easy)", "やさしい日本語"), ("en", "英語"), ("ko", "韓国語"), ("zh-hans", "中国語（簡体字）"), ("zh-hant", "中国語（繁体字）")];
const CHIPS: usize = 3;
const ADD_HINT: &str = "Shift+ドラッグで範囲を追加";
const LIVE: usize = 2; // the one chip that is a switch, not a list
const BAR_H: i32 = 48;
const PAD: i32 = 8;
const CHIP_H: i32 = 32;
const CHIP_PAD: i32 = 14;
const ROW_H: i32 = 32;
const TOP: i32 = 16;
const RADIUS: i32 = 12;
const FONT_PX: i32 = 14;
const BG: u32 = 0x00221E1C;
const CHIP: u32 = 0x00332E2C;
const HOVER: u32 = 0x0044403C;
const TEXT: u32 = 0x00F2F3F0;
const SOFT: u32 = 0x00ACB1A8;
const ACCENT: u32 = 0x007AC337;

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static ANCHOR: Cell<(i32, i32, i32)> = const { Cell::new((0, 0, 96)) }; // centre, top, dpi
    static OPEN: Cell<Option<usize>> = const { Cell::new(None) }; // the chip showing its list
    static HOT: Cell<Option<(usize, Option<usize>)>> = const { Cell::new(None) }; // chip, and row of its list
    static LIVE_ON: Cell<bool> = const { Cell::new(false) };
    static RUNNING: Cell<bool> = const { Cell::new(false) }; // there is a job to add a region to
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn scaled(value: i32) -> i32 {
    value * ANCHOR.get().2 / 96
}

fn options(chip: usize) -> &'static [(&'static str, &'static str)] {
    if chip == 0 { &SOURCES } else { &TARGETS }
}

fn name_of(list: &[(&str, &'static str)], code: &str) -> String {
    t(list.iter().find(|(c, _)| *c == code).map_or(list[0].1, |(_, name)| name))
}

fn labels() -> [String; CHIPS] {
    let config = config::get();
    let onoff = t(if LIVE_ON.get() { "オン" } else { "オフ" });
    [
        format!("{}: {} ▾", t("翻訳元"), name_of(&SOURCES, &config.source)),
        format!("{}: {} ▾", t("翻訳先"), name_of(&TARGETS, &config.target)),
        format!("{}: {onoff}", t("リアルタイム")),
    ]
}

unsafe fn with_font<R>(hdc: HDC, work: impl FnOnce() -> R) -> R {
    unsafe {
        let font = CreateFontW(
            -scaled(FONT_PX),
            0,
            0,
            0,
            FW_MEDIUM as i32,
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
        let old = SelectObject(hdc, font);
        let out = work();
        SelectObject(hdc, old);
        DeleteObject(font);
        out
    }
}

fn text_width(hdc: HDC, text: &str) -> i32 {
    let text = wide(text);
    let mut size = SIZE { cx: 0, cy: 0 };
    unsafe { GetTextExtentPoint32W(hdc, text.as_ptr(), text.len() as i32, &mut size) };
    size.cx
}

/// The chips from left to right, and the bar's width.
fn chips(hdc: HDC) -> ([RECT; CHIPS], i32) {
    let top = scaled((BAR_H - CHIP_H) / 2);
    let mut x = scaled(PAD);
    let mut out = [RECT { left: 0, top: 0, right: 0, bottom: 0 }; CHIPS];
    for (rect, label) in out.iter_mut().zip(labels()) {
        let right = x + text_width(hdc, &label) + scaled(CHIP_PAD) * 2;
        *rect = RECT { left: x, top, right, bottom: top + scaled(CHIP_H) };
        x = right + scaled(PAD);
    }
    if RUNNING.get() {
        x += text_width(hdc, &t(ADD_HINT)) + scaled(CHIP_PAD);
    }
    (out, x)
}

/// The open chip's list: one rectangle per option, starting under the bar.
fn rows(chip: usize, chips: &[RECT; CHIPS]) -> Vec<RECT> {
    let left = chips[chip].left;
    let right = left + scaled(CHIP_PAD * 2 + 190);
    (0..options(chip).len() as i32)
        .map(|i| {
            let top = scaled(BAR_H + i * ROW_H);
            RECT { left, top, right, bottom: top + scaled(ROW_H) }
        })
        .collect()
}

fn lay_out() {
    let hwnd = WINDOW.get();
    unsafe {
        let hdc = GetDC(hwnd);
        let (rects, width) = with_font(hdc, || chips(hdc));
        ReleaseDC(hwnd, hdc);
        let open = OPEN.get();
        let list = open.map_or_else(Vec::new, |chip| rows(chip, &rects));
        let width = width.max(list.last().map_or(0, |r| r.right + scaled(PAD)));
        let height = list.last().map_or(scaled(BAR_H), |r| r.bottom + scaled(PAD));
        let (centre, top, _) = ANCHOR.get();
        SetWindowPos(hwnd, HWND_TOPMOST, centre - width / 2, top, width, height, SWP_NOACTIVATE);
        let corner = scaled(RADIUS);
        SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, width + 1, height + 1, corner, corner), 1);
        InvalidateRect(hwnd, null(), 0);
    }
}

pub fn show(live: bool, running: bool) {
    unsafe {
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let (mut dpi, mut unused) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
        let m = info.rcMonitor;
        ANCHOR.set(((m.left + m.right) / 2, m.top + TOP * dpi as i32 / 96, dpi as i32));
        OPEN.set(None);
        HOT.set(None);
        LIVE_ON.set(live);
        RUNNING.set(running);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("ScreenTranslateBar"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        WINDOW.set(hwnd);
        if std::env::var_os("SCREEN_TRANSLATE_DEV").is_none() {
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        lay_out();
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn hide() {
    unsafe { DestroyWindow(WINDOW.replace(null_mut())) };
}

/// True while the switch on the bar is on.
pub fn live() -> bool {
    LIVE_ON.get()
}

fn hit(x: i32, y: i32, hdc: HDC) -> Option<(usize, Option<usize>)> {
    let (rects, _) = chips(hdc);
    let at = POINT { x, y };
    let inside = |r: &RECT| unsafe { PtInRect(r, at) != 0 };
    if let Some(chip) = OPEN.get()
        && let Some(row) = rows(chip, &rects).iter().position(inside)
    {
        return Some((chip, Some(row)));
    }
    rects.iter().position(inside).map(|chip| (chip, None))
}

unsafe fn fill(hdc: HDC, rect: &RECT, color: u32, corner: i32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID, 1, color);
        let (old_brush, old_pen) = (SelectObject(hdc, brush), SelectObject(hdc, pen));
        RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, corner, corner);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        DeleteObject(brush);
        DeleteObject(pen);
    }
}

unsafe fn label(hdc: HDC, text: &str, rect: &RECT, color: u32) {
    unsafe {
        let text = wide(text);
        let mut area = RECT { left: rect.left + scaled(CHIP_PAD), ..*rect };
        SetTextColor(hdc, color);
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut area, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let screen = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let (w, h) = (client.right, client.bottom);
        let hdc = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(hdc, bitmap);
        fill(hdc, &RECT { left: -1, top: -1, right: w + 2, bottom: h + 2 }, BG, 0);
        SetBkMode(hdc, TRANSPARENT as i32);
        with_font(hdc, || {
            let (rects, _) = chips(hdc);
            let corner = scaled(8);
            let hot = HOT.get();
            for (i, (rect, text)) in rects.iter().zip(labels()).enumerate() {
                let on = i == LIVE && LIVE_ON.get();
                let color = if on { ACCENT } else if hot == Some((i, None)) || OPEN.get() == Some(i) { HOVER } else { CHIP };
                fill(hdc, rect, color, corner);
                label(hdc, &text, rect, if on { BG } else { TEXT });
            }
            if RUNNING.get() {
                let last = rects[CHIPS - 1];
                let hint = RECT { left: last.right, right: w, top: 0, bottom: scaled(BAR_H) };
                label(hdc, &t(ADD_HINT), &hint, SOFT);
            }
            if let Some(chip) = OPEN.get() {
                let config = config::get();
                let current = if chip == 0 { config.source } else { config.target };
                for (row, (rect, (code, name))) in rows(chip, &rects).iter().zip(options(chip)).enumerate() {
                    if hot == Some((chip, Some(row))) {
                        fill(hdc, rect, HOVER, corner);
                    }
                    label(hdc, &t(name), rect, if *code == current { ACCENT } else { TEXT });
                }
            }
        });
        BitBlt(screen, 0, 0, w, h, hdc, 0, 0, SRCCOPY);
        SelectObject(hdc, old);
        DeleteObject(bitmap);
        DeleteDC(hdc);
        EndPaint(hwnd, &ps);
    }
}

fn clicked(chip: usize, row: Option<usize>) {
    match row {
        Some(row) => {
            let code = options(chip)[row].0.to_string();
            config::update(|c| if chip == 0 { c.source = code } else { c.target = code });
            OPEN.set(None);
        }
        None if chip == LIVE => {
            LIVE_ON.set(!LIVE_ON.get());
            OPEN.set(None);
        }
        None => OPEN.set(if OPEN.get() == Some(chip) { None } else { Some(chip) }),
    }
    lay_out();
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let xy = || ((l & 0xFFFF) as i16 as i32, (l >> 16 & 0xFFFF) as i16 as i32);
    let found = || unsafe {
        let hdc = GetDC(hwnd);
        let (x, y) = xy();
        let found = with_font(hdc, || hit(x, y, hdc));
        ReleaseDC(hwnd, hdc);
        found
    };
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                let now = found();
                if HOT.replace(now) != now {
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            WM_LBUTTONDOWN => {
                match found() {
                    Some((chip, row)) => clicked(chip, row),
                    None if OPEN.take().is_some() => lay_out(),
                    None => {}
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

pub fn register_class() {
    unsafe {
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = Some(proc);
        class.hInstance = GetModuleHandleW(null());
        class.lpszClassName = w!("ScreenTranslateBar");
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        RegisterClassExW(&class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_of_the_bar_has_a_name() {
        assert_eq!(name_of(&TARGETS, "ja(easy)"), "やさしい日本語");
        assert_eq!(name_of(&SOURCES, "unknown"), "自動");
    }
}
