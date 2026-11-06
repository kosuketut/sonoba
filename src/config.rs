//! Saved settings: one JSON file, rewritten whenever something changes.

use crate::engine::Device;
use crate::models;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

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
    pub hotkeys: [Hotkey; 3], // one shot, live, clear
}

impl Default for Config {
    fn default() -> Self {
        let ctrl_alt = |key: char| Hotkey { mods: MOD_CONTROL | MOD_ALT, key: key as u32 };
        Self { model: models::DEFAULT.to_string(), device: Device::Gpu, hotkeys: [ctrl_alt('T'), ctrl_alt('R'), ctrl_alt('X')] }
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
            Some("hybrid") => Device::Hybrid,
            _ => Device::Gpu,
        };
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
            Device::Gpu => "gpu",
            Device::Hybrid => "hybrid",
            Device::Cpu => "cpu",
        };
        let saved = json!({
            "model": self.model,
            "device": device,
            "once": hotkey(&self.hotkeys[0]),
            "live": hotkey(&self.hotkeys[1]),
            "clear": hotkey(&self.hotkeys[2]),
        });
        serde_json::to_string_pretty(&saved).unwrap_or_default()
    }
}

fn path() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("screen-translate\\config.json")
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
        config.device = Device::Hybrid;
        config.hotkeys[1] = Hotkey { mods: 6, key: 0x70 };
        assert_eq!(Config::parse(&config.to_json()), config);
    }

    #[test]
    fn a_damaged_file_means_defaults() {
        assert_eq!(Config::parse("{ not json"), Config::default());
        assert_eq!(Config::parse("{\"device\": 7}"), Config::default());
    }
}
