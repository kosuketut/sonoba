//! Screen OCR and translation, driven by global hotkeys.
//!
//! Everything that decides what to translate and how is shared; `platform` is the part that
//! talks to the operating system: windows, hotkeys, the screen, the tray or menu bar.
#![windows_subsystem = "windows"]

mod config;
mod engine;
mod i18n;
mod job;
mod models;
mod ocr;
mod patch;
mod settings;
mod translate;

#[cfg(windows)]
#[path = "win/mod.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "mac/mod.rs"]
mod platform;

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

fn main() {
    platform::main();
}
