//! Win32 global hotkey registration, validation, conflict probing, and key formatting.

use crate::config::HotkeySpec;
use crate::win_util::last_win32_error;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

pub const HOTKEY_ID_TRIGGER: i32 = 1;
pub const HOTKEY_ID_CANCEL: i32 = 2;
const HOTKEY_ID_PROBE: i32 = 99;
const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

/// Validate whether a hotkey combination is safe and reasonable as a global shortcut.
pub fn validate_spec(spec: HotkeySpec) -> Result<(), String> {
    if spec.vk == 0 {
        return Err("请按下有效的主键位".to_string());
    }
    let mods = spec.modifiers & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN);
    // Require at least one modifier unless it's an F13..F24 key or Pause/ScrollLock.
    let is_extended_fn = (0x7C..=0x87).contains(&spec.vk) || spec.vk == 0x13 || spec.vk == 0x91;
    if mods == 0 && !is_extended_fn {
        return Err("全局快捷键至少需要包含 Ctrl / Alt / Shift / Win 中的一个修饰键".to_string());
    }

    // Disallow standard universal editing/OS combos that would break normal desktop usage.
    if mods == MOD_CONTROL && matches!(spec.vk, 0x41 | 0x43 | 0x56 | 0x58 | 0x5A | 0x59 | 0x53) {
        return Err(format!(
            "{} 为系统高频编辑快捷键，请换用包含 Alt 或 Shift 的组合",
            format_hotkey(spec)
        ));
    }
    if mods == MOD_ALT && matches!(spec.vk, 0x09 | 0x73 | 0x20) {
        return Err(format!(
            "{} 为 Windows 系统窗口快捷键，请换用其他组合",
            format_hotkey(spec)
        ));
    }
    Ok(())
}

/// Register a hotkey on `hwnd` with `MOD_NOREPEAT` so holding keys does not fire repeatedly.
pub fn register(hwnd: HWND, id: i32, spec: HotkeySpec) -> Result<(), String> {
    validate_spec(spec)?;
    let mods = (spec.modifiers & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN)) | MOD_NOREPEAT;
    let ok = unsafe { RegisterHotKey(hwnd, id, mods, spec.vk) };
    if ok != 0 {
        return Ok(());
    }
    let err = last_win32_error();
    let label = format_hotkey(spec);
    if err == ERROR_HOTKEY_ALREADY_REGISTERED {
        Err(format!("快捷键 {label} 已被其他软件或系统占用，存在冲突！"))
    } else {
        Err(format!("注册快捷键 {label} 失败（Windows 错误码 {err}），可能与系统保留快捷键冲突"))
    }
}

pub fn unregister(hwnd: HWND, id: i32) {
    unsafe {
        UnregisterHotKey(hwnd, id);
    }
}

/// Probe whether `spec` can be registered without disturbing active trigger/cancel hotkeys.
pub fn probe_conflict(
    hwnd: HWND,
    spec: HotkeySpec,
    active_trigger: Option<HotkeySpec>,
    active_cancel: Option<HotkeySpec>,
) -> Result<(), String> {
    validate_spec(spec)?;
    // If identical to a hotkey we ourselves already hold, it is available to keep.
    if active_trigger == Some(spec) || active_cancel == Some(spec) {
        return Ok(());
    }
    register(hwnd, HOTKEY_ID_PROBE, spec)?;
    unregister(hwnd, HOTKEY_ID_PROBE);
    Ok(())
}

/// Format a `HotkeySpec` into a human-readable label like `Ctrl + Alt + Q`.
pub fn format_hotkey(spec: HotkeySpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    if spec.modifiers & MOD_CONTROL != 0 {
        parts.push("Ctrl".to_string());
    }
    if spec.modifiers & MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if spec.modifiers & MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if spec.modifiers & MOD_WIN != 0 {
        parts.push("Win".to_string());
    }
    parts.push(vk_name(spec.vk));
    parts.join(" + ")
}

/// Check whether a virtual-key code represents a pure modifier key rather than a primary key.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    )
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 => format!("{}", (vk as u8) as char),
        0x41..=0x5A => format!("{}", (vk as u8) as char),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x08 => "Backspace".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x13 => "Pause".to_string(),
        0x14 => "CapsLock".to_string(),
        0x1B => "Esc".to_string(),
        0x20 => "Space".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x91 => "ScrollLock".to_string(),
        0xBA => ";".to_string(),
        0xBB => "=".to_string(),
        0xBC => ",".to_string(),
        0xBD => "-".to_string(),
        0xBE => ".".to_string(),
        0xBF => "/".to_string(),
        0xC0 => "`".to_string(),
        0xDB => "[".to_string(),
        0xDC => "\\".to_string(),
        0xDD => "]".to_string(),
        0xDE => "'".to_string(),
        other => format!("VK(0x{other:02X})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_validates_hotkeys() {
        let q = HotkeySpec {
            modifiers: MOD_CONTROL | MOD_ALT,
            vk: 0x51,
        };
        assert_eq!(format_hotkey(q), "Ctrl + Alt + Q");
        assert!(validate_spec(q).is_ok());

        let bare_a = HotkeySpec {
            modifiers: 0,
            vk: 0x41,
        };
        assert!(validate_spec(bare_a).is_err());

        let ctrl_c = HotkeySpec {
            modifiers: MOD_CONTROL,
            vk: 0x43,
        };
        assert!(validate_spec(ctrl_c).is_err());
    }
}
