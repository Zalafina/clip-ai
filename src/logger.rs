//! Lightweight rotating file logger and panic hook for clip-ai diagnostics.

use crate::config::AppConfig;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 256 * 1024;
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Return the log file path alongside `config.json` (`%APPDATA%\clip-ai\clip-ai.log` by default).
pub fn log_path() -> PathBuf {
    AppConfig::config_path()
        .parent()
        .map(|p| p.join("clip-ai.log"))
        .unwrap_or_else(|| PathBuf::from("clip-ai.log"))
}

/// Append a timestamped line to `clip-ai.log`, rotating if the file exceeds `MAX_LOG_BYTES`.
pub fn log_line(level: &str, msg: &str) {
    let _guard = LOG_LOCK.lock().ok();
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let old = path.with_extension("log.old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, &old);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let ts = local_timestamp();
        let _ = writeln!(f, "[{ts}] [{level}] {msg}");
    }
}

/// Install a panic hook that writes fatal panic details into `clip-ai.log`.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        };
        log_line("PANIC", &format!("{payload} at {location}"));
    }));
}

fn local_timestamp() -> String {
    unsafe {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
        )
    }
}
