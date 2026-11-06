//! The settings window: model, device and hotkeys, in plain Win32 controls.

use crate::config::{self, Hotkey};
use crate::engine::{self, Device};
use crate::{models, request_load, request_unload, try_hotkeys};
use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const ID_MODEL: i32 = 101;
const ID_DEVICE: i32 = 102;
const ID_DOWNLOAD: i32 = 103;
const ID_DELETE: i32 = 104;
const ID_LOAD: i32 = 105;
const ID_UNLOAD: i32 = 106;
const ID_STATUS: i32 = 107;
const ID_MESSAGE: i32 = 108;
const ID_CLOSE: i32 = 109;
const ID_HOTKEY: i32 = 110; // and the two after it
const ID_GPU_LIMIT: i32 = 113;
const ID_THREADS: i32 = 114;
const ID_SHOW_FRAME: i32 = 115;
const ID_FLASH_FRAME: i32 = 116;
const TIMER: usize = 1;
const TIMER_MS: u32 = 250;
const HOTKEY_LABELS: [&str; 3] = ["範囲を 1 回翻訳", "リアルタイム翻訳", "訳文を消す"];
const DEVICES: [(&str, Device); 3] =
    [("自動（GPU メモリに載る分を GPU で、残りを CPU で実行）", Device::Auto), ("GPU のみ", Device::Gpu), ("CPU のみ", Device::Cpu)];
const GPU_LIMITS: [(&str, Option<u64>); 9] = [
    ("制限なし", None),
    ("0.5GB", Some(512)),
    ("1GB", Some(1024)),
    ("1.5GB", Some(1536)),
    ("2GB", Some(2048)),
    ("3GB", Some(3072)),
    ("4GB", Some(4096)),
    ("6GB", Some(6144)),
    ("8GB", Some(8192)),
];
const FONT_PX: i32 = 14;
const LICENSE_QUESTION: &str = "PLaMo 3 Translate は PLaMo コミュニティライセンスで提供されています。\n\
    ダウンロードして使うと、この契約に同意したことになります。\n\
    https://plamo.preferredai.jp/info/plamo-community-license-ja\n\n\
    組織内での利用を含む商業目的の利用には、PFN への事前登録が必要です。\n\nダウンロードしますか？";

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static FONT: Cell<HFONT> = const { Cell::new(null_mut()) };
    static ENTRIES: std::cell::RefCell<Vec<models::Entry>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn window() -> HWND {
    WINDOW.get()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn control(window: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(window, id) }
}

fn set_text(window: HWND, id: i32, text: &str) {
    unsafe { SetWindowTextW(control(window, id), wide(text).as_ptr()) };
}

fn enable(window: HWND, id: i32, on: bool) {
    unsafe { EnableWindow(control(window, id), on as i32) };
}

fn add(parent: HWND, class: *const u16, text: &str, style: u32, id: i32, (x, y, w, h): (i32, i32, i32, i32)) -> HWND {
    unsafe {
        let dpi = GetDpiForWindow(parent) as i32;
        let s = |v: i32| v * dpi / 96;
        let text = wide(text);
        let child = CreateWindowExW(
            0,
            class,
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            s(x),
            s(y),
            s(w),
            s(h),
            parent,
            id as isize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(child, WM_SETFONT, FONT.get() as usize, 1);
        child
    }
}

fn selected_file(window: HWND) -> Option<String> {
    let index = unsafe { SendMessageW(control(window, ID_MODEL), CB_GETCURSEL, 0, 0) };
    ENTRIES.with_borrow(|entries| entries.get(index as usize).map(|e| e.file.clone()))
}

fn fill_models(window: HWND) {
    let current = config::get().model;
    let combo = control(window, ID_MODEL);
    let entries = models::list();
    unsafe {
        SendMessageW(combo, CB_RESETCONTENT, 0, 0);
        for entry in &entries {
            SendMessageW(combo, CB_ADDSTRING, 0, wide(&entry.label).as_ptr() as isize);
        }
        let index = entries.iter().position(|e| e.file == current).unwrap_or_else(|| {
            // The file has gone, so the choice goes with it.
            let index = entries.iter().position(|e| e.file == models::DEFAULT).unwrap_or(0);
            config::update(|c| c.model = entries[index].file.clone());
            index
        });
        SendMessageW(combo, CB_SETCURSEL, index, 0);
    }
    ENTRIES.set(entries);
}

/// Keeps the status line and the buttons in step with the model, the engine and any download.
fn refresh(window: HWND) {
    let Some(file) = selected_file(window) else { return };
    let present = models::is_present(&file);
    let partial = models::has_part(&file);
    let progress = models::PROGRESS.load(Ordering::Relaxed);
    let state = engine::STATE.load(Ordering::Relaxed);
    let status = match progress {
        models::VERIFYING => "ダウンロードしたファイルを確認しています".to_string(),
        0.. => format!("ダウンロード中 {progress}%"),
        _ if !present && partial => "途中までダウンロードしてあります（続きから再開できます）".to_string(),
        _ if !present => "ダウンロードされていません".to_string(),
        _ if state == engine::READY => "読み込み済み（すぐ翻訳できます）".to_string(),
        _ if state == engine::LOADING => "読み込み中…".to_string(),
        _ => "読み込まれていません（最初の翻訳のときに読み込みます）".to_string(),
    };
    set_text(window, ID_STATUS, &status);
    if let Some(error) = models::LAST_ERROR.lock().unwrap().take() {
        set_text(window, ID_MESSAGE, &error);
    }
    let downloading = progress >= 0;
    set_text(window, ID_DOWNLOAD, if downloading { "中止" } else { "ダウンロード" });
    enable(window, ID_DOWNLOAD, (downloading && progress != models::VERIFYING) || (!downloading && !present && models::can_download(&file)));
    enable(window, ID_DELETE, (present || partial) && !downloading);
    enable(window, ID_LOAD, present && !downloading && state == engine::UNLOADED);
    enable(window, ID_UNLOAD, state != engine::UNLOADED);
    enable(window, ID_GPU_LIMIT, config::get().device == Device::Auto);
}

fn read_hotkey(control: HWND) -> Option<Hotkey> {
    let value = unsafe { SendMessageW(control, HKM_GETHOTKEY, 0, 0) } as u32;
    let (key, flags) = (value & 0xFF, (value >> 8) & 0xFF);
    let mut mods = 0;
    for (flag, modifier) in [(HOTKEYF_ALT, MOD_ALT), (HOTKEYF_CONTROL, MOD_CONTROL), (HOTKEYF_SHIFT, MOD_SHIFT)] {
        if flags & flag as u32 != 0 {
            mods |= modifier;
        }
    }
    (key != 0 && mods != 0).then_some(Hotkey { mods, key })
}

fn write_hotkey(control: HWND, hotkey: Hotkey) {
    let mut flags = 0;
    for (flag, modifier) in [(HOTKEYF_ALT, MOD_ALT), (HOTKEYF_CONTROL, MOD_CONTROL), (HOTKEYF_SHIFT, MOD_SHIFT)] {
        if hotkey.mods & modifier != 0 {
            flags |= flag as u32;
        }
    }
    unsafe { SendMessageW(control, HKM_SETHOTKEY, (hotkey.key | flags << 8) as usize, 0) };
}

fn hotkey_changed(window: HWND, index: usize) {
    let box_ = control(window, ID_HOTKEY + index as i32);
    let mut hotkeys = config::get().hotkeys;
    let Some(new) = read_hotkey(box_) else { return };
    hotkeys[index] = new;
    let repeated = (0..3).any(|i| i != index && hotkeys[i] == new);
    if repeated || !try_hotkeys(new) {
        set_text(window, ID_MESSAGE, "そのキーは使えません（別の操作や別のアプリが使っています）");
        write_hotkey(box_, config::get().hotkeys[index]);
        return;
    }
    set_text(window, ID_MESSAGE, "");
    config::update(|c| c.hotkeys = hotkeys);
}

/// A changed choice takes effect with the next load, or right away if a model is in memory.
fn choice_changed(window: HWND, change: impl FnOnce(&mut config::Config)) {
    config::update(change);
    if engine::STATE.load(Ordering::Relaxed) != engine::UNLOADED {
        request_unload();
        if selected_file(window).is_some_and(|f| models::is_present(&f)) {
            request_load();
        }
    }
}

fn on_command(window: HWND, id: i32, code: u32) {
    match (id, code) {
        (ID_MODEL, CBN_SELCHANGE) => {
            if let Some(file) = selected_file(window) {
                choice_changed(window, |c| c.model = file);
            }
        }
        (ID_DEVICE, CBN_SELCHANGE) => {
            let index = unsafe { SendMessageW(control(window, ID_DEVICE), CB_GETCURSEL, 0, 0) } as usize;
            if let Some((_, device)) = DEVICES.get(index) {
                choice_changed(window, |c| c.device = *device);
            }
        }
        (ID_GPU_LIMIT, CBN_SELCHANGE) => {
            let index = unsafe { SendMessageW(control(window, ID_GPU_LIMIT), CB_GETCURSEL, 0, 0) } as usize;
            if let Some((_, limit)) = GPU_LIMITS.get(index) {
                choice_changed(window, |c| c.gpu_limit_mib = *limit);
            }
        }
        (ID_THREADS, CBN_SELCHANGE) => {
            let index = unsafe { SendMessageW(control(window, ID_THREADS), CB_GETCURSEL, 0, 0) } as usize;
            choice_changed(window, |c| c.cpu_threads = (index > 0).then_some(index));
        }
        (ID_SHOW_FRAME | ID_FLASH_FRAME, BN_CLICKED) => {
            let on = unsafe { SendMessageW(control(window, id), BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
            config::update(|c| if id == ID_SHOW_FRAME { c.show_frame = on } else { c.flash_frame = on });
            crate::update_frame();
        }
        (ID_DOWNLOAD, BN_CLICKED) => {
            set_text(window, ID_MESSAGE, "");
            if models::PROGRESS.load(Ordering::Relaxed) >= 0 {
                models::CANCEL.store(true, Ordering::Relaxed);
            } else if let Some(file) = selected_file(window) {
                // The licence binds whoever downloads the model, so it is theirs to accept.
                let question = wide(LICENSE_QUESTION);
                let answer = unsafe { MessageBoxW(window, question.as_ptr(), w!("モデルのライセンス"), MB_YESNO | MB_ICONINFORMATION) };
                if answer == IDYES {
                    models::start_download(file);
                }
            }
        }
        (ID_DELETE, BN_CLICKED) => {
            let Some(file) = selected_file(window) else { return };
            let after = if models::can_download(&file) { "もう一度使うには、ダウンロードし直す必要があります。" } else { "このファイルはごみ箱に入らず、元に戻せません。" };
            let question = wide(&format!("{file} を削除しますか？\n{after}"));
            let answer = unsafe { MessageBoxW(window, question.as_ptr(), w!("モデルの削除"), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) };
            if answer == IDYES {
                if config::get().model == file {
                    crate::unload_now();
                    engine::wait_until_stopped(); // Windows cannot delete a file that is in use
                }
                if let Err(e) = models::delete(&file) {
                    set_text(window, ID_MESSAGE, &format!("削除できません: {e}"));
                }
                fill_models(window);
            }
        }
        (ID_LOAD, BN_CLICKED) => request_load(),
        (ID_UNLOAD, BN_CLICKED) => crate::unload_now(),
        (ID_CLOSE, BN_CLICKED) => unsafe {
            DestroyWindow(window);
        },
        (ID_HOTKEY..=112, EN_CHANGE) => hotkey_changed(window, (id - ID_HOTKEY) as usize),
        _ => {}
    }
    refresh(window);
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                on_command(hwnd, (w & 0xFFFF) as i32, (w >> 16) as u32);
                0
            }
            WM_TIMER => {
                refresh(hwnd);
                0
            }
            // The hotkey boxes need the keys in use, but only while this window has the focus.
            WM_ACTIVATE => {
                if w & 0xFFFF == WA_INACTIVE as usize || w >> 16 != 0 {
                    crate::resume_hotkeys();
                } else {
                    crate::pause_hotkeys();
                }
                DefWindowProcW(hwnd, msg, w, l) // which gives the keyboard focus to the window
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER);
                DeleteObject(FONT.replace(null_mut()));
                WINDOW.set(null_mut());
                crate::resume_hotkeys();
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

pub fn register_class() {
    unsafe {
        let controls = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_HOTKEY_CLASS };
        InitCommonControlsEx(&controls);
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = Some(proc);
        class.hInstance = GetModuleHandleW(null());
        class.lpszClassName = w!("ScreenTranslateSettings");
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hbrBackground = (COLOR_BTNFACE + 1) as isize as HBRUSH;
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
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = CreateWindowExW(
            0,
            w!("ScreenTranslateSettings"),
            w!("screen-translate の設定"),
            style,
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
        let dpi = GetDpiForWindow(hwnd) as i32;
        let s = |v: i32| v * dpi / 96;
        let mut outer = RECT { left: 0, top: 0, right: s(560), bottom: s(500) };
        AdjustWindowRectExForDpi(&mut outer, style, 0, 0, dpi as u32);
        SetWindowPos(hwnd, null_mut(), 0, 0, outer.right - outer.left, outer.bottom - outer.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        FONT.set(CreateFontW(
            -s(FONT_PX),
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
        ));

        let combo = WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32;
        let button = WS_TABSTOP | BS_PUSHBUTTON as u32;
        add(hwnd, w!("STATIC"), "翻訳モデル", 0, -1, (20, 22, 110, 22));
        add(hwnd, w!("COMBOBOX"), "", combo, ID_MODEL, (136, 18, 404, 260));
        add(hwnd, w!("STATIC"), "", 0, ID_STATUS, (136, 52, 404, 22));
        add(hwnd, w!("BUTTON"), "ダウンロード", button, ID_DOWNLOAD, (136, 80, 100, 30));
        add(hwnd, w!("BUTTON"), "削除", button, ID_DELETE, (240, 80, 64, 30));
        add(hwnd, w!("BUTTON"), "読み込む", button, ID_LOAD, (328, 80, 100, 30));
        add(hwnd, w!("BUTTON"), "アンロード", button, ID_UNLOAD, (432, 80, 108, 30));

        add(hwnd, w!("STATIC"), "実行方法", 0, -1, (20, 130, 110, 22));
        add(hwnd, w!("COMBOBOX"), "", combo, ID_DEVICE, (136, 126, 404, 200));
        let config = config::get();
        for (label, _) in DEVICES {
            SendMessageW(control(hwnd, ID_DEVICE), CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
        }
        let selected = DEVICES.iter().position(|(_, d)| *d == config.device).unwrap_or(0);
        SendMessageW(control(hwnd, ID_DEVICE), CB_SETCURSEL, selected, 0);

        add(hwnd, w!("STATIC"), "GPU メモリ上限", 0, -1, (20, 166, 110, 22));
        add(hwnd, w!("COMBOBOX"), "", combo, ID_GPU_LIMIT, (136, 162, 200, 260));
        for (label, _) in GPU_LIMITS {
            SendMessageW(control(hwnd, ID_GPU_LIMIT), CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
        }
        let selected = GPU_LIMITS.iter().position(|(_, l)| *l == config.gpu_limit_mib).unwrap_or(0);
        SendMessageW(control(hwnd, ID_GPU_LIMIT), CB_SETCURSEL, selected, 0);

        add(hwnd, w!("STATIC"), "CPU スレッド数", 0, -1, (20, 202, 110, 22));
        add(hwnd, w!("COMBOBOX"), "", combo, ID_THREADS, (136, 198, 200, 260));
        let thread_box = control(hwnd, ID_THREADS);
        let auto = wide(&format!("自動（{}）", engine::performance_cores()));
        SendMessageW(thread_box, CB_ADDSTRING, 0, auto.as_ptr() as isize);
        for n in 1..=std::thread::available_parallelism().map_or(1, |n| n.get()) {
            SendMessageW(thread_box, CB_ADDSTRING, 0, wide(&n.to_string()).as_ptr() as isize);
        }
        SendMessageW(thread_box, CB_SETCURSEL, config.cpu_threads.unwrap_or(0), 0);

        let hotkeys = config.hotkeys;
        for (i, label) in HOTKEY_LABELS.iter().enumerate() {
            let y = 244 + i as i32 * 36;
            add(hwnd, w!("STATIC"), label, 0, -1, (20, y + 4, 110, 22));
            let id = ID_HOTKEY + i as i32;
            let box_ = add(hwnd, HOTKEY_CLASSW, "", WS_TABSTOP | WS_BORDER, id, (136, y, 220, 26));
            SendMessageW(box_, HKM_SETRULES, (HKCOMB_NONE | HKCOMB_S) as usize, (HOTKEYF_CONTROL | HOTKEYF_ALT) as isize);
            write_hotkey(box_, hotkeys[i]);
        }
        let check = WS_TABSTOP | BS_AUTOCHECKBOX as u32;
        add(hwnd, w!("BUTTON"), "翻訳している範囲に緑の枠線を表示する", check, ID_SHOW_FRAME, (20, 354, 520, 24));
        add(hwnd, w!("BUTTON"), "翻訳が終わったら枠線を一瞬表示する", check, ID_FLASH_FRAME, (20, 382, 520, 24));
        SendMessageW(control(hwnd, ID_SHOW_FRAME), BM_SETCHECK, config.show_frame as usize, 0);
        SendMessageW(control(hwnd, ID_FLASH_FRAME), BM_SETCHECK, config.flash_frame as usize, 0);
        add(hwnd, w!("STATIC"), "", 0, ID_MESSAGE, (20, 416, 520, 24));
        add(hwnd, w!("STATIC"), "Built with PLaMo", 0, -1, (20, 456, 200, 22));
        add(hwnd, w!("BUTTON"), "閉じる", button, ID_CLOSE, (440, 450, 100, 32));

        fill_models(hwnd);
        refresh(hwnd);
        if !models::is_present(&config.model) {
            set_text(hwnd, ID_MESSAGE, "はじめに、翻訳モデルをダウンロードしてください");
        }
        SetTimer(hwnd, TIMER, TIMER_MS, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}
