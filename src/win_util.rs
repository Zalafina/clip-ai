//! Shared Win32 UTF-16 string and diagnostic helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::GetLastError;

/// Convert a Rust `&str` into a NUL-terminated UTF-16 `Vec<u16>` for Win32 `*W` APIs.
pub fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Convert a NUL-terminated UTF-16 slice into a `String`, stopping at the first NUL.
pub fn from_wide_slice(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Read the current thread's last Win32 error code.
pub fn last_win32_error() -> u32 {
    unsafe { GetLastError() }
}
