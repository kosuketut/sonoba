//! Saved settings: one JSON file, rewritten whenever something changes.

use crate::engine::{self, Device, Options};
use crate::models;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MOD_ALT: u32 = 1;
const MOD_CONTROL: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hotkey {
    pub mods: u32, // MOD_ALT and so on, as RegisterHotKey takes them
    pub key: u32,  // a virtual key code
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub model: String, // a file in the models folder
    pub device: Device,
    pub cpu_threads: Option<usize>, // None: every performance core
    pub gpu_limit_mib: Option<u64>, // None: whatever fits
    pub show_frame: bool, // a thin frame around the region while a job runs
    pub flash_frame: bool, // the frame for a moment whenever a translation finishes
    pub source: String, // what to read: "auto", "ja" (translate Japanese too) or "ko"
    pub target: String, // a language code of the model, "ja(easy)" included
    pub hotkeys: [Hotkey; 3], // one shot, live, clear
}

impl Default for Config {
    fn default() -> Self {
        let ctrl_alt = |key: char| Hotkey { mods: MOD_CONTROL | MOD_ALT, key: key as u32 };
        Self {
            model: models::DEFAULT.to_string(),
            device: Device::Auto,
            cpu_threads: None,
            gpu_limit_mib: None,
            show_frame: false,
            flash_frame: false,
            source: "auto".to_string(),
            target: "ja".to_string(),
            hotkeys: [ctrl_alt('T'), ctrl_alt('R'), ctrl_alt('X')],
        }
    }
}

impl Config {
    /// Whatever is missing or malformed falls back to its default.
    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let Ok(saved) = serde_json::from_str::<Value>(text) else { return config };
        if let Some(model) = saved["model"].as_str() {
            config.model = model.to_string();
        }
        config.device = match saved["device"].as_str() {
            Some("cpu") => Device::Cpu,
            Some("gpu") => Device::Gpu,
            _ => Device::Auto,
        };
        config.cpu_threads = saved["cpu_threads"].as_u64().map(|n| (n as usize).max(1));
        config.gpu_limit_mib = saved["gpu_limit_mib"].as_u64();
        config.show_frame = saved["show_frame"].as_bool().unwrap_or(false);
        config.flash_frame = saved["flash_frame"].as_bool().unwrap_or(false);
        if let Some(source) = saved["source"].as_str() {
            config.source = source.to_string();
        }
        if let Some(target) = saved["target"].as_str() {
            config.target = target.to_string();
        }
        for (slot, name) in config.hotkeys.iter_mut().zip(["once", "live", "clear"]) {
            if let (Some(mods), Some(key)) = (saved[name]["mods"].as_u64(), saved[name]["key"].as_u64()) {
                *slot = Hotkey { mods: mods as u32, key: key as u32 };
            }
        }
        config
    }

    fn to_json(&self) -> String {
        let hotkey = |h: &Hotkey| json!({ "mods": h.mods, "key": h.key });
        let device = match self.device {
            Device::Auto => "auto",
            Device::Gpu => "gpu",
            Device::Cpu => "cpu",
        };
        let saved = json!({
            "model": self.model,
            "device": device,
            "cpu_threads": self.cpu_threads,
            "gpu_limit_mib": self.gpu_limit_mib,
            "show_frame": self.show_frame,
            "flash_frame": self.flash_frame,
            "source": self.source,
            "target": self.target,
            "once": hotkey(&self.hotkeys[0]),
            "live": hotkey(&self.hotkeys[1]),
            "clear": hotkey(&self.hotkeys[2]),
        });
        serde_json::to_string_pretty(&saved).unwrap_or_default()
    }

    pub fn threads(&self) -> usize {
        self.cpu_threads.unwrap_or_else(engine::performance_cores).max(1)
    }

    pub fn engine_options(&self) -> Options {
        Options { device: self.device, threads: self.threads(), gpu_limit_mib: self.gpu_limit_mib }
    }
}

/// A copy unpacked from the zip keeps everything in its `data` folder, so that deleting the folder
/// removes the app. Where it cannot write there (Program Files), the user's profile is used.
pub fn portable_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::current_exe().ok()?.parent()?.join("data");
        let probe = dir.join(".write-test");
        std::fs::write(&probe, "").ok()?;
        let _ = std::fs::remove_file(probe);
        Some(dir)
    })
    .as_ref()
}

fn path() -> PathBuf {
    let profile = || PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("screen-translate");
    portable_dir().cloned().unwrap_or_else(profile).join("config.json")
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

pub fn get() -> Config {
    let mut config = CONFIG.lock().unwrap();
    config.get_or_insert_with(|| Config::parse(&std::fs::read_to_string(path()).unwrap_or_default())).clone()
}

pub fn update(change: impl FnOnce(&mut Config)) {
    let mut config = get();
    change(&mut config);
    if let Some(dir) = path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path(), config.to_json()); // a failed save only loses it at the next start
    *CONFIG.lock().unwrap() = Some(config);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let mut config = Config::default();
        config.model = "custom.gguf".into();
        config.device = Device::Cpu;
        config.cpu_threads = Some(3);
        config.gpu_limit_mib = Some(1536);
        config.show_frame = true;
        config.flash_frame = true;
        config.source = "ko".into();
        config.target = "ja(easy)".into();
        config.hotkeys[1] = Hotkey { mods: 6, key: 0x70 };
        assert_eq!(Config::parse(&config.to_json()), config);
    }

    #[test]
    fn a_damaged_file_means_defaults() {
        assert_eq!(Config::parse("{ not json"), Config::default());
        assert_eq!(Config::parse("{\"device\": 7}"), Config::default());
    }
}
