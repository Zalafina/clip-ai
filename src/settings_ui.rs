//! Native Win32 Settings window with DPI scaling, hotkey conflict capture,
//! CLI priority ordering, audio preview, and last-run diagnostics.

use crate::audio::{AudioPlayer, SoundCue};
use crate::cli_runner::{detect_all_backends, BackendDetection};
use crate::config::{
    apply_autostart, AppConfig, CliKind, HotkeySpec, InputSourceMode, TaskPreset,
    TranslateTargetLang, DEFAULT_SYSTEM_INSTRUCTION,
};
use crate::hotkey::{format_hotkey, is_modifier_vk, probe_conflict};
use crate::logger;
use crate::win_util::{from_wide_slice, to_wide};
use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetStockObject, HFONT, DEFAULT_GUI_FONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, LoadIconW,
    MessageBoxW, PostMessageW, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX,
    BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL,
    CBN_SELCHANGE, CBS_DROPDOWNLIST, EN_CHANGE, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_READONLY, GWLP_USERDATA, GWLP_WNDPROC, LB_ADDSTRING, LB_GETCURSEL, LB_RESETCONTENT,
    LB_SETCURSEL, LBN_SELCHANGE, MB_ICONWARNING, MB_OK, SM_CXSCREEN, SM_CYSCREEN, SW_RESTORE,
    SW_SHOW, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_SETFONT, WM_SYSKEYDOWN,
    WNDCLASSW, WNDPROC, WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CONTROLPARENT,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

pub const WM_APP_CONFIG_SAVED: u32 = 0x8000 + 20;

const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;

// Control IDs
const ID_EDIT_TRIGGER_HK: i32 = 101;
const ID_BTN_CHECK_TRIGGER: i32 = 102;
const ID_EDIT_CANCEL_HK: i32 = 103;
const ID_BTN_CLEAR_CANCEL: i32 = 104;
const ID_LBL_HK_STATUS: i32 = 105;
const ID_CMB_INPUT_MODE: i32 = 106;
const ID_CHK_HYBRID_CTRL_C: i32 = 107;

const ID_LIST_PRIORITY: i32 = 201;
const ID_BTN_TOGGLE_BACKEND: i32 = 202;
const ID_BTN_MOVE_UP: i32 = 203;
const ID_BTN_MOVE_DOWN: i32 = 204;
const ID_BTN_REFRESH_DETECT: i32 = 205;
const ID_CHK_FALLBACK: i32 = 206;
const ID_CHK_PURE_QA: i32 = 207;
const ID_CHK_SKIP_PERM: i32 = 208;

const ID_EDIT_CLI_PATH: i32 = 210;
const ID_BTN_BROWSE_CLI: i32 = 211;
const ID_EDIT_MODEL: i32 = 212;
const ID_EDIT_EFFORT: i32 = 213;
const ID_EDIT_EXTRA_ARGS: i32 = 214;
const ID_CHK_CUSTOM_STDIN: i32 = 215;

const ID_CHK_SOUND: i32 = 301;
const ID_EDIT_VOLUME: i32 = 302;
const ID_EDIT_HEARTBEAT: i32 = 303;
const ID_EDIT_TIMEOUT: i32 = 304;
const ID_CHK_STRIP_MD: i32 = 305;
const ID_CHK_AUTOSTART: i32 = 306;
const ID_CMB_WAV_CUE: i32 = 307;
const ID_EDIT_WAV_PATH: i32 = 308;
const ID_BTN_BROWSE_WAV: i32 = 309;
const ID_BTN_TEST_SOUND: i32 = 310;

const ID_EDIT_PROMPT_PREFIX: i32 = 401;
const ID_BTN_RESET_PROMPT: i32 = 402;
const ID_CMB_TASK_PRESET: i32 = 403;
const ID_CMB_TARGET_LANG: i32 = 404;
const ID_EDIT_DIAGNOSTICS: i32 = 501;
const ID_BTN_OPEN_LOG: i32 = 502;

const ID_BTN_SAVE: i32 = 901;
const ID_BTN_CANCEL: i32 = 902;

#[derive(Debug, Clone, Default)]
pub struct LastRunReport {
    pub timestamp: String,
    pub state_label: String,
    pub summary: String,
}

struct SettingsWindowState {
    main_hwnd: HWND,
    preview_audio: Option<AudioPlayer>,
    draft: AppConfig,
    active_trigger: Option<HotkeySpec>,
    active_cancel: Option<HotkeySpec>,
    detections: Vec<BackendDetection>,
    selected_priority_idx: usize,
    selected_wav_idx: usize,
    hfont: HFONT,
    orig_hk_proc: WNDPROC,
    diagnostics: LastRunReport,
}

thread_local! {
    static SETTINGS_HWND: RefCell<HWND> = const { RefCell::new(std::ptr::null_mut()) };
    static SETTINGS_STATE: RefCell<Option<Box<SettingsWindowState>>> = const { RefCell::new(None) };
    // Separate Cell avoids ever holding a RefCell borrow across SetWindowTextW / SendMessageW.
    static SUPPRESS_EVENTS: Cell<bool> = const { Cell::new(false) };
}

pub fn open_or_focus(
    main_hwnd: HWND,
    current_cfg: &AppConfig,
    active_trigger: Option<HotkeySpec>,
    active_cancel: Option<HotkeySpec>,
    _audio: AudioPlayer,
    diagnostics: LastRunReport,
) {
    let existing = SETTINGS_HWND.with(|cell| *cell.borrow());
    if !existing.is_null() {
        unsafe {
            ShowWindow(existing, SW_RESTORE);
            SetForegroundWindow(existing);
        }
        return;
    }

    logger::log_line("INFO", "Opening settings dialog");
    let detections = detect_all_backends(current_cfg);
    let state = Box::new(SettingsWindowState {
        main_hwnd,
        preview_audio: None,
        draft: current_cfg.clone(),
        active_trigger,
        active_cancel,
        detections,
        selected_priority_idx: 0,
        selected_wav_idx: 0,
        hfont: std::ptr::null_mut(),
        orig_hk_proc: None,
        diagnostics,
    });
    SETTINGS_STATE.with(|cell| *cell.borrow_mut() = Some(state));

    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null()) as HINSTANCE;
        let app_icon = LoadIconW(hinst, 1 as *const u16);
        let class_name = to_wide("ClipAISettingsWindow");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(settings_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: app_icon,
            hCursor: std::ptr::null_mut(),
            hbrBackground: 16 as _, // COLOR_BTNFACE + 1
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForSystem();
        let w = mul_dpi(720, dpi);
        let h = mul_dpi(825, dpi);
        let sx = GetSystemMetrics(SM_CXSCREEN);
        let sy = GetSystemMetrics(SM_CYSCREEN);
        let x = ((sx - w) / 2).max(0);
        let y = ((sy - h) / 2).max(0);

        let title = to_wide("clip-ai 设置");
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN | WS_VISIBLE,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if !hwnd.is_null() {
            SETTINGS_HWND.with(|cell| *cell.borrow_mut() = hwnd);
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        } else {
            SETTINGS_STATE.with(|cell| *cell.borrow_mut() = None);
        }
    }
}

fn mul_dpi(px: i32, dpi: u32) -> i32 {
    ((px as i64 * dpi.max(96) as i64) / 96) as i32
}

unsafe extern "system" fn settings_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            build_controls(hwnd);
            populate_all_fields(hwnd);
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u32;
            handle_command(hwnd, id, code);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().take() {
                    if !st.hfont.is_null() {
                        DeleteObject(st.hfont);
                    }
                }
            });
            SETTINGS_HWND.with(|cell| *cell.borrow_mut() = std::ptr::null_mut());
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn build_controls(hwnd: HWND) {
    let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForSystem();
    let s = |v: i32| mul_dpi(v, dpi);
    let hinst = GetModuleHandleW(std::ptr::null()) as HINSTANCE;

    let font_height = -mul_dpi(13, dpi);
    let font_face = to_wide("Microsoft YaHei UI");
    let mut hfont = CreateFontW(
        font_height,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        font_face.as_ptr(),
    );
    if hfont.is_null() {
        hfont = GetStockObject(DEFAULT_GUI_FONT as i32) as HFONT;
    }
    SETTINGS_STATE.with(|cell| {
        if let Some(st) = cell.borrow_mut().as_mut() {
            st.hfont = hfont;
        }
    });

    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let cw = rc.right - rc.left;
    let margin = s(14);
    let gw = cw - margin * 2;

    // 1. Hotkeys & Input Source Group
    let mut y = s(10);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "全局快捷键与提示词输入来源（存在冲突时立即提示）", WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32, margin, y, gw, s(122), 0);
    create_ctrl(hwnd, hinst, hfont, "STATIC", "触发问答快捷键:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(26), s(108), s(20), 0);
    let hk_trig = create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_READONLY as u32, margin + s(122), y + s(23), s(170), s(24), ID_EDIT_TRIGGER_HK);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "检测", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + s(298), y + s(22), s(52), s(25), ID_BTN_CHECK_TRIGGER);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "取消任务快捷键:", WS_CHILD | WS_VISIBLE, margin + s(366), y + s(26), s(108), s(20), 0);
    let hk_cancel = create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_READONLY as u32, margin + s(476), y + s(23), s(144), s(24), ID_EDIT_CANCEL_HK);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "禁用", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + s(624), y + s(22), s(44), s(25), ID_BTN_CLEAR_CANCEL);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "点击上方输入框直接按下新组合键即可录入", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(54), gw - s(24), s(20), ID_LBL_HK_STATUS);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "提示词输入来源:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(84), s(102), s(20), 0);
    let cmb_mode = create_ctrl(hwnd, hinst, hfont, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32, margin + s(116), y + s(81), s(278), s(140), ID_CMB_INPUT_MODE);
    for mode in InputSourceMode::all() {
        let wl = to_wide(mode.label());
        SendMessageW(cmb_mode, CB_ADDSTRING, 0, wl.as_ptr() as LPARAM);
    }
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "UIA无选区时允许Ctrl+C(跳过终端)", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(404), y + s(83), gw - s(416), s(22), ID_CHK_HYBRID_CTRL_C);

    let orig_proc = GetWindowLongPtrW(hk_trig, GWLP_WNDPROC);
    SETTINGS_STATE.with(|cell| {
        if let Some(st) = cell.borrow_mut().as_mut() {
            st.orig_hk_proc = std::mem::transmute(orig_proc);
        }
    });
    SetWindowLongPtrW(hk_trig, GWLP_USERDATA, ID_EDIT_TRIGGER_HK as isize);
    SetWindowLongPtrW(hk_trig, GWLP_WNDPROC, hotkey_edit_proc as *const () as isize);
    SetWindowLongPtrW(hk_cancel, GWLP_USERDATA, ID_EDIT_CANCEL_HK as isize);
    SetWindowLongPtrW(hk_cancel, GWLP_WNDPROC, hotkey_edit_proc as *const () as isize);

    // 2. CLI Priority & Backend Details Group
    y += s(130);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "CLI 后端优先级排序与参数（默认 agy 优先）", WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32, margin, y, gw, s(246), 0);
    create_ctrl(hwnd, hinst, hfont, "LISTBOX", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WS_VSCROLL | 0x0001, margin + s(12), y + s(24), gw - s(134), s(80), ID_LIST_PRIORITY);
    let bx = margin + gw - s(112);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "启用/禁用", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, bx, y + s(22), s(100), s(24), ID_BTN_TOGGLE_BACKEND);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "上移", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, bx, y + s(50), s(48), s(24), ID_BTN_MOVE_UP);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "下移", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, bx + s(52), y + s(50), s(48), s(24), ID_BTN_MOVE_DOWN);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "刷新本机检测", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, bx, y + s(78), s(100), s(24), ID_BTN_REFRESH_DETECT);

    create_ctrl(hwnd, hinst, hfont, "BUTTON", "失败时自动回退下一个可用 CLI", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(12), y + s(110), s(225), s(22), ID_CHK_FALLBACK);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "纯文本安全问答 (禁止本地工具)", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(242), y + s(110), s(212), s(22), ID_CHK_PURE_QA);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "跳过权限审批 (需关安全模式)", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(460), y + s(110), gw - s(472), s(22), ID_CHK_SKIP_PERM);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "程序路径/命令:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(142), s(102), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(116), y + s(139), gw - s(208), s(24), ID_EDIT_CLI_PATH);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "浏览...", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + gw - s(84), y + s(138), s(72), s(25), ID_BTN_BROWSE_CLI);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "模型(选填):", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(174), s(84), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(98), y + s(171), s(210), s(24), ID_EDIT_MODEL);
    create_ctrl(hwnd, hinst, hfont, "STATIC", "推理强度(如 low):", WS_CHILD | WS_VISIBLE, margin + s(320), y + s(174), s(118), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(442), y + s(171), s(100), s(24), ID_EDIT_EFFORT);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "stdin传参", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(552), y + s(173), s(116), s(22), ID_CHK_CUSTOM_STDIN);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "额外参数/模板:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(206), s(102), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(116), y + s(203), gw - s(128), s(24), ID_EDIT_EXTRA_ARGS);

    // 3. Audio, Output & Prompt Group
    y += s(254);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "提示音、输出清理与功能模式提示词", WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32, margin, y, gw, s(226), 0);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "启用提示音", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(12), y + s(24), s(104), s(22), ID_CHK_SOUND);
    create_ctrl(hwnd, hinst, hfont, "STATIC", "音量(0-100):", WS_CHILD | WS_VISIBLE, margin + s(124), y + s(26), s(86), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(212), y + s(23), s(46), s(24), ID_EDIT_VOLUME);
    create_ctrl(hwnd, hinst, hfont, "STATIC", "处理中间隔音(秒,0=关):", WS_CHILD | WS_VISIBLE, margin + s(268), y + s(26), s(148), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(418), y + s(23), s(46), s(24), ID_EDIT_HEARTBEAT);
    create_ctrl(hwnd, hinst, hfont, "STATIC", "超时(秒,0=不限):", WS_CHILD | WS_VISIBLE, margin + s(476), y + s(26), s(112), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(592), y + s(23), s(56), s(24), ID_EDIT_TIMEOUT);

    let cmb_wav = create_ctrl(hwnd, hinst, hfont, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32, margin + s(12), y + s(55), s(134), s(150), ID_CMB_WAV_CUE);
    for label in ["开始提示音", "处理中提示音", "完成提示音", "错误提示音", "忙音/取消音"] {
        let wl = to_wide(label);
        SendMessageW(cmb_wav, CB_ADDSTRING, 0, wl.as_ptr() as LPARAM);
    }
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32, margin + s(154), y + s(55), gw - s(318), s(24), ID_EDIT_WAV_PATH);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "浏览.wav", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + gw - s(156), y + s(54), s(72), s(25), ID_BTN_BROWSE_WAV);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "试听", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + gw - s(78), y + s(54), s(66), s(25), ID_BTN_TEST_SOUND);

    create_ctrl(hwnd, hinst, hfont, "BUTTON", "轻量清理常见 Markdown 符号（保留代码内容）", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(12), y + s(86), s(340), s(22), ID_CHK_STRIP_MD);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "开机自启动 (Windows 登录时静默启动)", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32, margin + s(360), y + s(86), s(290), s(22), ID_CHK_AUTOSTART);

    create_ctrl(hwnd, hinst, hfont, "STATIC", "功能模式:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(116), s(68), s(20), 0);
    let cmb_preset = create_ctrl(hwnd, hinst, hfont, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32, margin + s(82), y + s(113), s(220), s(170), ID_CMB_TASK_PRESET);
    for preset in TaskPreset::all() {
        let wl = to_wide(preset.label());
        SendMessageW(cmb_preset, CB_ADDSTRING, 0, wl.as_ptr() as LPARAM);
    }
    create_ctrl(hwnd, hinst, hfont, "STATIC", "翻译目标语言:", WS_CHILD | WS_VISIBLE, margin + s(314), y + s(116), s(90), s(20), 0);
    let cmb_lang = create_ctrl(hwnd, hinst, hfont, "COMBOBOX", "", WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32, margin + s(406), y + s(113), gw - s(418), s(200), ID_CMB_TARGET_LANG);
    for lang in TranslateTargetLang::all() {
        let wl = to_wide(lang.label());
        SendMessageW(cmb_lang, CB_ADDSTRING, 0, wl.as_ptr() as LPARAM);
    }

    create_ctrl(hwnd, hinst, hfont, "STATIC", "生效提示词:", WS_CHILD | WS_VISIBLE, margin + s(12), y + s(148), s(96), s(20), 0);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "恢复默认", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + s(12), y + s(174), s(88), s(24), ID_BTN_RESET_PROMPT);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WS_VSCROLL | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32, margin + s(116), y + s(146), gw - s(128), s(68), ID_EDIT_PROMPT_PREFIX);

    // 4. Diagnostics Group
    y += s(234);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "最近一次运行状态与诊断", WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32, margin, y, gw, s(88), 0);
    create_ctrl(hwnd, hinst, hfont, "EDIT", "", WS_CHILD | WS_VISIBLE | WS_BORDER | WS_VSCROLL | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_READONLY as u32, margin + s(12), y + s(22), gw - s(116), s(54), ID_EDIT_DIAGNOSTICS);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "打开日志", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + gw - s(96), y + s(22), s(84), s(26), ID_BTN_OPEN_LOG);

    // Bottom Action Buttons
    y += s(98);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "保存并应用", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32, margin + gw - s(212), y, s(102), s(30), ID_BTN_SAVE);
    create_ctrl(hwnd, hinst, hfont, "BUTTON", "取消", WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32, margin + gw - s(102), y, s(90), s(30), ID_BTN_CANCEL);
}

unsafe fn create_ctrl(
    parent: HWND,
    hinst: HINSTANCE,
    hfont: HFONT,
    class_name: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
) -> HWND {
    let wc = to_wide(class_name);
    let wt = to_wide(text);
    let ctrl = CreateWindowExW(
        0,
        wc.as_ptr(),
        wt.as_ptr(),
        style,
        x,
        y,
        w,
        h,
        parent,
        id as isize as _,
        hinst,
        std::ptr::null(),
    );
    if !ctrl.is_null() && !hfont.is_null() {
        SendMessageW(ctrl, WM_SETFONT, hfont as WPARAM, 1);
    }
    ctrl
}

unsafe extern "system" fn hotkey_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let orig_proc = SETTINGS_STATE.with(|cell| cell.borrow().as_ref().and_then(|s| s.orig_hk_proc));

    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = (wparam & 0xFF) as u32;
            if is_modifier_vk(vk) {
                return 0;
            }
            let ctrl_down = GetKeyState(VK_CONTROL as i32) < 0;
            let alt_down = GetKeyState(VK_MENU as i32) < 0;
            let shift_down = GetKeyState(VK_SHIFT as i32) < 0;
            let win_down = GetKeyState(VK_LWIN as i32) < 0 || GetKeyState(VK_RWIN as i32) < 0;

            let ctrl_id = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as i32;
            if !ctrl_down
                && !alt_down
                && !shift_down
                && !win_down
                && (vk == VK_BACK as u32 || vk == VK_DELETE as u32)
            {
                if ctrl_id == ID_EDIT_CANCEL_HK {
                    update_captured_hotkey(hwnd, ctrl_id, None);
                }
                return 0;
            }

            let mut mods = 0u32;
            if ctrl_down {
                mods |= MOD_CONTROL;
            }
            if alt_down {
                mods |= MOD_ALT;
            }
            if shift_down {
                mods |= MOD_SHIFT;
            }
            if win_down {
                mods |= MOD_WIN;
            }
            update_captured_hotkey(hwnd, ctrl_id, Some(HotkeySpec { modifiers: mods, vk }));
            return 0;
        }
        0x0102 | 0x0106 => return 0, // WM_CHAR / WM_SYSCHAR
        _ => {}
    }

    if let Some(proc_fn) = orig_proc {
        CallWindowProcW(Some(proc_fn), hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn update_captured_hotkey(edit_hwnd: HWND, ctrl_id: i32, spec: Option<HotkeySpec>) {
    let parent = windows_sys::Win32::UI::WindowsAndMessaging::GetParent(edit_hwnd);
    let mut ui_text: Option<String> = None;
    let mut status_msg = String::new();

    SETTINGS_STATE.with(|cell| {
        if let Some(st) = cell.borrow_mut().as_mut() {
            if ctrl_id == ID_EDIT_TRIGGER_HK {
                if let Some(sp) = spec {
                    st.draft.trigger_hotkey = sp;
                    ui_text = Some(format_hotkey(sp));
                    status_msg = check_single_hotkey_status(st, sp, true);
                }
            } else if ctrl_id == ID_EDIT_CANCEL_HK {
                st.draft.cancel_hotkey = spec;
                ui_text = Some(
                    spec.map(format_hotkey)
                        .unwrap_or_else(|| "（未设置/已禁用）".to_string()),
                );
                status_msg = match spec {
                    Some(sp) => check_single_hotkey_status(st, sp, false),
                    None => "取消快捷键已禁用".to_string(),
                };
            }
        }
    });

    if let Some(t) = ui_text {
        SUPPRESS_EVENTS.set(true);
        set_ctrl_text(parent, ctrl_id, &t);
        SUPPRESS_EVENTS.set(false);
    }
    if !status_msg.is_empty() {
        set_ctrl_text(parent, ID_LBL_HK_STATUS, &status_msg);
    }
}

fn check_single_hotkey_status(st: &SettingsWindowState, spec: HotkeySpec, is_trigger: bool) -> String {
    if is_trigger && st.draft.cancel_hotkey == Some(spec) {
        return "冲突：触发快捷键与取消快捷键不能相同！".to_string();
    }
    if !is_trigger && st.draft.trigger_hotkey == spec {
        return "冲突：取消快捷键与触发快捷键不能相同！".to_string();
    }
    match probe_conflict(st.main_hwnd, spec, st.active_trigger, st.active_cancel) {
        Ok(()) => format!("可用：{} 当前无系统冲突", format_hotkey(spec)),
        Err(err) => format!("冲突提示：{err}"),
    }
}

struct InitSnapshot {
    trigger_text: String,
    cancel_text: String,
    input_mode_idx: usize,
    hybrid_ctrl_c: bool,
    enable_fallback: bool,
    pure_text_qa_mode: bool,
    skip_permissions: bool,
    sound_enabled: bool,
    sound_volume: String,
    heartbeat_secs: String,
    timeout_secs: String,
    strip_markdown: bool,
    autostart: bool,
    preset_idx: usize,
    target_lang_idx: usize,
    effective_instruction: String,
    diag_text: String,
    selected_wav_idx: usize,
}

unsafe fn populate_all_fields(hwnd: HWND) {
    let snap = SETTINGS_STATE.with(|cell| {
        let borrow = cell.borrow();
        let st = borrow.as_ref()?;
        let diag_text = if st.diagnostics.summary.is_empty() {
            format!(
                "暂无运行记录。日志路径: {}",
                logger::log_path().display()
            )
        } else {
            format!(
                "时间: {} | 状态: {}\r\n{}",
                st.diagnostics.timestamp, st.diagnostics.state_label, st.diagnostics.summary
            )
        };
        let input_mode_idx = InputSourceMode::all()
            .iter()
            .position(|&m| m == st.draft.input_mode)
            .unwrap_or(0);
        let preset_idx = TaskPreset::all()
            .iter()
            .position(|&p| p == st.draft.task_preset)
            .unwrap_or(0);
        let target_lang_idx = TranslateTargetLang::all()
            .iter()
            .position(|&l| l == st.draft.translate_target_lang)
            .unwrap_or(0);
        Some(InitSnapshot {
            trigger_text: format_hotkey(st.draft.trigger_hotkey),
            cancel_text: st
                .draft
                .cancel_hotkey
                .map(format_hotkey)
                .unwrap_or_else(|| "（未设置/已禁用）".to_string()),
            input_mode_idx,
            hybrid_ctrl_c: st.draft.hybrid_ctrl_c_fallback,
            enable_fallback: st.draft.enable_fallback,
            pure_text_qa_mode: st.draft.pure_text_qa_mode,
            skip_permissions: st.draft.skip_permissions,
            sound_enabled: st.draft.sound_enabled,
            sound_volume: st.draft.sound_volume.to_string(),
            heartbeat_secs: st.draft.heartbeat_interval_secs.to_string(),
            timeout_secs: st.draft.timeout_secs.to_string(),
            strip_markdown: st.draft.strip_markdown,
            autostart: st.draft.autostart,
            preset_idx,
            target_lang_idx,
            effective_instruction: st.draft.effective_system_instruction(),
            diag_text,
            selected_wav_idx: st.selected_wav_idx,
        })
    });

    let Some(s) = snap else {
        return;
    };

    SUPPRESS_EVENTS.set(true);
    set_ctrl_text(hwnd, ID_EDIT_TRIGGER_HK, &s.trigger_text);
    set_ctrl_text(hwnd, ID_EDIT_CANCEL_HK, &s.cancel_text);
    SendMessageW(get_ctrl(hwnd, ID_CMB_INPUT_MODE), CB_SETCURSEL, s.input_mode_idx, 0);
    set_check(hwnd, ID_CHK_HYBRID_CTRL_C, s.hybrid_ctrl_c);
    EnableWindow(
        get_ctrl(hwnd, ID_CHK_HYBRID_CTRL_C),
        (s.input_mode_idx == 0) as i32,
    );
    set_check(hwnd, ID_CHK_FALLBACK, s.enable_fallback);
    set_check(hwnd, ID_CHK_PURE_QA, s.pure_text_qa_mode);
    set_check(hwnd, ID_CHK_SKIP_PERM, s.skip_permissions);
    EnableWindow(
        get_ctrl(hwnd, ID_CHK_SKIP_PERM),
        (!s.pure_text_qa_mode) as i32,
    );
    set_check(hwnd, ID_CHK_SOUND, s.sound_enabled);
    set_ctrl_text(hwnd, ID_EDIT_VOLUME, &s.sound_volume);
    set_ctrl_text(hwnd, ID_EDIT_HEARTBEAT, &s.heartbeat_secs);
    set_ctrl_text(hwnd, ID_EDIT_TIMEOUT, &s.timeout_secs);
    set_check(hwnd, ID_CHK_STRIP_MD, s.strip_markdown);
    set_check(hwnd, ID_CHK_AUTOSTART, s.autostart);
    SendMessageW(get_ctrl(hwnd, ID_CMB_TASK_PRESET), CB_SETCURSEL, s.preset_idx, 0);
    SendMessageW(get_ctrl(hwnd, ID_CMB_TARGET_LANG), CB_SETCURSEL, s.target_lang_idx, 0);
    EnableWindow(
        get_ctrl(hwnd, ID_CMB_TARGET_LANG),
        (s.preset_idx == 1) as i32,
    );
    set_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX, &s.effective_instruction);
    set_ctrl_text(hwnd, ID_EDIT_DIAGNOSTICS, &s.diag_text);
    SendMessageW(get_ctrl(hwnd, ID_CMB_WAV_CUE), CB_SETCURSEL, s.selected_wav_idx, 0);
    SUPPRESS_EVENTS.set(false);

    refresh_wav_field(hwnd);
    refresh_priority_list(hwnd);
    refresh_backend_detail(hwnd);
}

unsafe fn refresh_priority_list(hwnd: HWND) {
    let data = SETTINGS_STATE.with(|cell| {
        let borrow = cell.borrow();
        let st = borrow.as_ref()?;
        let lines: Vec<String> = st
            .draft
            .backend_priority
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let mark = if entry.enabled { "✓" } else { " " };
                let detected = st
                    .detections
                    .iter()
                    .find(|d| d.kind == entry.kind)
                    .map(|d| if d.available { "可用" } else { "未检测到" })
                    .unwrap_or("未知");
                format!("{}. [{}] {}  —  [{}]", idx + 1, mark, entry.kind.label(), detected)
            })
            .collect();
        let sel = st
            .selected_priority_idx
            .min(st.draft.backend_priority.len().saturating_sub(1));
        Some((lines, sel))
    });

    let Some((lines, sel)) = data else {
        return;
    };

    let lb = get_ctrl(hwnd, ID_LIST_PRIORITY);
    SendMessageW(lb, LB_RESETCONTENT, 0, 0);
    for line in lines {
        let wl = to_wide(&line);
        SendMessageW(lb, LB_ADDSTRING, 0, wl.as_ptr() as LPARAM);
    }
    SendMessageW(lb, LB_SETCURSEL, sel, 0);
}

unsafe fn refresh_backend_detail(hwnd: HWND) {
    let detail = SETTINGS_STATE.with(|cell| {
        let borrow = cell.borrow();
        let st = borrow.as_ref()?;
        let entry = st.draft.backend_priority.get(st.selected_priority_idx)?;
        let kind = entry.kind;
        let is_custom = kind == CliKind::Custom;
        if is_custom {
            Some((
                is_custom,
                st.draft.custom_cli.command.clone(),
                String::new(),
                String::new(),
                st.draft.custom_cli.args_template.clone(),
                st.draft.custom_cli.use_stdin,
            ))
        } else {
            let prof = st.draft.profile(kind);
            Some((
                is_custom,
                prof.exe_path,
                prof.model,
                prof.effort,
                prof.extra_args,
                true,
            ))
        }
    });

    let Some((is_custom, path, model, effort, extra, use_stdin)) = detail else {
        return;
    };

    SUPPRESS_EVENTS.set(true);
    set_ctrl_text(hwnd, ID_EDIT_CLI_PATH, &path);
    set_ctrl_text(hwnd, ID_EDIT_MODEL, &model);
    set_ctrl_text(hwnd, ID_EDIT_EFFORT, &effort);
    set_ctrl_text(hwnd, ID_EDIT_EXTRA_ARGS, &extra);
    set_check(hwnd, ID_CHK_CUSTOM_STDIN, use_stdin);
    EnableWindow(get_ctrl(hwnd, ID_EDIT_MODEL), (!is_custom) as i32);
    EnableWindow(get_ctrl(hwnd, ID_EDIT_EFFORT), (!is_custom) as i32);
    EnableWindow(get_ctrl(hwnd, ID_CHK_CUSTOM_STDIN), is_custom as i32);
    SUPPRESS_EVENTS.set(false);
}

unsafe fn refresh_wav_field(hwnd: HWND) {
    let val = SETTINGS_STATE.with(|cell| {
        let borrow = cell.borrow();
        let st = borrow.as_ref()?;
        Some(
            match st.selected_wav_idx {
                0 => &st.draft.custom_wavs.start,
                1 => &st.draft.custom_wavs.processing,
                2 => &st.draft.custom_wavs.complete,
                3 => &st.draft.custom_wavs.error,
                _ => &st.draft.custom_wavs.busy,
            }
            .clone(),
        )
    });

    if let Some(v) = val {
        SUPPRESS_EVENTS.set(true);
        set_ctrl_text(hwnd, ID_EDIT_WAV_PATH, &v);
        SUPPRESS_EVENTS.set(false);
    }
}

unsafe fn handle_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_BTN_CHECK_TRIGGER => {
            let msg = SETTINGS_STATE.with(|cell| {
                let borrow = cell.borrow();
                let Some(st) = borrow.as_ref() else {
                    return String::new();
                };
                check_single_hotkey_status(st, st.draft.trigger_hotkey, true)
            });
            set_ctrl_text(hwnd, ID_LBL_HK_STATUS, &msg);
        }
        ID_BTN_CLEAR_CANCEL => {
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    st.draft.cancel_hotkey = None;
                }
            });
            set_ctrl_text(hwnd, ID_EDIT_CANCEL_HK, "（未设置/已禁用）");
            set_ctrl_text(hwnd, ID_LBL_HK_STATUS, "取消快捷键已禁用");
        }
        ID_CMB_INPUT_MODE if code == CBN_SELCHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let sel = SendMessageW(get_ctrl(hwnd, ID_CMB_INPUT_MODE), CB_GETCURSEL, 0, 0);
            if sel >= 0 {
                let modes = InputSourceMode::all();
                if let Some(&mode) = modes.get(sel as usize) {
                    SETTINGS_STATE.with(|cell| {
                        if let Some(st) = cell.borrow_mut().as_mut() {
                            st.draft.input_mode = mode;
                        }
                    });
                    EnableWindow(
                        get_ctrl(hwnd, ID_CHK_HYBRID_CTRL_C),
                        (mode == InputSourceMode::SmartHybrid) as i32,
                    );
                }
            }
        }
        ID_CHK_PURE_QA => {
            if !SUPPRESS_EVENTS.get() {
                let pure_qa = get_check(hwnd, ID_CHK_PURE_QA);
                EnableWindow(get_ctrl(hwnd, ID_CHK_SKIP_PERM), (!pure_qa) as i32);
            }
        }
        ID_LIST_PRIORITY if code == LBN_SELCHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let sel = SendMessageW(get_ctrl(hwnd, ID_LIST_PRIORITY), LB_GETCURSEL, 0, 0);
            if sel >= 0 {
                SETTINGS_STATE.with(|cell| {
                    if let Some(st) = cell.borrow_mut().as_mut() {
                        st.selected_priority_idx = sel as usize;
                    }
                });
                refresh_backend_detail(hwnd);
            }
        }
        ID_BTN_TOGGLE_BACKEND => {
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    if let Some(entry) = st.draft.backend_priority.get_mut(st.selected_priority_idx) {
                        entry.enabled = !entry.enabled;
                    }
                }
            });
            refresh_priority_list(hwnd);
        }
        ID_BTN_MOVE_UP => {
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    if st.selected_priority_idx > 0
                        && st.selected_priority_idx < st.draft.backend_priority.len()
                    {
                        st.draft
                            .backend_priority
                            .swap(st.selected_priority_idx, st.selected_priority_idx - 1);
                        st.selected_priority_idx -= 1;
                    }
                }
            });
            refresh_priority_list(hwnd);
        }
        ID_BTN_MOVE_DOWN => {
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    if st.selected_priority_idx + 1 < st.draft.backend_priority.len() {
                        st.draft
                            .backend_priority
                            .swap(st.selected_priority_idx, st.selected_priority_idx + 1);
                        st.selected_priority_idx += 1;
                    }
                }
            });
            refresh_priority_list(hwnd);
        }
        ID_BTN_REFRESH_DETECT => {
            let new_det = SETTINGS_STATE.with(|cell| {
                cell.borrow()
                    .as_ref()
                    .map(|st| detect_all_backends(&st.draft))
            });
            if let Some(det) = new_det {
                SETTINGS_STATE.with(|cell| {
                    if let Some(st) = cell.borrow_mut().as_mut() {
                        st.detections = det;
                    }
                });
                refresh_priority_list(hwnd);
            }
        }
        ID_EDIT_CLI_PATH | ID_EDIT_MODEL | ID_EDIT_EFFORT | ID_EDIT_EXTRA_ARGS
            if code == EN_CHANGE =>
        {
            if !SUPPRESS_EVENTS.get() {
                sync_backend_detail_from_ui(hwnd);
            }
        }
        ID_CHK_CUSTOM_STDIN => {
            if !SUPPRESS_EVENTS.get() {
                sync_backend_detail_from_ui(hwnd);
            }
        }
        ID_BTN_BROWSE_CLI => {
            if let Some(path) = browse_file(hwnd, "可执行文件 (*.exe;*.cmd)\0*.exe;*.cmd\0所有文件 (*.*)\0*.*\0\0") {
                set_ctrl_text(hwnd, ID_EDIT_CLI_PATH, &path);
                sync_backend_detail_from_ui(hwnd);
            }
        }
        ID_CMB_WAV_CUE if code == CBN_SELCHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let sel = SendMessageW(get_ctrl(hwnd, ID_CMB_WAV_CUE), CB_GETCURSEL, 0, 0);
            if sel >= 0 {
                SETTINGS_STATE.with(|cell| {
                    if let Some(st) = cell.borrow_mut().as_mut() {
                        st.selected_wav_idx = sel as usize;
                    }
                });
                refresh_wav_field(hwnd);
            }
        }
        ID_EDIT_WAV_PATH if code == EN_CHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let val = get_ctrl_text(hwnd, ID_EDIT_WAV_PATH);
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    match st.selected_wav_idx {
                        0 => st.draft.custom_wavs.start = val,
                        1 => st.draft.custom_wavs.processing = val,
                        2 => st.draft.custom_wavs.complete = val,
                        3 => st.draft.custom_wavs.error = val,
                        _ => st.draft.custom_wavs.busy = val,
                    }
                }
            });
        }
        ID_BTN_BROWSE_WAV => {
            if let Some(path) = browse_file(hwnd, "WAV 音频 (*.wav)\0*.wav\0所有文件 (*.*)\0*.*\0\0") {
                set_ctrl_text(hwnd, ID_EDIT_WAV_PATH, &path);
            }
        }
        ID_BTN_TEST_SOUND => {
            let vol = get_ctrl_text(hwnd, ID_EDIT_VOLUME).trim().parse::<u8>().unwrap_or(75).min(100);
            SETTINGS_STATE.with(|cell| {
                if let Some(st) = cell.borrow_mut().as_mut() {
                    let preview = AudioPlayer::new(true, vol, &st.draft.custom_wavs);
                    let cue = match st.selected_wav_idx {
                        0 => SoundCue::Start,
                        1 => SoundCue::Processing,
                        2 => SoundCue::Complete,
                        3 => SoundCue::Error,
                        _ => SoundCue::Busy,
                    };
                    preview.play_preview(cue);
                    st.preview_audio = Some(preview);
                }
            });
        }
        ID_CMB_TASK_PRESET if code == CBN_SELCHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let sel = SendMessageW(get_ctrl(hwnd, ID_CMB_TASK_PRESET), CB_GETCURSEL, 0, 0);
            if sel >= 0 {
                if let Some(&preset) = TaskPreset::all().get(sel as usize) {
                    let next_prompt = SETTINGS_STATE.with(|cell| {
                        let mut borrow = cell.borrow_mut();
                        let st = borrow.as_mut()?;
                        st.draft.task_preset = preset;
                        Some(st.draft.effective_system_instruction())
                    });
                    EnableWindow(
                        get_ctrl(hwnd, ID_CMB_TARGET_LANG),
                        (preset == TaskPreset::Translate) as i32,
                    );
                    if let Some(p) = next_prompt {
                        SUPPRESS_EVENTS.set(true);
                        set_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX, &p);
                        SUPPRESS_EVENTS.set(false);
                    }
                }
            }
        }
        ID_CMB_TARGET_LANG if code == CBN_SELCHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let sel = SendMessageW(get_ctrl(hwnd, ID_CMB_TARGET_LANG), CB_GETCURSEL, 0, 0);
            if sel >= 0 {
                if let Some(&lang) = TranslateTargetLang::all().get(sel as usize) {
                    let next_prompt = SETTINGS_STATE.with(|cell| {
                        let mut borrow = cell.borrow_mut();
                        let st = borrow.as_mut()?;
                        st.draft.translate_target_lang = lang;
                        Some(st.draft.effective_system_instruction())
                    });
                    if let Some(p) = next_prompt {
                        SUPPRESS_EVENTS.set(true);
                        set_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX, &p);
                        SUPPRESS_EVENTS.set(false);
                    }
                }
            }
        }
        ID_EDIT_PROMPT_PREFIX if code == EN_CHANGE => {
            if SUPPRESS_EVENTS.get() {
                return;
            }
            let edited = get_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX);
            let switched_to_custom = SETTINGS_STATE.with(|cell| {
                let mut borrow = cell.borrow_mut();
                let Some(st) = borrow.as_mut() else {
                    return false;
                };
                st.draft.system_instruction = edited;
                if st.draft.task_preset != TaskPreset::Custom {
                    st.draft.task_preset = TaskPreset::Custom;
                    return true;
                }
                false
            });
            if switched_to_custom {
                let custom_idx = TaskPreset::all()
                    .iter()
                    .position(|&p| p == TaskPreset::Custom)
                    .unwrap_or(5);
                SUPPRESS_EVENTS.set(true);
                SendMessageW(get_ctrl(hwnd, ID_CMB_TASK_PRESET), CB_SETCURSEL, custom_idx, 0);
                EnableWindow(get_ctrl(hwnd, ID_CMB_TARGET_LANG), 0);
                SUPPRESS_EVENTS.set(false);
            }
        }
        ID_BTN_RESET_PROMPT => {
            let (reset_text, preset_idx, is_translate) = SETTINGS_STATE.with(|cell| {
                let mut borrow = cell.borrow_mut();
                let Some(st) = borrow.as_mut() else {
                    return (DEFAULT_SYSTEM_INSTRUCTION.to_string(), 0usize, false);
                };
                if st.draft.task_preset == TaskPreset::Custom {
                    st.draft.task_preset = TaskPreset::Qa;
                    st.draft.system_instruction = DEFAULT_SYSTEM_INSTRUCTION.to_string();
                }
                let idx = TaskPreset::all()
                    .iter()
                    .position(|&p| p == st.draft.task_preset)
                    .unwrap_or(0);
                (
                    st.draft.effective_system_instruction(),
                    idx,
                    st.draft.task_preset == TaskPreset::Translate,
                )
            });
            SUPPRESS_EVENTS.set(true);
            SendMessageW(get_ctrl(hwnd, ID_CMB_TASK_PRESET), CB_SETCURSEL, preset_idx, 0);
            EnableWindow(get_ctrl(hwnd, ID_CMB_TARGET_LANG), is_translate as i32);
            set_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX, &reset_text);
            SUPPRESS_EVENTS.set(false);
        }
        ID_BTN_OPEN_LOG => {
            let path = logger::log_path();
            if !path.exists() {
                logger::log_line("INFO", "Log opened from settings");
            }
            let _ = Command::new("notepad.exe")
                .arg(path)
                .creation_flags(0x0800_0000)
                .spawn();
        }
        ID_BTN_SAVE => save_and_close(hwnd),
        ID_BTN_CANCEL => {
            DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe fn sync_backend_detail_from_ui(hwnd: HWND) {
    let path_val = get_ctrl_text(hwnd, ID_EDIT_CLI_PATH);
    let model_val = get_ctrl_text(hwnd, ID_EDIT_MODEL);
    let effort_val = get_ctrl_text(hwnd, ID_EDIT_EFFORT);
    let extra_val = get_ctrl_text(hwnd, ID_EDIT_EXTRA_ARGS);
    let use_stdin = get_check(hwnd, ID_CHK_CUSTOM_STDIN);

    SETTINGS_STATE.with(|cell| {
        if let Some(st) = cell.borrow_mut().as_mut() {
            let Some(entry) = st.draft.backend_priority.get(st.selected_priority_idx) else {
                return;
            };
            let kind = entry.kind;
            if kind == CliKind::Custom {
                st.draft.custom_cli.command = path_val;
                st.draft.custom_cli.args_template = extra_val;
                st.draft.custom_cli.use_stdin = use_stdin;
            } else {
                let mut prof = st.draft.profile(kind);
                prof.exe_path = path_val;
                prof.model = model_val;
                prof.effort = effort_val;
                prof.extra_args = extra_val;
                st.draft.set_profile(kind, prof);
            }
        }
    });
}

unsafe fn save_and_close(hwnd: HWND) {
    sync_backend_detail_from_ui(hwnd);

    let hybrid_ctrl_c_fallback = get_check(hwnd, ID_CHK_HYBRID_CTRL_C);
    let mode_sel = SendMessageW(get_ctrl(hwnd, ID_CMB_INPUT_MODE), CB_GETCURSEL, 0, 0);
    let enable_fallback = get_check(hwnd, ID_CHK_FALLBACK);
    let pure_text_qa_mode = get_check(hwnd, ID_CHK_PURE_QA);
    let skip_permissions = get_check(hwnd, ID_CHK_SKIP_PERM);
    let sound_enabled = get_check(hwnd, ID_CHK_SOUND);
    let vol_text = get_ctrl_text(hwnd, ID_EDIT_VOLUME);
    let hb_text = get_ctrl_text(hwnd, ID_EDIT_HEARTBEAT);
    let timeout_text = get_ctrl_text(hwnd, ID_EDIT_TIMEOUT);
    let strip_markdown = get_check(hwnd, ID_CHK_STRIP_MD);
    let autostart = get_check(hwnd, ID_CHK_AUTOSTART);
    let preset_sel = SendMessageW(get_ctrl(hwnd, ID_CMB_TASK_PRESET), CB_GETCURSEL, 0, 0);
    let lang_sel = SendMessageW(get_ctrl(hwnd, ID_CMB_TARGET_LANG), CB_GETCURSEL, 0, 0);
    let system_instruction = get_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX);

    let result: Result<(AppConfig, HWND), String> = (|| {
        let vol = vol_text
            .trim()
            .parse::<u8>()
            .map_err(|_| "音量请输入 0 到 100 之间的整数".to_string())?
            .min(100);
        let hb = hb_text
            .trim()
            .parse::<u32>()
            .map_err(|_| "处理中提示音间隔请输入非负整数（0 表示关闭）".to_string())?;
        let timeout = timeout_text
            .trim()
            .parse::<u32>()
            .map_err(|_| "超时时间请输入非负整数（0 表示不限）".to_string())?;

        let (mut cfg, main_hwnd, active_trig, active_cancel) = SETTINGS_STATE.with(|cell| {
            let borrow = cell.borrow();
            let st = borrow.as_ref().ok_or_else(|| "内部状态异常".to_string())?;
            Ok::<_, String>((
                st.draft.clone(),
                st.main_hwnd,
                st.active_trigger,
                st.active_cancel,
            ))
        })?;

        if let Some(&mode) = InputSourceMode::all().get(mode_sel.max(0) as usize) {
            cfg.input_mode = mode;
        }
        cfg.hybrid_ctrl_c_fallback = hybrid_ctrl_c_fallback;
        cfg.enable_fallback = enable_fallback;
        cfg.pure_text_qa_mode = pure_text_qa_mode;
        cfg.skip_permissions = skip_permissions;
        cfg.sound_enabled = sound_enabled;
        cfg.sound_volume = vol;
        cfg.heartbeat_interval_secs = hb;
        cfg.timeout_secs = timeout;
        cfg.strip_markdown = strip_markdown;
        cfg.autostart = autostart;
        if let Some(&preset) = TaskPreset::all().get(preset_sel.max(0) as usize) {
            cfg.task_preset = preset;
        }
        if let Some(&lang) = TranslateTargetLang::all().get(lang_sel.max(0) as usize) {
            cfg.translate_target_lang = lang;
        }
        if cfg.task_preset == TaskPreset::Custom {
            cfg.system_instruction = system_instruction;
        }

        if !cfg.backend_priority.iter().any(|e| e.enabled) {
            return Err("请至少启用一个 CLI 后端".to_string());
        }
        if cfg.cancel_hotkey == Some(cfg.trigger_hotkey) {
            return Err("触发快捷键与取消快捷键不能相同，请修改其中一个".to_string());
        }

        probe_conflict(main_hwnd, cfg.trigger_hotkey, active_trig, active_cancel)?;
        if let Some(cancel_hk) = cfg.cancel_hotkey {
            probe_conflict(main_hwnd, cancel_hk, active_trig, active_cancel)?;
        }

        apply_autostart(cfg.autostart)?;
        cfg.save()?;
        Ok((cfg, main_hwnd))
    })();

    match result {
        Ok((cfg, main_hwnd)) => {
            logger::log_line("INFO", "Settings saved");
            let boxed = Box::into_raw(Box::new(cfg));
            PostMessageW(main_hwnd, WM_APP_CONFIG_SAVED, 0, boxed as LPARAM);
            DestroyWindow(hwnd);
        }
        Err(err) => {
            logger::log_line("WARN", &format!("Settings save rejected: {err}"));
            show_warning(hwnd, "无法保存设置", &err);
            let _ = SetFocus(get_ctrl(hwnd, ID_EDIT_TRIGGER_HK));
        }
    }
}

pub unsafe fn show_warning(owner: HWND, title: &str, msg: &str) {
    let wt = to_wide(title);
    let wm = to_wide(msg);
    MessageBoxW(owner, wm.as_ptr(), wt.as_ptr(), MB_OK | MB_ICONWARNING);
}

unsafe fn browse_file(owner: HWND, filter_double_nul: &str) -> Option<String> {
    let mut file_buf = vec![0u16; 1024];
    let filter_w: Vec<u16> = filter_double_nul.encode_utf16().collect();
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter_w.as_ptr();
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if GetOpenFileNameW(&mut ofn) != 0 {
        Some(from_wide_slice(&file_buf))
    } else {
        None
    }
}

unsafe fn get_ctrl(parent: HWND, id: i32) -> HWND {
    windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(parent, id)
}

unsafe fn set_ctrl_text(parent: HWND, id: i32, text: &str) {
    let w = to_wide(text);
    SetWindowTextW(get_ctrl(parent, id), w.as_ptr());
}

unsafe fn get_ctrl_text(parent: HWND, id: i32) -> String {
    let ctrl = get_ctrl(parent, id);
    let len = GetWindowTextLengthW(ctrl);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    GetWindowTextW(ctrl, buf.as_mut_ptr(), buf.len() as i32);
    from_wide_slice(&buf)
}

unsafe fn set_check(parent: HWND, id: i32, checked: bool) {
    let state = if checked { BST_CHECKED } else { BST_UNCHECKED };
    SendMessageW(get_ctrl(parent, id), BM_SETCHECK, state, 0);
}

unsafe fn get_check(parent: HWND, id: i32) -> bool {
    SendMessageW(get_ctrl(parent, id), BM_GETCHECK, 0, 0) as usize == BST_CHECKED
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    #[test]
    fn settings_window_opens_switches_tabs_and_closes_without_panic() {
        let cfg = AppConfig::default();
        let audio = AudioPlayer::new(false, 50, &cfg.custom_wavs);
        let report = LastRunReport {
            timestamp: "16:10:00".to_string(),
            state_label: "成功".to_string(),
            summary: "agy 成功 (3.1s)".to_string(),
        };

        open_or_focus(
            std::ptr::null_mut(),
            &cfg,
            Some(cfg.trigger_hotkey),
            cfg.cancel_hotkey,
            audio,
            report,
        );

        let hwnd = SETTINGS_HWND.with(|cell| *cell.borrow());
        assert!(!hwnd.is_null(), "settings window failed to create");

        unsafe {
            // Simulate selecting another backend in priority list and another wav cue.
            let lb = get_ctrl(hwnd, ID_LIST_PRIORITY);
            SendMessageW(lb, LB_SETCURSEL, 1, 0);
            handle_command(hwnd, ID_LIST_PRIORITY, LBN_SELCHANGE);
            handle_command(hwnd, ID_BTN_TOGGLE_BACKEND, 0);
            handle_command(hwnd, ID_BTN_MOVE_UP, 0);

            let cmb = get_ctrl(hwnd, ID_CMB_WAV_CUE);
            SendMessageW(cmb, CB_SETCURSEL, 2, 0);
            handle_command(hwnd, ID_CMB_WAV_CUE, CBN_SELCHANGE);

            let cmb_mode = get_ctrl(hwnd, ID_CMB_INPUT_MODE);
            SendMessageW(cmb_mode, CB_SETCURSEL, 2, 0);
            handle_command(hwnd, ID_CMB_INPUT_MODE, CBN_SELCHANGE);
            SendMessageW(cmb_mode, CB_SETCURSEL, 0, 0);
            handle_command(hwnd, ID_CMB_INPUT_MODE, CBN_SELCHANGE);

            let cmb_preset = get_ctrl(hwnd, ID_CMB_TASK_PRESET);
            SendMessageW(cmb_preset, CB_SETCURSEL, 1, 0);
            handle_command(hwnd, ID_CMB_TASK_PRESET, CBN_SELCHANGE);
            let cmb_lang = get_ctrl(hwnd, ID_CMB_TARGET_LANG);
            let en_idx = TranslateTargetLang::all()
                .iter()
                .position(|&l| l == TranslateTargetLang::En)
                .unwrap();
            SendMessageW(cmb_lang, CB_SETCURSEL, en_idx, 0);
            handle_command(hwnd, ID_CMB_TARGET_LANG, CBN_SELCHANGE);
            assert!(get_ctrl_text(hwnd, ID_EDIT_PROMPT_PREFIX).contains("英文"));

            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            DestroyWindow(hwnd);
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let after = SETTINGS_HWND.with(|cell| *cell.borrow());
        assert!(after.is_null(), "settings HWND should clear on destroy");
    }
}
