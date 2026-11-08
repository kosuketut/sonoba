//! The settings window: a page in WebView2, which comes with Windows 11 and with Edge on Windows
//! 10. Only this window uses it, so none of it stays in memory once the window closes.

use crate::i18n::t;
use crate::settings::{self, PAGE};
use std::cell::{Cell, OnceCell, RefCell};
use std::ptr::{null, null_mut};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, WebMessageReceivedEventHandler};
use windows::Win32::Foundation::{HWND as ComHwnd, RECT as ComRect};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const BIG_ICON: i32 = 48;
const TIMER: usize = 1;
const TIMER_MS: u32 = 250;
const WIDTH: i32 = 800;
const HEIGHT: i32 = 620;
const MIN_WIDTH: i32 = 640;
const MIN_HEIGHT: i32 = 480;
const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: *const u16 = w!("Sonoba");
const OLD_RUN_VALUE: *const u16 = w!("screen-translate");
const THEME_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
const USE_DARK_TITLE_BAR: u32 = 20; // DWMWA_USE_IMMERSIVE_DARK_MODE

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static WEBVIEW: RefCell<Option<ICoreWebView2>> = const { RefCell::new(None) };
    static FONTS: OnceCell<Vec<String>> = const { OnceCell::new() };
}

pub fn window() -> HWND {
    WINDOW.get()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn run_value_exists(value: *const u16) -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, value, RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == 0 }
}

pub fn autostart() -> bool {
    run_value_exists(RUN_VALUE)
}

/// An autostart from before the rename to Sonoba would start the old exe, so it moves to this one.
pub fn carry_over_autostart() {
    if run_value_exists(OLD_RUN_VALUE) {
        set_autostart(true);
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, OLD_RUN_VALUE) };
    }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if !on {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let command = wide(&format!("\"{}\"", exe.display()));
        RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ, command.as_ptr() as *const _, command.len() as u32 * 2);
    }
}

fn light_theme() -> bool {
    let (mut value, mut size) = (1u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, THEME_KEY, w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, null_mut(), &mut value as *mut _ as *mut _, &mut size) };
    value != 0
}

unsafe extern "system" fn add_font(font: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, list: LPARAM) -> i32 {
    unsafe {
        let name = &(*font).lfFaceName;
        let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
        if !name.starts_with('@') {
            (*(list as *mut Vec<String>)).push(name); // the @ ones are for vertical text
        }
        1
    }
}

pub fn fonts() -> Vec<String> {
    FONTS.with(|fonts| {
        fonts
            .get_or_init(|| unsafe {
                let mut list: Vec<String> = Vec::new();
                let mut wanted: LOGFONTW = std::mem::zeroed();
                wanted.lfCharSet = DEFAULT_CHARSET as u8;
                let hdc = GetDC(null_mut());
                EnumFontFamiliesExW(hdc, &wanted, Some(add_font), &mut list as *mut _ as LPARAM, 0);
                ReleaseDC(null_mut(), hdc);
                list.sort();
                list.dedup();
                list
            })
            .clone()
    })
}

pub fn open_link(url: &str) {
    if url.starts_with("https://") {
        unsafe { ShellExecuteW(null_mut(), w!("open"), wide(url).as_ptr(), null(), null(), SW_SHOWNORMAL) };
    }
}

pub fn post_to_page(text: &str) {
    WEBVIEW.with_borrow(|webview| {
        if let Some(webview) = webview {
            let _ = unsafe { webview.PostWebMessageAsJson(&HSTRING::from(text)) };
        }
    });
}

pub fn close_settings() {
    unsafe { DestroyWindow(window()) };
}

fn fit_to_window(hwnd: HWND) {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut r) };
    CONTROLLER.with_borrow(|controller| {
        if let Some(controller) = controller {
            let _ = unsafe { controller.SetBounds(ComRect { left: 0, top: 0, right: r.right, bottom: r.bottom }) };
        }
    });
}

fn page_ready(hwnd: HWND, controller: ICoreWebView2Controller) -> windows::core::Result<()> {
    if hwnd != window() {
        return unsafe { controller.Close() }; // closed again before the page came up
    }
    unsafe {
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetIsZoomControlEnabled(false)?;
        settings.SetAreDevToolsEnabled(std::env::var_os("SONOBA_DEV").is_some())?;
        let mut token = 0;
        let received = WebMessageReceivedEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                let mut text = PWSTR::null();
                args.WebMessageAsJson(&mut text)?;
                settings::on_message(&CoTaskMemPWSTR::from(text).to_string());
            }
            Ok(())
        }));
        webview.add_WebMessageReceived(&received, &mut token)?;
        controller.SetIsVisible(true)?;
        webview.NavigateToString(&HSTRING::from(PAGE))?;
        controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)?;
        CONTROLLER.set(Some(controller));
        WEBVIEW.set(Some(webview));
    }
    fit_to_window(hwnd);
    Ok(())
}

fn start_page(hwnd: HWND) -> windows::core::Result<()> {
    let folder = HSTRING::from(settings::data_dir().join("webview").as_os_str());
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        result?;
        let Some(environment) = environment else { return Ok(()) };
        let created = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
            result?;
            controller.map_or(Ok(()), |controller| page_ready(hwnd, controller))
        }));
        unsafe { environment.CreateCoreWebView2Controller(ComHwnd(hwnd), &created) }
    }));
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &folder, None, &handler) }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                settings::refresh();
                0
            }
            WM_SIZE => {
                fit_to_window(hwnd);
                0
            }
            WM_MOVE => {
                CONTROLLER.with_borrow(|controller| {
                    if let Some(controller) = controller {
                        let _ = controller.NotifyParentWindowPositionChanged();
                    }
                });
                0
            }
            WM_DPICHANGED => {
                let r = &*(l as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd) as i32;
                let info = &mut *(l as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT { x: MIN_WIDTH * dpi / 96, y: MIN_HEIGHT * dpi / 96 };
                0
            }
            WM_ACTIVATE => {
                if w & 0xFFFF == WA_INACTIVE as usize || w >> 16 != 0 {
                    settings::stop_choosing_key();
                } else {
                    CONTROLLER.with_borrow(|controller| {
                        if let Some(controller) = controller {
                            let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                        }
                    });
                }
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER);
                WEBVIEW.set(None);
                if let Some(controller) = CONTROLLER.take() {
                    let _ = controller.Close();
                }
                WINDOW.set(null_mut());
                settings::closed();
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
        class.lpszClassName = w!("SonobaSettings");
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hIcon = super::app_icon(Some(BIG_ICON));
        class.hIconSm = super::app_icon(None);
        class.hbrBackground = GetStockObject(if light_theme() { WHITE_BRUSH } else { BLACK_BRUSH }) as HBRUSH;
        RegisterClassExW(&class);
    }
}

pub fn open() {
    unsafe {
        if !window().is_null() {
            ShowWindow(window(), SW_RESTORE);
            SetForegroundWindow(window());
            return;
        }
        let title = wide(&t("Sonoba の設定"));
        let hwnd = CreateWindowExW(
            0,
            w!("SonobaSettings"),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        WINDOW.set(hwnd);
        let dark = (!light_theme()) as i32;
        DwmSetWindowAttribute(hwnd, USE_DARK_TITLE_BAR, &dark as *const _ as *const _, 4);
        let dpi = GetDpiForWindow(hwnd) as i32;
        SetWindowPos(hwnd, null_mut(), 0, 0, WIDTH * dpi / 96, HEIGHT * dpi / 96, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetTimer(hwnd, TIMER, TIMER_MS, None);
        if start_page(hwnd).is_err() {
            DestroyWindow(hwnd);
            MessageBoxW(null_mut(), wide(&t("設定画面を開けません。WebView2 ランタイムが必要です。")).as_ptr(), w!("Sonoba"), MB_OK | MB_ICONWARNING);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_autostart_value_moves_to_the_new_name() {
        // HKEY_CURRENT_USER points at a throwaway key for this process, so the real autostart entries stay untouched.
        unsafe {
            let sandbox = w!("Software\\Sonoba-test");
            let mut key = null_mut();
            assert_eq!(RegCreateKeyW(HKEY_CURRENT_USER, sandbox, &mut key), 0);
            assert_eq!(RegOverridePredefKey(HKEY_CURRENT_USER, key), 0);

            carry_over_autostart();
            assert!(!autostart(), "nothing to carry over");

            let old = wide("screen-translate.exe");
            assert_eq!(RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, OLD_RUN_VALUE, REG_SZ, old.as_ptr() as *const _, old.len() as u32 * 2), 0);
            carry_over_autostart();
            let (moved, gone) = (autostart(), !run_value_exists(OLD_RUN_VALUE));

            RegOverridePredefKey(HKEY_CURRENT_USER, null_mut());
            RegCloseKey(key);
            RegDeleteTreeW(HKEY_CURRENT_USER, sandbox);
            assert!(moved && gone);
        }
    }
}
