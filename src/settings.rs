//! The settings page: what it shows and what its messages do. The window around it belongs to
//! the platform, which hands the page's messages to `on_message` and calls `refresh` every so often.

use crate::config::{self, Hotkey};
use crate::engine::{self, Device};
use crate::i18n::t;
use crate::job::{request_load, request_unload};
use crate::models;
use crate::platform;
use crate::translate::HISTORY;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

pub const PAGE: &str = include_str!("settings.html");

thread_local! {
    static SENT: RefCell<String> = const { RefCell::new(String::new()) };
    static MESSAGE: RefCell<String> = const { RefCell::new(String::new()) };
    static CHOOSING_KEY: Cell<bool> = const { Cell::new(false) };
}

pub fn data_dir() -> PathBuf {
    models::dir().parent().map(PathBuf::from).unwrap_or_default()
}

/// Everything the page shows.
fn state() -> Value {
    let mut config = config::get();
    let entries = models::list();
    if !entries.iter().any(|e| e.file == config.model) {
        // The file has gone, so the choice goes with it.
        config::update(|c| c.model = models::DEFAULT.to_string());
        config = config::get();
    }
    if let Some(error) = models::LAST_ERROR.lock().unwrap().take() {
        MESSAGE.set(t(&error));
    }
    let list: Vec<Value> = entries
        .iter()
        .map(|e| json!({ "file": e.file, "label": e.label, "present": models::is_present(&e.file), "partial": models::has_part(&e.file), "download": models::can_download(&e.file) }))
        .collect();
    let device = match config.device {
        Device::Auto => "auto",
        Device::Gpu => "gpu",
        Device::Cpu => "cpu",
    };
    let engine_state = match engine::STATE.load(Ordering::Relaxed) {
        engine::READY => "ready",
        engine::LOADING => "loading",
        _ => "unloaded",
    };
    let hotkeys: Vec<Value> = config.hotkeys.iter().map(|h| json!({ "mods": h.mods, "key": h.key })).collect();
    json!({
        "title": t("Sonoba の設定"),
        "version": env!("CARGO_PKG_VERSION"),
        "folder": data_dir().display().to_string(),
        "models": list,
        "model": config.model,
        "progress": models::PROGRESS.load(Ordering::Relaxed),
        "downloading": models::DOWNLOADING.lock().unwrap().clone(),
        "korean": { "present": models::is_present(models::KOREAN_OCR), "partial": models::has_part(models::KOREAN_OCR) },
        "engine": engine_state,
        "message": MESSAGE.with_borrow(Clone::clone),
        "device": device,
        "gpu_limit": config.gpu_limit_mib,
        "threads": config.cpu_threads,
        "cpu_auto": engine::performance_cores(),
        "cpu_max": std::thread::available_parallelism().map_or(1, |n| n.get()),
        "hotkeys": hotkeys,
        "default_hotkeys": config::Config::default().hotkeys.iter().map(|h| json!({ "mods": h.mods, "key": h.key })).collect::<Vec<_>>(),
        "show_frame": config.show_frame,
        "flash_frame": config.flash_frame,
        "source": config.source,
        "target": config.target,
        "ui_lang": config.ui_lang,
        "taken": *platform::TAKEN_KEYS.lock().unwrap(),
        "form": config.form,
        "glossary": config.glossary,
        "font": config.font,
        "fonts": platform::fonts(),
        "bold_text": config.bold_text,
        "opacity": config.opacity,
        "keep_history": config.keep_history,
        "autostart": platform::autostart(),
        "platform": if cfg!(target_os = "macos") { "mac" } else { "windows" },
        "intro": !config.license_accepted || (!config.intro_done && !models::is_present(&config.model)),
        "intro_done": config.intro_done,
    })
}

/// Sends the state if it changed since the last time.
pub fn refresh() {
    let text = state().to_string();
    if SENT.with_borrow(|sent| *sent != text) {
        platform::post_to_page(&text);
        SENT.set(text);
    }
}

/// The window has gone: the next one gets everything afresh, and the keys come back.
pub fn closed() {
    SENT.set(String::new());
    stop_choosing_key();
}

/// The window lost the focus while waiting for a key, which it can no longer receive.
pub fn stop_choosing_key() {
    if CHOOSING_KEY.replace(false) {
        platform::resume_hotkeys();
    }
}

fn hotkey_changed(index: usize, new: Hotkey) {
    let mut hotkeys = config::get().hotkeys;
    if index >= hotkeys.len() {
        return;
    }
    hotkeys[index] = new;
    let repeated = new != Hotkey::OFF && (0..hotkeys.len()).any(|i| i != index && hotkeys[i] == new);
    platform::pause_hotkeys(); // its own key must not look taken
    if repeated || (new != Hotkey::OFF && !platform::try_hotkeys(new)) {
        MESSAGE.set(t("そのキーは使えません（別の操作や別のアプリが使っています）"));
    } else {
        config::update(|c| c.hotkeys = hotkeys);
    }
    CHOOSING_KEY.set(false);
    platform::resume_hotkeys();
}

/// The keys go back to the defaults, except that none may land on another action's key.
fn reset_hotkeys(index: Option<usize>) {
    let defaults = config::Config::default().hotkeys;
    match index {
        Some(index) if index < defaults.len() => hotkey_changed(index, defaults[index]),
        Some(_) => {}
        None => {
            config::update(|c| c.hotkeys = defaults);
            platform::resume_hotkeys();
        }
    }
}

/// A changed choice takes effect with the next load, or right away if a model is in memory.
fn choice_changed(change: impl FnOnce(&mut config::Config)) {
    config::update(change);
    if engine::STATE.load(Ordering::Relaxed) != engine::UNLOADED {
        request_unload();
        if models::is_present(&config::get().model) {
            request_load();
        }
    }
}

fn delete_model() {
    let file = config::get().model;
    platform::unload_now();
    engine::wait_until_stopped(); // Windows cannot delete a file that is in use
    if let Err(e) = models::delete(&file) {
        MESSAGE.set(format!("{}{e}", t("削除できません: ")));
    }
}

pub fn on_message(text: &str) {
    let Ok(message) = serde_json::from_str::<Value>(text) else { return };
    let value = &message["value"];
    let flag = value.as_bool().unwrap_or(false);
    let text_value = value.as_str().unwrap_or("").to_string();
    MESSAGE.set(String::new());
    match message["cmd"].as_str().unwrap_or("") {
        "ready" => SENT.set(String::new()),
        "model" => choice_changed(|c| c.model = text_value),
        "device" => choice_changed(|c| {
            c.device = match text_value.as_str() {
                "gpu" => Device::Gpu,
                "cpu" => Device::Cpu,
                _ => Device::Auto,
            }
        }),
        "gpu_limit" => choice_changed(|c| c.gpu_limit_mib = value.as_u64()),
        "threads" => choice_changed(|c| c.cpu_threads = value.as_u64().map(|n| n as usize)),
        "source" => config::update(|c| c.source = text_value),
        "target" => config::update(|c| c.target = text_value),
        "ui_lang" => {
            config::update(|c| c.ui_lang = text_value);
            platform::language_changed();
        }
        "show_frame" => {
            config::update(|c| c.show_frame = flag);
            platform::update_frame();
        }
        "flash_frame" => config::update(|c| c.flash_frame = flag),
        "autostart" => platform::set_autostart(flag),
        "form" => config::update(|c| c.form = text_value),
        "glossary" => {
            let pair = |t: &Value| Some((t[0].as_str()?.trim().to_string(), t[1].as_str()?.trim().to_string()));
            let terms = value.as_array().map_or_else(Vec::new, |terms| terms.iter().filter_map(pair).collect());
            config::update(|c| c.glossary = terms);
        }
        "font" => {
            config::update(|c| c.font = text_value);
            platform::look_changed();
        }
        "bold_text" => {
            config::update(|c| c.bold_text = flag);
            platform::look_changed();
        }
        "opacity" => {
            config::update(|c| c.opacity = value.as_u64().unwrap_or(100).clamp(30, 100) as u8);
            platform::look_changed();
        }
        "keep_history" => {
            config::update(|c| c.keep_history = flag);
            if !flag {
                HISTORY.lock().unwrap().clear();
            }
        }
        "history_clear" => HISTORY.lock().unwrap().clear(),
        "copy" => {
            platform::copy_to_clipboard(&text_value);
        }
        "history" => {
            let history: Vec<(String, String)> = HISTORY.lock().unwrap().iter().cloned().collect();
            platform::post_to_page(&json!({ "history": history }).to_string());
        }
        "intro_done" => config::update(|c| c.intro_done = true),
        "license_accept" => config::update(|c| c.license_accepted = true),
        "close" => platform::close_settings(),
        // The licence binds whoever uses the model, so the guide has them accept it first.
        "download" if config::get().license_accepted => models::start_download(config::get().model),
        "korean_download" => models::start_download(models::KOREAN_OCR.to_string()),
        "korean_delete" => {
            if let Err(e) = models::delete(models::KOREAN_OCR) {
                MESSAGE.set(format!("{}{e}", t("削除できません: ")));
            }
        }
        "cancel" => models::CANCEL.store(true, Ordering::Relaxed),
        "delete" => delete_model(),
        "load" => request_load(),
        "unload" => platform::unload_now(),
        "capture" => {
            CHOOSING_KEY.set(flag);
            if flag { platform::pause_hotkeys() } else { platform::resume_hotkeys() }
        }
        "hotkey_reset" => reset_hotkeys(value.as_u64().map(|i| i as usize)),
        "hotkey_off" => hotkey_changed(value.as_u64().unwrap_or(u64::MAX) as usize, Hotkey::OFF),
        "hotkey" => {
            if let (Some(index), Some(mods), Some(key)) = (message["index"].as_u64(), message["mods"].as_u64(), message["key"].as_u64()) {
                hotkey_changed(index as usize, Hotkey { mods: mods as u32, key: key as u32 });
            }
        }
        "open" => platform::open_link(&text_value),
        _ => {}
    }
    refresh();
}
