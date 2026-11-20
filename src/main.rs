//! clip-ai entry point: single-instance Win32 message loop, global hotkeys,
//! zero-focus-steal background execution, and tray context menu.

#![windows_subsystem = "windows"]

mod audio;
mod cli_runner;
mod clipboard;
mod config;
mod hotkey;
mod logger;
mod markdown_clean;
mod selection;
mod settings_ui;
mod tray_icon;
mod win_util;

use crate::audio::{AudioPlayer, SoundCue};
use crate::cli_runner::{run_with_fallback, CancelHandle, ExecutionOutcome};
use crate::config::{is_autostart_registered, AppConfig, CliKind, HotkeySpec, InputSourceMode};
use crate::hotkey::{
    format_hotkey, register, unregister, HOTKEY_ID_CANCEL, HOTKEY_ID_TRIGGER,
};
use crate::markdown_clean::clean_markdown;
use crate::selection::acquire_prompt;
use crate::settings_ui::{open_or_focus, show_warning, LastRunReport, WM_APP_CONFIG_SAVED};
use crate::tray_icon::{TrayManager, TrayVisualState, WM_APP_TRAY};
use crate::win_util::to_wide;
use std::cell::RefCell;
use std::thread;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage,
    CW_USEDEFAULT, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_COMMAND, WM_DESTROY, WM_HOTKEY,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const ERROR_ALREADY_EXISTS: u32 = 183;
const WM_CONTEXTMENU: u32 = 0x007B;

const WM_APP_JOB_STARTED: u32 = 0x8000 + 10;
const WM_APP_JOB_FINISHED: u32 = 0x8000 + 11;

const TIMER_ANIM_ID: usize = 1;
const TIMER_HEARTBEAT_ID: usize = 2;
const TIMER_SUCCESS_RESET_ID: usize = 3;

const MENU_CANCEL_TASK: usize = 1001;
const MENU_TOGGLE_SOUND: usize = 1002;
const MENU_OPEN_SETTINGS: usize = 1003;
const MENU_EXIT: usize = 1004;
const MENU_TOGGLE_PURE_QA: usize = 1005;
const MENU_PROMOTE_BASE: usize = 1100;
const MENU_INPUT_MODE_BASE: usize = 1200;

struct AppRuntime {
    hwnd: HWND,
    cfg: AppConfig,
    audio: AudioPlayer,
    tray: TrayManager,
    active_trigger: Option<HotkeySpec>,
    active_cancel: Option<HotkeySpec>,
    running_cancel: Option<CancelHandle>,
    pending_restore_clipboard: Option<String>,
    last_source_label: &'static str,
    last_prompt_preview: String,
    anim_frame: u8,
    last_report: LastRunReport,
}

thread_local! {
    static APP: RefCell<Option<AppRuntime>> = const { RefCell::new(None) };
}

fn main() {
    logger::install_panic_hook();
    logger::log_line("INFO", "clip-ai starting");
    let is_autostart = std::env::args().any(|a| a == "--autostart");
    let mutex_name = to_wide("Local\\ClipAI_SingleInstance_Mutex");
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if !is_autostart {
            unsafe {
                show_warning(
                    std::ptr::null_mut(),
                    "clip-ai",
                    "clip-ai 已在系统托盘运行中，无需重复启动。",
                );
            }
        }
        unsafe {
            CloseHandle(mutex);
        }
        return;
    }

    let mut cfg = AppConfig::load();
    cfg.autostart = is_autostart_registered();
    let audio = AudioPlayer::new(cfg.sound_enabled, cfg.sound_volume, &cfg.custom_wavs);

    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null()) as HINSTANCE;
        let class_name = to_wide("ClipAIMessageHost");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(main_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            to_wide("clip-ai").as_ptr(),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            logger::log_line("ERROR", "CreateWindowExW for ClipAIMessageHost failed");
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return;
        }
        logger::log_line("INFO", &format!("Message host window ready: {:?}", hwnd));

        let tray = TrayManager::new(hwnd);
        let runtime = AppRuntime {
            hwnd,
            cfg,
            audio,
            tray,
            active_trigger: None,
            active_cancel: None,
            running_cancel: None,
            pending_restore_clipboard: None,
            last_source_label: "剪贴板",
            last_prompt_preview: String::new(),
            anim_frame: 0,
            last_report: LastRunReport::default(),
        };
        APP.with(|cell| *cell.borrow_mut() = Some(runtime));

        let startup_conflicts = apply_hotkeys_from_cfg();
        if !startup_conflicts.is_empty() {
            let joined = startup_conflicts.join("\r\n");
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.tray.set_state(TrayVisualState::Error);
                    app.audio.play(SoundCue::Error);
                    app.last_report = LastRunReport {
                        timestamp: current_local_hhmmss(),
                        state_label: "快捷键冲突".to_string(),
                        summary: joined.clone(),
                    };
                }
            });
            if !is_autostart {
                show_warning(
                    std::ptr::null_mut(),
                    "clip-ai 快捷键冲突",
                    &format!("{joined}\n\n即将打开设置窗口，请重新指定不冲突的快捷键。"),
                );
                open_settings_dialog();
            }
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        APP.with(|cell| {
            if let Some(app) = cell.borrow_mut().take() {
                unregister(app.hwnd, HOTKEY_ID_TRIGGER);
                unregister(app.hwnd, HOTKEY_ID_CANCEL);
                if let Some(cancel) = app.running_cancel {
                    cancel.cancel();
                }
            }
        });
        if !mutex.is_null() {
            CloseHandle(mutex);
        }
    }
}

fn apply_hotkeys_from_cfg() -> Vec<String> {
    let mut errors = Vec::new();
    APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(app) = borrow.as_mut() else {
            return;
        };
        unregister(app.hwnd, HOTKEY_ID_TRIGGER);
        unregister(app.hwnd, HOTKEY_ID_CANCEL);
        app.active_trigger = None;
        app.active_cancel = None;

        match register(app.hwnd, HOTKEY_ID_TRIGGER, app.cfg.trigger_hotkey) {
            Ok(()) => app.active_trigger = Some(app.cfg.trigger_hotkey),
            Err(e) => errors.push(format!("触发快捷键注册失败: {e}")),
        }

        if let Some(cancel_hk) = app.cfg.cancel_hotkey {
            if cancel_hk == app.cfg.trigger_hotkey {
                errors.push("取消快捷键与触发快捷键相同，已忽略取消快捷键".to_string());
            } else {
                match register(app.hwnd, HOTKEY_ID_CANCEL, cancel_hk) {
                    Ok(()) => app.active_cancel = Some(cancel_hk),
                    Err(e) => errors.push(format!("取消快捷键注册失败: {e}")),
                }
            }
        }
    });
    errors
}

unsafe extern "system" fn main_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            match wparam as i32 {
                HOTKEY_ID_TRIGGER => handle_trigger_hotkey(),
                HOTKEY_ID_CANCEL => handle_cancel_hotkey(),
                _ => {}
            }
            0
        }
        WM_APP_TRAY => {
            let ev = (lparam & 0xFFFF) as u32;
            match ev {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_tray_menu(hwnd),
                WM_LBUTTONDBLCLK => open_settings_dialog(),
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            handle_menu_command(hwnd, (wparam & 0xFFFF) as usize);
            0
        }
        WM_TIMER => {
            handle_timer(hwnd, wparam);
            0
        }
        WM_APP_JOB_STARTED => {
            on_job_started(hwnd, wparam as usize);
            0
        }
        WM_APP_JOB_FINISHED => {
            let boxed = Box::from_raw(lparam as *mut ExecutionOutcome);
            on_job_finished(hwnd, *boxed);
            0
        }
        WM_APP_CONFIG_SAVED => {
            let new_cfg = Box::from_raw(lparam as *mut AppConfig);
            on_config_saved(*new_cfg);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_trigger_hotkey() {
    let launch_data: Option<(HWND, AppConfig, CancelHandle)> = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let app = borrow.as_mut()?;

        if app.running_cancel.is_some() {
            app.audio.play(SoundCue::Busy);
            return None;
        }

        let acquired = match acquire_prompt(
            app.hwnd,
            app.cfg.input_mode,
            app.cfg.hybrid_ctrl_c_fallback,
        ) {
            Ok(a) => a,
            Err(err) => {
                logger::log_line("WARN", &format!("Input acquisition failed ({}): {err}", app.cfg.input_mode.short_name()));
                app.tray.set_state(TrayVisualState::Error);
                app.audio.play(SoundCue::Error);
                app.last_report = LastRunReport {
                    timestamp: current_local_hhmmss(),
                    state_label: format!("出错（{}获取失败）", app.cfg.input_mode.short_name()),
                    summary: err,
                };
                return None;
            }
        };

        logger::log_line(
            "INFO",
            &format!(
                "Prompt acquired via {} | preview=\"{}\"",
                acquired.source_label, acquired.prompt_preview
            ),
        );
        let cancel = CancelHandle::new();
        app.running_cancel = Some(cancel.clone());
        app.pending_restore_clipboard = acquired.restore_clipboard_on_fail;
        app.last_source_label = acquired.source_label;
        app.last_prompt_preview = acquired.prompt_preview;
        let cfg_snapshot = app.cfg.clone();
        let hwnd = app.hwnd;
        Some((hwnd, cfg_snapshot, cancel)).map(|(h, c, ch)| {
            spawn_worker(h, c, acquired.prompt, ch.clone());
            (h, app.cfg.clone(), ch)
        })
    });
    let _ = launch_data;
}

fn spawn_worker(hwnd: HWND, cfg: AppConfig, prompt: String, cancel: CancelHandle) {
    let hwnd_raw = hwnd as usize;
    thread::spawn(move || {
        let mut notified_start = false;
        let outcome = run_with_fallback(&cfg, &prompt, &cancel, |kind| {
            if !notified_start {
                notified_start = true;
                unsafe {
                    PostMessageW(hwnd_raw as HWND, WM_APP_JOB_STARTED, kind as usize, 0);
                }
            }
        });
        let boxed = Box::into_raw(Box::new(outcome));
        unsafe {
            PostMessageW(hwnd_raw as HWND, WM_APP_JOB_FINISHED, 0, boxed as LPARAM);
        }
    });
}

fn handle_cancel_hotkey() {
    APP.with(|cell| {
        if let Some(app) = cell.borrow().as_ref() {
            if let Some(cancel) = app.running_cancel.as_ref() {
                cancel.cancel();
                app.audio.play(SoundCue::Busy);
            }
        }
    });
}

unsafe fn on_job_started(hwnd: HWND, _backend_idx: usize) {
    APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(app) = borrow.as_mut() else {
            return;
        };
        KillTimer(hwnd, TIMER_SUCCESS_RESET_ID);
        app.anim_frame = 0;
        app.tray.set_state(TrayVisualState::Processing(0));
        app.audio.play(SoundCue::Start);
        SetTimer(hwnd, TIMER_ANIM_ID, 350, None);
        if app.cfg.heartbeat_interval_secs > 0 {
            SetTimer(
                hwnd,
                TIMER_HEARTBEAT_ID,
                app.cfg.heartbeat_interval_secs.saturating_mul(1000),
                None,
            );
        }
    });
}

unsafe fn on_job_finished(hwnd: HWND, outcome: ExecutionOutcome) {
    KillTimer(hwnd, TIMER_ANIM_ID);
    KillTimer(hwnd, TIMER_HEARTBEAT_ID);

    APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(app) = borrow.as_mut() else {
            return;
        };
        app.running_cancel = None;
        let source_label = app.last_source_label;
        let prompt_preview = app.last_prompt_preview.clone();
        let backup_clip = app.pending_restore_clipboard.take();

        match outcome {
            ExecutionOutcome::Success {
                backend,
                answer,
                attempt_notes,
                duration_ms,
            } => {
                logger::log_line(
                    "INFO",
                    &format!("Success via {} (source={}) in {}ms: {}", backend.display_name(), source_label, duration_ms, attempt_notes.join(" | ")),
                );
                let final_text = if app.cfg.strip_markdown {
                    clean_markdown(&answer)
                } else {
                    answer
                };
                if final_text.trim().is_empty() {
                    if let Some(ref old) = backup_clip {
                        let _ = clipboard::write_text(hwnd, old);
                    }
                    app.tray.set_state(TrayVisualState::Error);
                    app.audio.play(SoundCue::Error);
                    app.last_report = LastRunReport {
                        timestamp: current_local_hhmmss(),
                        state_label: format!("出错（{source_label} -> 返回结果为空）"),
                        summary: attempt_notes.join(" -> "),
                    };
                    return;
                }
                match clipboard::write_text(hwnd, &final_text) {
                    Ok(()) => {
                        app.tray.set_state(TrayVisualState::Success);
                        app.audio.play(SoundCue::Complete);
                        SetTimer(hwnd, TIMER_SUCCESS_RESET_ID, 4000, None);
                        app.last_report = LastRunReport {
                            timestamp: current_local_hhmmss(),
                            state_label: format!("成功（{source_label} -> {}，{:.1}s）", backend.display_name(), duration_ms as f32 / 1000.0),
                            summary: format!("{} | 提问摘要: \"{}\"", attempt_notes.join(" -> "), prompt_preview),
                        };
                    }
                    Err(cb_err) => {
                        app.tray.set_state(TrayVisualState::Error);
                        app.audio.play(SoundCue::Error);
                        app.last_report = LastRunReport {
                            timestamp: current_local_hhmmss(),
                            state_label: "出错（写入剪贴板失败）".to_string(),
                            summary: cb_err,
                        };
                    }
                }
            }
            ExecutionOutcome::Cancelled {
                attempt_notes,
                duration_ms,
            } => {
                if let Some(ref old) = backup_clip {
                    let _ = clipboard::write_text(hwnd, old);
                }
                app.tray.set_state(TrayVisualState::Idle);
                app.last_report = LastRunReport {
                    timestamp: current_local_hhmmss(),
                    state_label: format!("已取消（{source_label}，{:.1}s）", duration_ms as f32 / 1000.0),
                    summary: if attempt_notes.is_empty() {
                        "用户主动取消任务".to_string()
                    } else {
                        attempt_notes.join(" -> ")
                    },
                };
            }
            ExecutionOutcome::Failed {
                reason,
                attempt_notes,
                duration_ms,
            } => {
                if let Some(ref old) = backup_clip {
                    let _ = clipboard::write_text(hwnd, old);
                }
                logger::log_line(
                    "ERROR",
                    &format!("Failed (source={}) in {}ms: {} | {}", source_label, duration_ms, reason, attempt_notes.join(" | ")),
                );
                app.tray.set_state(TrayVisualState::Error);
                app.audio.play(SoundCue::Error);
                app.last_report = LastRunReport {
                    timestamp: current_local_hhmmss(),
                    state_label: format!("出错（{source_label}，{:.1}s）", duration_ms as f32 / 1000.0),
                    summary: format!(
                        "{} | 提问摘要: \"{}\"",
                        if attempt_notes.is_empty() { reason } else { attempt_notes.join(" -> ") },
                        prompt_preview
                    ),
                };
            }
        }
    });
}

unsafe fn handle_timer(hwnd: HWND, timer_id: WPARAM) {
    APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(app) = borrow.as_mut() else {
            return;
        };
        match timer_id {
            TIMER_ANIM_ID => {
                if app.running_cancel.is_some() {
                    app.anim_frame = (app.anim_frame + 1) % 4;
                    app.tray.set_state(TrayVisualState::Processing(app.anim_frame));
                } else {
                    KillTimer(hwnd, TIMER_ANIM_ID);
                }
            }
            TIMER_HEARTBEAT_ID => {
                if app.running_cancel.is_some() {
                    app.audio.play(SoundCue::Processing);
                } else {
                    KillTimer(hwnd, TIMER_HEARTBEAT_ID);
                }
            }
            TIMER_SUCCESS_RESET_ID => {
                KillTimer(hwnd, TIMER_SUCCESS_RESET_ID);
                if app.tray.current_state() == TrayVisualState::Success && app.running_cancel.is_none() {
                    app.tray.set_state(TrayVisualState::Idle);
                }
            }
            _ => {}
        }
    });
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }

    let (status_line, is_running, sound_on, pure_qa_on, priority_list, trigger_label, input_mode) = APP.with(|cell| {
        let borrow = cell.borrow();
        let app = borrow.as_ref().unwrap();
        let state = app.tray.current_state().label_zh();
        let primary = app.cfg.primary_backend().display_name();
        let mode_short = app.cfg.input_mode.short_name();
        let line = format!("clip-ai | {state} | {primary} | {mode_short}");
        (
            line,
            app.running_cancel.is_some(),
            app.cfg.sound_enabled,
            app.cfg.pure_text_qa_mode,
            app.cfg.backend_priority.clone(),
            format_hotkey(app.cfg.trigger_hotkey),
            app.cfg.input_mode,
        )
    });

    let header_w = to_wide(&format!("{status_line} ({trigger_label})"));
    AppendMenuW(menu, MF_STRING | MF_DISABLED | MF_GRAYED, 0, header_w.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

    // Input source mode submenu
    let mode_sub = CreatePopupMenu();
    for (idx, mode) in InputSourceMode::all().into_iter().enumerate() {
        let flag = if mode == input_mode {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let w = to_wide(mode.label());
        AppendMenuW(mode_sub, flag, MENU_INPUT_MODE_BASE + idx, w.as_ptr());
    }
    AppendMenuW(
        menu,
        MF_POPUP,
        mode_sub as usize,
        to_wide("提示词输入来源").as_ptr(),
    );

    // Quick promote primary CLI submenu
    let sub = CreatePopupMenu();
    for (idx, kind) in CliKind::all().into_iter().enumerate() {
        let is_primary = priority_list
            .iter()
            .find(|e| e.enabled)
            .map(|e| e.kind == kind)
            .unwrap_or(false);
        let flag = if is_primary { MF_STRING | MF_CHECKED } else { MF_STRING };
        let w = to_wide(kind.label());
        AppendMenuW(sub, flag, MENU_PROMOTE_BASE + idx, w.as_ptr());
    }
    AppendMenuW(
        menu,
        MF_POPUP,
        sub as usize,
        to_wide("首选 CLI 后端 (置顶)").as_ptr(),
    );

    let cancel_flags = if is_running {
        MF_STRING
    } else {
        MF_STRING | MF_DISABLED | MF_GRAYED
    };
    AppendMenuW(menu, cancel_flags, MENU_CANCEL_TASK, to_wide("取消当前任务").as_ptr());

    let sound_flags = if sound_on {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | MF_UNCHECKED
    };
    AppendMenuW(menu, sound_flags, MENU_TOGGLE_SOUND, to_wide("启用提示音").as_ptr());
    let qa_flags = if pure_qa_on {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | MF_UNCHECKED
    };
    AppendMenuW(
        menu,
        qa_flags,
        MENU_TOGGLE_PURE_QA,
        to_wide("纯文本安全问答模式 (禁工具)").as_ptr(),
    );
    AppendMenuW(menu, MF_STRING, MENU_OPEN_SETTINGS, to_wide("设置...").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, MENU_EXIT, to_wide("退出").as_ptr());

    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    // Win32 requirement only on explicit user tray menu open so menu closes on outside click.
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_LEFTALIGN | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, std::ptr::null());
    DestroyMenu(menu);
}

unsafe fn handle_menu_command(hwnd: HWND, cmd: usize) {
    match cmd {
        MENU_CANCEL_TASK => handle_cancel_hotkey(),
        MENU_TOGGLE_SOUND => {
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.cfg.sound_enabled = !app.cfg.sound_enabled;
                    app.audio.set_enabled(app.cfg.sound_enabled);
                    let _ = app.cfg.save();
                }
            });
        }
        MENU_TOGGLE_PURE_QA => {
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.cfg.pure_text_qa_mode = !app.cfg.pure_text_qa_mode;
                    let _ = app.cfg.save();
                }
            });
        }
        MENU_OPEN_SETTINGS => open_settings_dialog(),
        MENU_EXIT => {
            DestroyWindow(hwnd);
        }
        id if id >= MENU_PROMOTE_BASE && id < MENU_PROMOTE_BASE + CliKind::all().len() => {
            let kind = CliKind::all()[id - MENU_PROMOTE_BASE];
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.cfg.promote_backend(kind);
                    let _ = app.cfg.save();
                }
            });
        }
        id if id >= MENU_INPUT_MODE_BASE
            && id < MENU_INPUT_MODE_BASE + InputSourceMode::all().len() =>
        {
            let mode = InputSourceMode::all()[id - MENU_INPUT_MODE_BASE];
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.cfg.input_mode = mode;
                    let _ = app.cfg.save();
                }
            });
            logger::log_line("INFO", &format!("Input mode switched via tray to {}", mode.short_name()));
        }
        _ => {}
    }
}

fn open_settings_dialog() {
    let snapshot = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let app = borrow.as_mut()?;
        if app.tray.current_state() == TrayVisualState::Error && app.running_cancel.is_none() {
            app.tray.set_state(TrayVisualState::Idle);
        }
        Some((
            app.hwnd,
            app.cfg.clone(),
            app.active_trigger,
            app.active_cancel,
            app.audio.clone(),
            app.last_report.clone(),
        ))
    });
    if let Some((hwnd, cfg, trig, cancel, audio, report)) = snapshot {
        open_or_focus(hwnd, &cfg, trig, cancel, audio, report);
    }
}

fn on_config_saved(new_cfg: AppConfig) {
    APP.with(|cell| {
        if let Some(app) = cell.borrow_mut().as_mut() {
            app.cfg = new_cfg;
            app.audio.reload(
                app.cfg.sound_enabled,
                app.cfg.sound_volume,
                &app.cfg.custom_wavs,
            );
            if app.running_cancel.is_none() {
                app.tray.set_state(TrayVisualState::Idle);
            }
        }
    });
    let errs = apply_hotkeys_from_cfg();
    if !errs.is_empty() {
        APP.with(|cell| {
            if let Some(app) = cell.borrow_mut().as_mut() {
                app.tray.set_state(TrayVisualState::Error);
                app.audio.play(SoundCue::Error);
                app.last_report = LastRunReport {
                    timestamp: current_local_hhmmss(),
                    state_label: "快捷键冲突".to_string(),
                    summary: errs.join("\r\n"),
                };
            }
        });
    }
}

fn current_local_hhmmss() -> String {
    unsafe {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
        format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
    }
}
