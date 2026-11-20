//! Prompt acquisition from UI Automation text selection, guarded Ctrl+C fallback,
//! and system clipboard according to the configured `InputSourceMode`.

use crate::clipboard;
use crate::config::InputSourceMode;
use crate::win_util::from_wide_slice;
use std::cell::RefCell;
use std::mem::size_of;
use std::thread;
use std::time::Duration;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    UIA_TextPatternId,
};
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetCursorPos, GetForegroundWindow};

thread_local! {
    static UIA_CLIENT: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAcquisition {
    pub prompt: String,
    pub prompt_preview: String,
    pub source_label: &'static str,
    /// When `SmartHybrid` used Ctrl+C to grab a selection, store the previous clipboard text so it
    /// can be restored if the AI execution fails or is cancelled.
    pub restore_clipboard_on_fail: Option<String>,
}

pub fn build_preview(text: &str, max_chars: usize) -> String {
    let single_line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = single_line.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

/// Acquire the user prompt according to `mode`.
/// In `SelectionOnly`, Ctrl+C simulation is NEVER invoked regardless of `allow_ctrl_c`
/// and stale clipboard content is NEVER read.
pub fn acquire_prompt(
    hwnd: HWND,
    mode: InputSourceMode,
    allow_ctrl_c: bool,
) -> Result<PromptAcquisition, String> {
    acquire_prompt_with(
        mode,
        allow_ctrl_c,
        try_read_uia_selection,
        || try_read_selection_via_ctrl_c(hwnd),
        || clipboard::read_text(hwnd),
    )
}

fn acquire_prompt_with<FUia, FCtrlC, FClip>(
    mode: InputSourceMode,
    allow_ctrl_c: bool,
    read_uia: FUia,
    read_ctrl_c: FCtrlC,
    read_clip: FClip,
) -> Result<PromptAcquisition, String>
where
    FUia: FnOnce() -> Option<String>,
    FCtrlC: FnOnce() -> Option<(String, Option<String>)>,
    FClip: FnOnce() -> Result<String, String>,
{
    match mode {
        InputSourceMode::ClipboardOnly => {
            let prompt = read_clip()?;
            let prompt_preview = build_preview(&prompt, 45);
            Ok(PromptAcquisition {
                prompt,
                prompt_preview,
                source_label: "剪贴板",
                restore_clipboard_on_fail: None,
            })
        }
        InputSourceMode::SelectionOnly => {
            if let Some(prompt) = read_uia() {
                let prompt_preview = build_preview(&prompt, 45);
                Ok(PromptAcquisition {
                    prompt,
                    prompt_preview,
                    source_label: "划词 (UIA)",
                    restore_clipboard_on_fail: None,
                })
            } else {
                Err("未检测到高亮划词（仅划词模式不会触发 Ctrl+C 复制，也不会读取旧剪贴板）".to_string())
            }
        }
        InputSourceMode::SmartHybrid => {
            if let Some(prompt) = read_uia() {
                let prompt_preview = build_preview(&prompt, 45);
                return Ok(PromptAcquisition {
                    prompt,
                    prompt_preview,
                    source_label: "划词 (UIA)",
                    restore_clipboard_on_fail: None,
                });
            }
            if allow_ctrl_c {
                if let Some((prompt, old_clip)) = read_ctrl_c() {
                    let prompt_preview = build_preview(&prompt, 45);
                    return Ok(PromptAcquisition {
                        prompt,
                        prompt_preview,
                        source_label: "划词 (Ctrl+C)",
                        restore_clipboard_on_fail: old_clip,
                    });
                }
            }
            match read_clip() {
                Ok(prompt) => {
                    let prompt_preview = build_preview(&prompt, 45);
                    Ok(PromptAcquisition {
                        prompt,
                        prompt_preview,
                        source_label: "剪贴板(无划词回退)",
                        restore_clipboard_on_fail: None,
                    })
                }
                Err(_) => Err("未检测到高亮划词，且剪贴板为空或不包含有效文本".to_string()),
            }
        }
    }
}

/// Try reading selected text via Windows UI Automation from the focused element (and ancestors),
/// gently falling back to a single cursor-point lookup (`ElementFromPoint`) without subtree scans.
pub fn try_read_uia_selection() -> Option<String> {
    let uia = get_or_init_uia()?;
    unsafe {
        if let Ok(focused) = uia.GetFocusedElement() {
            if let Some(text) = walk_ancestors_for_selection(&uia, focused) {
                return Some(text);
            }
        }

        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            let win_pt = windows::Win32::Foundation::POINT { x: pt.x, y: pt.y };
            if let Ok(under_cursor) = uia.ElementFromPoint(win_pt) {
                if let Some(text) = walk_ancestors_for_selection(&uia, under_cursor) {
                    return Some(text);
                }
            }
        }
        None
    }
}

unsafe fn walk_ancestors_for_selection(
    uia: &IUIAutomation,
    start: IUIAutomationElement,
) -> Option<String> {
    let mut cur = start;
    let walker = uia.ControlViewWalker().ok();
    for depth in 0..4 {
        if let Some(text) = read_text_pattern_selection(&cur) {
            return Some(text);
        }
        if depth == 3 {
            break;
        }
        let Some(ref w) = walker else {
            break;
        };
        match w.GetParentElement(&cur) {
            Ok(parent) => cur = parent,
            Err(_) => break,
        }
    }
    None
}

unsafe fn read_text_pattern_selection(elem: &IUIAutomationElement) -> Option<String> {
    let pattern: IUIAutomationTextPattern = elem.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
    let ranges = pattern.GetSelection().ok()?;
    let count = ranges.Length().ok()?;
    if count <= 0 {
        return None;
    }

    let mut parts = Vec::new();
    for idx in 0..count {
        if let Ok(range) = ranges.GetElement(idx) {
            if let Ok(bstr) = range.GetText(-1) {
                let s = bstr.to_string();
                if !s.trim().is_empty() {
                    parts.push(s);
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn get_or_init_uia() -> Option<IUIAutomation> {
    UIA_CLIENT.with(|cell| {
        if let Some(existing) = cell.borrow().as_ref() {
            return Some(existing.clone());
        }
        unsafe {
            // Ignore RPC_E_CHANGED_MODE if thread already initialized COM in another apartment.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let created: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            *cell.borrow_mut() = Some(created.clone());
            Some(created)
        }
    })
}

/// Guarded Ctrl+C fallback for apps where UIA `TextPattern` is unavailable.
/// Skips console/terminal windows so an empty selection never sends SIGINT.
fn try_read_selection_via_ctrl_c(hwnd: HWND) -> Option<(String, Option<String>)> {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() {
        return None;
    }
    let class_name = read_window_class(fg);
    if is_terminal_window_class(&class_name) {
        return None;
    }

    let seq_before = unsafe { GetClipboardSequenceNumber() };
    let old_clip = clipboard::read_text(hwnd).ok();

    send_clean_ctrl_c();

    // Poll briefly for the foreground app to populate CF_UNICODETEXT.
    for _ in 0..14 {
        thread::sleep(Duration::from_millis(15));
        let seq_now = unsafe { GetClipboardSequenceNumber() };
        if seq_now != seq_before {
            if let Ok(new_text) = clipboard::read_text(hwnd) {
                return Some((new_text, old_clip));
            }
            return None;
        }
    }
    None
}

fn read_window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        String::new()
    } else {
        from_wide_slice(&buf[..len as usize])
    }
}

pub fn is_terminal_window_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "ConsoleWindowClass" | "CASCADIA_HOSTING_WINDOW_CLASS" | "mintty"
    )
}

fn send_clean_ctrl_c() {
    let modifiers_to_release = [
        VK_LMENU,
        VK_RMENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LWIN,
        VK_RWIN,
        VK_LCONTROL,
        VK_RCONTROL,
    ];

    let mut release_inputs = Vec::new();
    unsafe {
        for vk in modifiers_to_release {
            if GetAsyncKeyState(vk as i32) < 0 {
                release_inputs.push(key_input(vk, true));
            }
        }
        if !release_inputs.is_empty() {
            SendInput(
                release_inputs.len() as u32,
                release_inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            );
            thread::sleep(Duration::from_millis(15));
        }

        let copy_seq = [
            key_input(VK_CONTROL, false),
            key_input(0x43 /* 'C' */, false),
            key_input(0x43 /* 'C' */, true),
            key_input(VK_CONTROL, true),
        ];
        SendInput(
            copy_seq.len() as u32,
            copy_seq.as_ptr(),
            size_of::<INPUT>() as i32,
        );
    }
}

fn key_input(vk: u16, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_only_never_invokes_ctrl_c_or_clipboard() {
        let mut ctrl_c_called = false;
        let mut clip_called = false;
        let res = acquire_prompt_with(
            InputSourceMode::SelectionOnly,
            true,
            || None,
            || {
                ctrl_c_called = true;
                Some(("from_ctrl_c".to_string(), None))
            },
            || {
                clip_called = true;
                Ok("from_clip".to_string())
            },
        );
        assert!(!ctrl_c_called);
        assert!(!clip_called);
        assert!(res.is_err());

        let ok = acquire_prompt_with(
            InputSourceMode::SelectionOnly,
            true,
            || Some("selected_text".to_string()),
            || unreachable!("ctrl+c must never run in SelectionOnly"),
            || unreachable!("clipboard must never run in SelectionOnly"),
        )
        .unwrap();
        assert_eq!(ok.prompt, "selected_text");
        assert_eq!(ok.source_label, "划词 (UIA)");
    }

    #[test]
    fn smart_hybrid_prefers_uia_then_ctrl_c_then_clipboard() {
        let via_uia = acquire_prompt_with(
            InputSourceMode::SmartHybrid,
            true,
            || Some("uia_hit".to_string()),
            || unreachable!("should not invoke ctrl+c when UIA succeeds"),
            || unreachable!("should not read clipboard when UIA succeeds"),
        )
        .unwrap();
        assert_eq!(via_uia.prompt, "uia_hit");
        assert_eq!(via_uia.source_label, "划词 (UIA)");

        let via_ctrl_c = acquire_prompt_with(
            InputSourceMode::SmartHybrid,
            true,
            || None,
            || Some(("ctrl_c_hit".to_string(), Some("old".to_string()))),
            || unreachable!("should not fallback to clipboard when Ctrl+C grabbed selection"),
        )
        .unwrap();
        assert_eq!(via_ctrl_c.prompt, "ctrl_c_hit");
        assert_eq!(via_ctrl_c.source_label, "划词 (Ctrl+C)");
        assert_eq!(via_ctrl_c.restore_clipboard_on_fail.as_deref(), Some("old"));

        let via_clip = acquire_prompt_with(
            InputSourceMode::SmartHybrid,
            true,
            || None,
            || None,
            || Ok("clip_fallback line1\nline2".to_string()),
        )
        .unwrap();
        assert_eq!(via_clip.prompt, "clip_fallback line1\nline2");
        assert_eq!(via_clip.source_label, "剪贴板(无划词回退)");
        assert_eq!(via_clip.prompt_preview, "clip_fallback line1 line2");
    }

    #[test]
    fn preview_truncates_long_multiline_text() {
        let long = "第一行   第二行\n".repeat(10);
        let p = build_preview(&long, 12);
        assert!(p.ends_with("..."));
        assert!(!p.contains('\n'));
    }

    #[test]
    fn detects_terminal_window_classes_safely() {
        assert!(is_terminal_window_class("ConsoleWindowClass"));
        assert!(is_terminal_window_class("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(is_terminal_window_class("mintty"));
        assert!(!is_terminal_window_class("Notepad"));
        assert!(!is_terminal_window_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn uia_probe_executes_without_crashing() {
        let _ = try_read_uia_selection();
    }
}
