//! A short message beside the cursor, for actions that change nothing else on screen.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const SHOW_MS: u32 = 1400;
const TIMER: usize = 1;
const FONT_PX: i32 = 14;
const PAD_X: i32 = 14;
const PAD_Y: i32 = 8;
const CURSOR_GAP: i32 = 18;
const RADIUS: i32 = 10;
const BG: u32 = 0x00221E1C;
const TEXT: u32 = 0x00F2F3F0;

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static MESSAGE: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static DPI: Cell<i32> = const { Cell::new(96) };
}

fn scaled(value: i32) -> i32 {
    value * DPI.get() / 96
}

unsafe fn font() -> HFONT {
    unsafe {
        CreateFontW(
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
        )
    }
}

pub fn show(text: &str) {
    unsafe {
        let mut hwnd = WINDOW.get();
        if hwnd.is_null() {
            hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                w!("ScreenTranslateToast"),
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
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE); // OCR must not read it
            WINDOW.set(hwnd);
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut title = units.clone();
        title.push(0);
        SetWindowTextW(hwnd, title.as_ptr());

        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let (mut dpi, mut unused) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
        DPI.set(dpi as i32);

        let hdc = GetDC(hwnd);
        let f = font();
        let old = SelectObject(hdc, f);
        let mut size = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(hdc, units.as_ptr(), units.len() as i32, &mut size);
        SelectObject(hdc, old);
        DeleteObject(f);
        ReleaseDC(hwnd, hdc);
        MESSAGE.set(units);

        let (width, height) = (size.cx + scaled(PAD_X) * 2, size.cy + scaled(PAD_Y) * 2);
        let work = info.rcWork;
        let x = (at.x + scaled(CURSOR_GAP)).min(work.right - width);
        let y = (at.y + scaled(CURSOR_GAP)).min(work.bottom - height);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let corner = scaled(RADIUS);
        SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, width + 1, height + 1, corner, corner), 1);
        InvalidateRect(hwnd, null(), 0);
        SetTimer(hwnd, TIMER, SHOW_MS, None);
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut area: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut area);
        let brush = CreateSolidBrush(BG);
        FillRect(hdc, &area, brush);
        DeleteObject(brush);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, TEXT);
        let f = font();
        let old = SelectObject(hdc, f);
        MESSAGE.with_borrow(|units| {
            DrawTextW(hdc, units.as_ptr(), units.len() as i32, &mut area, DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX);
        });
        SelectObject(hdc, old);
        DeleteObject(f);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_TIMER => {
                KillTimer(hwnd, TIMER);
                ShowWindow(hwnd, SW_HIDE);
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
        class.lpszClassName = w!("ScreenTranslateToast");
        RegisterClassExW(&class);
    }
}
