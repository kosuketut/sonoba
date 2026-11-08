//! The translation engine: one llama-server process, which holds the model.
//!
//! Stopping the process is what unloads a model, so nothing stays behind in either kind of
//! memory. The numbers behind this choice are in the README.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
#[cfg(windows)]
use {
    std::os::windows::io::AsRawHandle,
    std::os::windows::process::CommandExt,
    std::ptr::null,
    std::sync::OnceLock,
    windows_sys::Win32::Foundation::HANDLE,
    windows_sys::Win32::System::JobObjects::*,
    windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    },
    windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
};

pub const SLOTS: usize = 4; // requests at once: twice the speed of one; eight add little
const CONTEXT_PER_SLOT: usize = 2048;
const BATCH: &str = "1024";
const AUTO_GPU_MARGIN_MIB: u64 = 256; // left free for everything else when no limit is set

pub const UNLOADED: u8 = 0;
pub const LOADING: u8 = 1;
pub const READY: u8 = 2;
pub static STATE: AtomicU8 = AtomicU8::new(UNLOADED);

/// Blocks until the engine has let go of its model file.
pub fn wait_until_stopped() {
    for _ in 0..250 {
        if STATE.load(Ordering::Relaxed) == UNLOADED {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Device {
    Auto, // as many layers as fit in GPU memory, the rest on the CPU; no GPU at all works too
    Gpu,
    Cpu,
}

/// Everything about how the engine runs. A change means restarting it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    pub device: Device,
    pub threads: usize,
    pub gpu_limit_mib: Option<u64>, // Auto only
}

/// A helper process opens no console window of its own.
pub fn quiet(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// The cores worth computing on: physical ones, and of those the performance cores, since a
/// thread on an efficiency core holds the others back. One core counts once, however many
/// hardware threads it has. This fits any machine, so nothing here assumes a core count.
#[cfg(target_os = "macos")]
pub fn performance_cores() -> usize {
    let count = |name: &str| {
        let name = std::ffi::CString::new(name).ok()?;
        let (mut value, mut size) = (0i32, size_of::<i32>());
        let ok = unsafe { sysctlbyname(name.as_ptr(), &mut value as *mut i32 as *mut _, &mut size, std::ptr::null_mut(), 0) } == 0;
        (ok && value > 0).then_some(value as usize)
    };
    // Apple silicon numbers its performance cores level 0; an Intel Mac has only hw.physicalcpu.
    count("hw.perflevel0.physicalcpu").or_else(|| count("hw.physicalcpu")).unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sysctlbyname(name: *const std::ffi::c_char, old: *mut std::ffi::c_void, old_len: *mut usize, new: *mut std::ffi::c_void, new_len: usize) -> i32;
}

#[cfg(windows)]
pub fn performance_cores() -> usize {
    let fallback = || std::thread::available_parallelism().map_or(1, |n| n.get());
    unsafe {
        let mut bytes = 0u32;
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut bytes);
        let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)]; // u64 for the alignment
        let start = buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        if bytes == 0 || GetLogicalProcessorInformationEx(RelationProcessorCore, start, &mut bytes) == 0 {
            return fallback();
        }
        let mut classes = Vec::new();
        let mut offset = 0;
        while offset < bytes as usize {
            let info = &*((start as *const u8).add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            classes.push(info.Anonymous.Processor.EfficiencyClass);
            offset += info.Size as usize;
        }
        let best = classes.iter().copied().max().unwrap_or(0);
        Some(classes.iter().filter(|&&class| class == best).count()).filter(|&n| n > 0).unwrap_or_else(fallback)
    }
}

/// The MiB a line of `--list-devices` reports free: "Vulkan0: Some GPU (11996 MiB, 11228 MiB free)".
/// A device with no memory of its own, such as the Mac's BLAS, is not the GPU.
fn free_mib(listing: &str) -> Option<u64> {
    let mib = |text: &str| text.trim().split(' ').next()?.parse::<u64>().ok();
    let line = listing.lines().map(str::trim_end).filter(|line| line.ends_with("MiB free)")).find(|line| {
        let total = line.rsplit_once(" (").and_then(|(_, sizes)| mib(sizes.split(", ").next()?));
        total.is_some_and(|total| total > 0)
    })?;
    mib(line.rsplit(", ").next()?)
}

/// The engine has no limit on GPU memory, only a margin to leave free, so one becomes the other.
fn gpu_margin_mib(limit: Option<u64>, free: Option<u64>) -> u64 {
    match (limit, free) {
        (Some(limit), Some(free)) => free.saturating_sub(limit),
        _ => AUTO_GPU_MARGIN_MIB,
    }
}

/// A release ships the engine inside the app. Without one, llama.cpp from Homebrew will do.
#[cfg(target_os = "macos")]
fn engine_command(_device: Device) -> Result<Command, String> {
    let exe = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)).unwrap_or_default();
    let mut places = vec![exe.join("engine/llama-server"), exe.join("../Resources/engine/llama-server")];
    places.extend(["/opt/homebrew/bin/llama-server", "/usr/local/bin/llama-server"].map(PathBuf::from));
    places.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|dir| dir.join("llama-server")));
    let server = places.into_iter().find(|path| path.is_file()).ok_or("翻訳エンジンが見つかりません")?;
    Ok(Command::new(server))
}

/// A release ships the engine beside the app. Without one, the copy inside Ollama will do.
#[cfg(windows)]
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

/// Ends a child process when the app ends, however it ends. A Mac has no job objects, so a small
/// shell waits for the app to go and then stops the child; it leaves a child that ended first alone.
#[cfg(not(windows))]
pub fn tie_to_app(child: &Child) {
    let script = format!("while kill -0 {app} && kill -0 {child}; do sleep 1; done 2>/dev/null; kill -0 {app} 2>/dev/null || kill {child} 2>/dev/null", app = std::process::id(), child = child.id());
    let watcher = Command::new("/bin/sh").args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut watcher) = watcher {
        std::thread::spawn(move || watcher.wait()); // no zombie once it is done
    }
}

/// Puts a child process in a job that ends with the app, however the app ends.
#[cfg(windows)]
pub fn tie_to_app(child: &Child) {
    static JOB: OnceLock<isize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(null(), null());
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits as *const _ as *const _, size);
        job as isize
    });
    unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
}

pub struct Engine {
    child: Child,
    pub url: String,
}

impl Engine {
    /// Starts loading, on a port that is free right now.
    pub fn start(model: &Path, options: Options) -> Result<Self, String> {
        let device = options.device;
        let layers = match device {
            Device::Auto => "auto",
            Device::Gpu => "all",
            Device::Cpu => "0",
        };
        let mut command = engine_command(device)?;
        if device == Device::Auto {
            let free = options.gpu_limit_mib.and_then(|_| {
                let listing = quiet(engine_command(device).ok()?.arg("--list-devices")).output().ok()?;
                let text = [listing.stdout, listing.stderr].concat();
                free_mib(&String::from_utf8_lossy(&text))
            });
            command.args(["--fit-target", &gpu_margin_mib(options.gpu_limit_mib, free).to_string()]);
        }
        let port = TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
        let child = quiet(&mut command)
            .arg("--model")
            .arg(model)
            .args(["--host", "127.0.0.1", "--port", &port.to_string(), "--no-webui", "--offline", "--log-disable"])
            .args(["-c", &(SLOTS * CONTEXT_PER_SLOT).to_string(), "-np", &SLOTS.to_string()])
            .args(["-b", BATCH, "-ub", BATCH, "-t", &options.threads.to_string(), "-ngl", layers])
            .args(["--cache-ram", "0", "--load-mode", "none"]) // no mapping: a third of the memory
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("翻訳エンジンを起動できません: {e}"))?;
        tie_to_app(&child);
        STATE.store(LOADING, Ordering::Relaxed);
        Ok(Self { child, url: format!("http://127.0.0.1:{port}") })
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_memory_comes_from_the_device_listing() {
        let listing = "Available devices:\r\n  Vulkan0: NVIDIA GeForce RTX 4070 Ti (11996 MiB, 11228 MiB free)\r\n";
        assert_eq!(free_mib(listing), Some(11228));
        assert_eq!(free_mib("Available devices:\n"), None);
        let mac = "Available devices:\n  BLAS: Accelerate (0 MiB, 0 MiB free)\n  MTL0: Apple M3 (18186 MiB, 18185 MiB free)\n";
        assert_eq!(free_mib(mac), Some(18185));
    }

    #[test]
    fn a_limit_becomes_the_margin_left_free() {
        assert_eq!(gpu_margin_mib(Some(1500), Some(11228)), 9728);
        assert_eq!(gpu_margin_mib(Some(4000), Some(1000)), 0);
        assert_eq!(gpu_margin_mib(None, Some(11228)), AUTO_GPU_MARGIN_MIB);
        assert_eq!(gpu_margin_mib(Some(1500), None), AUTO_GPU_MARGIN_MIB);
    }

    #[test]
    fn the_core_count_fits_the_machine() {
        let cores = performance_cores();
        assert!(cores >= 1 && cores <= std::thread::available_parallelism().unwrap().get());
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        STATE.store(UNLOADED, Ordering::Relaxed);
    }
}
