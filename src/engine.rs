//! The translation engine: one llama-server process, which holds the model.
//!
//! Stopping the process is what unloads a model, so nothing stays behind in either kind of
//! memory. The numbers behind this choice are in the README.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::ptr::null;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub const URL: &str = "http://127.0.0.1:11437";
const PORT: &str = "11437";
pub const SLOTS: usize = 4; // requests at once: twice the speed of one; eight add little
const CONTEXT_PER_SLOT: usize = 2048;
const BATCH: &str = "1024";
const HYBRID_LAYERS: &str = "12";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Device {
    Gpu,
    Hybrid,
    Cpu,
}

impl Device {
    fn gpu_layers(self) -> &'static str {
        match self {
            Device::Gpu => "99",
            Device::Hybrid => HYBRID_LAYERS,
            Device::Cpu => "0",
        }
    }
}

/// A release ships the engine beside the app. Without one, the copy inside Ollama will do.
fn engine_command(device: Device) -> Result<Command, String> {
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("engine").join("llama-server.exe")));
    if let Some(path) = beside.filter(|path| path.exists()) {
        return Ok(Command::new(path));
    }
    let ollama = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Programs\\Ollama\\lib\\ollama");
    let server = ollama.join("llama-server.exe");
    if !server.exists() {
        return Err("翻訳エンジンが見つかりません".to_string());
    }
    let mut command = Command::new(server);
    let vulkan = ollama.join("vulkan");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = vec![ollama.clone(), vulkan.clone()];
    paths.extend(std::env::split_paths(&path));
    command.env("PATH", std::env::join_paths(paths).map_err(|e| e.to_string())?);
    command.current_dir(ollama);
    if device != Device::Cpu {
        command.env("GGML_BACKEND_PATH", vulkan.join("ggml-vulkan.dll"));
    }
    Ok(command)
}

pub struct Engine {
    child: Child,
    job: isize,
}

impl Engine {
    /// Starts loading; `ready` tells when it has finished. The job object takes the process
    /// down with the app, however the app ends.
    pub fn start(model: &Path, device: Device) -> Result<Self, String> {
        let threads = std::thread::available_parallelism().map_or(4, |n| (n.get() / 3).clamp(2, 8));
        let child = engine_command(device)?
            .arg("--model")
            .arg(model)
            .args(["--host", "127.0.0.1", "--port", PORT, "--no-webui", "--offline", "--log-disable"])
            .args(["-c", &(SLOTS * CONTEXT_PER_SLOT).to_string(), "-np", &SLOTS.to_string()])
            .args(["-b", BATCH, "-ub", BATCH, "-t", &threads.to_string(), "-ngl", device.gpu_layers()])
            .args(["--cache-ram", "0", "--load-mode", "none"]) // no mapping: a third of the memory
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("翻訳エンジンを起動できません: {e}"))?;
        let job = unsafe {
            let job = CreateJobObjectW(null(), null());
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits as *const _ as *const _, size);
            AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
            job as isize
        };
        Ok(Self { child, job })
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.job as HANDLE) };
        let _ = self.child.wait();
    }
}
