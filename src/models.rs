//! The models: PLaMo 3 Translate in its four sizes, plus any GGUF file dropped into the folder.

use crate::i18n::t;
use serde_json::Value;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub const DEFAULT: &str = "plamo-3-translate-2b-Q4_K_M.gguf";
const CATALOG: [(&str, &str); 4] = [("0.8b", "軽量、0.6GB"), ("2b", "標準、1.8GB"), ("8b", "高品質、5.3GB"), ("31b", "最高品質、19.5GB")];
const SOURCE: &str = "https://huggingface.co";

pub const VERIFYING: i32 = 100;
/// Percent while a download runs, then VERIFYING; -1 when there is none.
pub static PROGRESS: AtomicI32 = AtomicI32::new(-1);
pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// How a model expects its prompt, which is all the app needs to know about it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Plamo,
    Chat, // anything else: the template inside the GGUF file, plus a request to translate
}

pub fn format_of(file: &str) -> Format {
    if file.starts_with("plamo-3-translate") { Format::Plamo } else { Format::Chat }
}

pub struct Entry {
    pub file: String,
    pub label: String,
}

fn catalog_file(size: &str) -> String {
    format!("plamo-3-translate-{size}-Q4_K_M.gguf")
}

/// The catalog, then whatever else the folder holds.
pub fn list() -> Vec<Entry> {
    let mut entries: Vec<Entry> = CATALOG
        .iter()
        .map(|(size, note)| Entry { file: catalog_file(size), label: format!("PLaMo 3 Translate {} ({})", size.to_uppercase(), t(note)) })
        .collect();
    if let Ok(folder) = std::fs::read_dir(dir()) {
        for file in folder.flatten().filter_map(|f| f.file_name().into_string().ok()) {
            if file.ends_with(".gguf") && !entries.iter().any(|e| e.file == file) {
                entries.push(Entry { label: file.clone(), file });
            }
        }
    }
    entries
}

pub fn dir() -> PathBuf {
    let profile = || PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("screen-translate");
    crate::config::portable_dir().cloned().unwrap_or_else(profile).join("models")
}

pub fn path(file: &str) -> PathBuf {
    dir().join(file)
}

pub fn is_present(file: &str) -> bool {
    path(file).exists()
}

fn part(file: &str) -> PathBuf {
    path(&format!("{file}.part"))
}

/// A download that stopped half way, which the next one picks up.
pub fn has_part(file: &str) -> bool {
    part(file).exists()
}

pub fn can_download(file: &str) -> bool {
    CATALOG.iter().any(|(size, _)| catalog_file(size) == file)
}

pub fn delete(file: &str) -> std::io::Result<()> {
    if has_part(file) {
        std::fs::remove_file(part(file))?;
    }
    if is_present(file) {
        std::fs::remove_file(path(file))?;
    }
    Ok(())
}

fn tool(name: &str) -> Command {
    let mut command = Command::new(name); // curl and certutil come with Windows
    command.stdin(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    command
}

/// (size, sha-256) of a file, from the repository's listing.
fn expected(repo: &str, file: &str) -> Result<(u64, String), String> {
    let url = format!("{SOURCE}/api/models/{repo}/tree/main");
    let listing = tool("curl").args(["-sSfL", &url]).output().map_err(|e| e.to_string())?;
    let listing: Value = serde_json::from_slice(&listing.stdout).map_err(|_| "モデルの一覧を取得できません".to_string())?;
    let entry = listing.as_array().and_then(|files| files.iter().find(|f| f["path"] == file));
    let size = entry.and_then(|f| f["size"].as_u64());
    let hash = entry.and_then(|f| f["lfs"]["oid"].as_str());
    size.zip(hash.map(str::to_string)).ok_or_else(|| "モデルの情報が見つかりません".to_string())
}

fn sha256(file: &PathBuf) -> Result<String, String> {
    let output = tool("certutil").arg("-hashfile").arg(file).arg("SHA256").output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let is_hash = |line: &&str| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit());
    text.lines().map(str::trim).find(is_hash).map(str::to_lowercase).ok_or_else(|| "ハッシュを計算できません".to_string())
}

fn download(file: &str) -> Result<(), String> {
    let size = CATALOG.iter().find(|(s, _)| catalog_file(s) == file).map(|(s, _)| *s).ok_or("このモデルはダウンロードできません")?;
    let repo = format!("pfnet/plamo-3-translate-{size}-GGUF");
    let (total, hash) = expected(&repo, file)?;
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let part = part(file); // kept when cancelled, and picked up again by -C -
    let url = format!("{SOURCE}/{repo}/resolve/main/{file}");
    if std::fs::metadata(&part).map_or(0, |m| m.len()) < total {
        let mut curl = tool("curl").args(["-sSfL", "-C", "-", "-o"]).arg(&part).arg(&url).stdout(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        crate::engine::tie_to_app(&curl);
        loop {
            if let Some(status) = curl.try_wait().map_err(|e| e.to_string())? {
                if !status.success() {
                    return Err("ダウンロードに失敗しました".to_string());
                }
                break;
            }
            if CANCEL.swap(false, Ordering::Relaxed) {
                let _ = curl.kill();
                let _ = curl.wait();
                return Ok(());
            }
            let got = std::fs::metadata(&part).map_or(0, |m| m.len());
            PROGRESS.store((got * 99 / total.max(1)) as i32, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    PROGRESS.store(VERIFYING, Ordering::Relaxed);
    if sha256(&part)? != hash {
        let _ = std::fs::remove_file(&part);
        return Err("ダウンロードしたファイルが壊れています".to_string());
    }
    std::fs::rename(&part, path(file)).map_err(|e| e.to_string())
}

pub fn start_download(file: String) {
    PROGRESS.store(0, Ordering::Relaxed);
    CANCEL.store(false, Ordering::Relaxed);
    std::thread::spawn(move || {
        *LAST_ERROR.lock().unwrap() = download(&file).err();
        PROGRESS.store(-1, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_decides_the_prompt_format() {
        assert_eq!(format_of(DEFAULT), Format::Plamo);
        assert_eq!(format_of("plamo-3-translate-8b-Q3_K_M.gguf"), Format::Plamo);
        assert_eq!(format_of("gemma-4-q4.gguf"), Format::Chat);
    }

    #[test]
    fn the_catalog_covers_every_size_and_the_default() {
        assert_eq!(list().iter().take(4).filter(|e| can_download(&e.file)).count(), 4);
        assert!(can_download(DEFAULT));
        assert!(!can_download("gemma-4-q4.gguf"));
    }
}
