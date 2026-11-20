//! Win32 CF_UNICODETEXT clipboard reader and writer with contention retries.

use crate::win_util::from_wide_slice;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

fn open_clipboard_with_retry(hwnd: HWND) -> Result<(), String> {
    for attempt in 0..12 {
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return Ok(());
        }
        if attempt < 11 {
            thread::sleep(Duration::from_millis(15));
        }
    }
    Err("无法打开系统剪贴板（可能正被其他软件占用）".to_string())
}

/// Read UTF-16 text from the Windows clipboard without altering foreground focus.
pub fn read_text(hwnd: HWND) -> Result<String, String> {
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Err("剪贴板中没有文本内容（可能为空或为图片/文件）".to_string());
        }
        open_clipboard_with_retry(hwnd)?;
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return Err("读取剪贴板文本句柄失败".to_string());
            }
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return Err("锁定剪贴板内存失败".to_string());
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let text = from_wide_slice(slice);
            GlobalUnlock(handle);
            if text.trim().is_empty() {
                return Err("剪贴板文本为空白内容".to_string());
            }
            Ok(text)
        })();
        CloseClipboard();
        result
    }
}

/// Write UTF-16 text with normalized CRLF line endings to the Windows clipboard.
pub fn write_text(hwnd: HWND, text: &str) -> Result<(), String> {
    let crlf = text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n");
    let wide: Vec<u16> = OsStr::new(&crlf)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        open_clipboard_with_retry(hwnd)?;
        let result = (|| {
            if EmptyClipboard() == 0 {
                return Err("清空剪贴板失败".to_string());
            }
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
            if hmem.is_null() {
                return Err("分配剪贴板内存失败".to_string());
            }
            let dst = GlobalLock(hmem) as *mut u16;
            if dst.is_null() {
                return Err("锁定剪贴板写入内存失败".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(hmem);

            if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                return Err("写入剪贴板数据失败".to_string());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}
