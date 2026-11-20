//! Persistent configuration, CLI priority ordering, and Windows autostart registry sync.

use crate::win_util::to_wide;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

pub const DEFAULT_SYSTEM_INSTRUCTION: &str = "直接用简洁纯文本一次性回答问题，不用Markdown，不反问。";

const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const VK_Q: u32 = 0x51;
const VK_X: u32 = 0x58;

const RUN_REG_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE_NAME: &str = "ClipAI";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliKind {
    Agy,
    Codex,
    Claude,
    Custom,
}

impl CliKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agy => "agy CLI (Antigravity)",
            Self::Codex => "codex CLI (OpenAI)",
            Self::Claude => "claude CLI (Anthropic)",
            Self::Custom => "自定义 CLI (Custom)",
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Agy, Self::Codex, Self::Claude, Self::Custom]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSourceMode {
    SmartHybrid,
    ClipboardOnly,
    SelectionOnly,
}

impl Default for InputSourceMode {
    fn default() -> Self {
        Self::SmartHybrid
    }
}

impl InputSourceMode {
    pub fn all() -> [Self; 3] {
        [Self::SmartHybrid, Self::ClipboardOnly, Self::SelectionOnly]
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Self::SmartHybrid => "智能混合",
            Self::ClipboardOnly => "仅剪贴板",
            Self::SelectionOnly => "仅划词",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SmartHybrid => "智能混合（优先划词，无划词回退剪贴板）",
            Self::ClipboardOnly => "仅剪贴板",
            Self::SelectionOnly => "仅划词高亮（仅 UIA，不触发 Ctrl+C）",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPreset {
    Qa,
    Translate,
    Summarize,
    Polish,
    Explain,
    Custom,
}

impl Default for TaskPreset {
    fn default() -> Self {
        Self::Qa
    }
}

impl TaskPreset {
    pub fn all() -> [Self; 6] {
        [
            Self::Qa,
            Self::Translate,
            Self::Summarize,
            Self::Polish,
            Self::Explain,
            Self::Custom,
        ]
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Self::Qa => "问答",
            Self::Translate => "翻译",
            Self::Summarize => "总结",
            Self::Polish => "润色",
            Self::Explain => "解释",
            Self::Custom => "自定义",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Qa => "纯文本问答（默认）",
            Self::Translate => "智能翻译（自动识别源语言）",
            Self::Summarize => "提炼总结",
            Self::Polish => "润色改写",
            Self::Explain => "概念/代码解释",
            Self::Custom => "自定义提示词",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateTargetLang {
    AutoZhEn,
    ZhCn,
    En,
    Ja,
    Ko,
    ZhTw,
    Fr,
    De,
}

impl Default for TranslateTargetLang {
    fn default() -> Self {
        Self::AutoZhEn
    }
}

impl TranslateTargetLang {
    pub fn all() -> [Self; 8] {
        [
            Self::AutoZhEn,
            Self::ZhCn,
            Self::En,
            Self::Ja,
            Self::Ko,
            Self::ZhTw,
            Self::Fr,
            Self::De,
        ]
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Self::AutoZhEn => "中英互译",
            Self::ZhCn => "简体中文",
            Self::En => "英文",
            Self::Ja => "日文",
            Self::Ko => "韩文",
            Self::ZhTw => "繁体中文",
            Self::Fr => "法语",
            Self::De => "德语",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AutoZhEn => "中英互译（中文→英文，外文→简体中文）",
            Self::ZhCn => "简体中文",
            Self::En => "English（英文）",
            Self::Ja => "日本語（日文）",
            Self::Ko => "한국어（韩文）",
            Self::ZhTw => "繁體中文",
            Self::Fr => "Français（法语）",
            Self::De => "Deutsch（德语）",
        }
    }

    fn prompt_target(self) -> &'static str {
        match self {
            Self::AutoZhEn => "",
            Self::ZhCn => "简体中文",
            Self::En => "自然地道的英文",
            Self::Ja => "自然地道的日文",
            Self::Ko => "自然地道的韩文",
            Self::ZhTw => "繁體中文",
            Self::Fr => "自然地道的法语",
            Self::De => "自然地道的德语",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendEntry {
    pub kind: CliKind,
    pub enabled: bool,
}

impl Default for BackendEntry {
    fn default() -> Self {
        Self {
            kind: CliKind::Agy,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySpec {
    /// Win32 modifier flags (`MOD_ALT=1`, `MOD_CONTROL=2`, `MOD_SHIFT=4`, `MOD_WIN=8`).
    pub modifiers: u32,
    /// Win32 virtual-key code (`VK_*`).
    pub vk: u32,
}

impl Default for HotkeySpec {
    fn default() -> Self {
        Self {
            modifiers: MOD_CONTROL | MOD_ALT,
            vk: VK_Q,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomSoundPaths {
    pub start: String,
    pub processing: String,
    pub complete: String,
    pub error: String,
    pub busy: String,
}

impl Default for CustomSoundPaths {
    fn default() -> Self {
        Self {
            start: String::new(),
            processing: String::new(),
            complete: String::new(),
            error: String::new(),
            busy: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CliProfile {
    pub exe_path: String,
    pub model: String,
    pub effort: String,
    pub extra_args: String,
}

impl Default for CliProfile {
    fn default() -> Self {
        Self {
            exe_path: String::new(),
            model: String::new(),
            effort: String::new(),
            extra_args: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCliConfig {
    pub command: String,
    /// Argument template; `{prompt}` and `{instruction}` are substituted if `use_stdin` is false.
    pub args_template: String,
    pub use_stdin: bool,
}

impl Default for CustomCliConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args_template: String::new(),
            use_stdin: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub input_mode: InputSourceMode,
    pub hybrid_ctrl_c_fallback: bool,
    pub task_preset: TaskPreset,
    pub translate_target_lang: TranslateTargetLang,
    pub backend_priority: Vec<BackendEntry>,
    pub enable_fallback: bool,
    pub pure_text_qa_mode: bool,
    pub skip_permissions: bool,
    pub trigger_hotkey: HotkeySpec,
    pub cancel_hotkey: Option<HotkeySpec>,
    pub sound_enabled: bool,
    pub sound_volume: u8,
    /// 0 disables periodic processing sound; >0 plays every N seconds while running.
    pub heartbeat_interval_secs: u32,
    pub custom_wavs: CustomSoundPaths,
    /// Timeout per CLI attempt in seconds; 0 disables timeout.
    pub timeout_secs: u32,
    pub strip_markdown: bool,
    pub system_instruction: String,
    pub cli_profiles: BTreeMap<String, CliProfile>,
    pub custom_cli: CustomCliConfig,
    pub autostart: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            CliKind::Agy.display_name().to_string(),
            CliProfile {
                effort: "low".to_string(),
                ..CliProfile::default()
            },
        );
        profiles.insert(
            CliKind::Codex.display_name().to_string(),
            CliProfile::default(),
        );
        profiles.insert(
            CliKind::Claude.display_name().to_string(),
            CliProfile::default(),
        );

        Self {
            input_mode: InputSourceMode::SmartHybrid,
            hybrid_ctrl_c_fallback: true,
            task_preset: TaskPreset::Qa,
            translate_target_lang: TranslateTargetLang::AutoZhEn,
            backend_priority: vec![
                BackendEntry {
                    kind: CliKind::Agy,
                    enabled: true,
                },
                BackendEntry {
                    kind: CliKind::Codex,
                    enabled: true,
                },
                BackendEntry {
                    kind: CliKind::Claude,
                    enabled: true,
                },
                BackendEntry {
                    kind: CliKind::Custom,
                    enabled: false,
                },
            ],
            enable_fallback: true,
            pure_text_qa_mode: true,
            skip_permissions: true,
            trigger_hotkey: HotkeySpec {
                modifiers: MOD_CONTROL | MOD_ALT,
                vk: VK_Q,
            },
            cancel_hotkey: Some(HotkeySpec {
                modifiers: MOD_CONTROL | MOD_ALT,
                vk: VK_X,
            }),
            sound_enabled: true,
            sound_volume: 75,
            heartbeat_interval_secs: 0,
            custom_wavs: CustomSoundPaths::default(),
            timeout_secs: 120,
            strip_markdown: true,
            system_instruction: DEFAULT_SYSTEM_INSTRUCTION.to_string(),
            cli_profiles: profiles,
            custom_cli: CustomCliConfig::default(),
            autostart: false,
        }
    }
}

impl AppConfig {
    /// Ensure every known `CliKind` appears once in `backend_priority` and profile entries exist.
    pub fn normalize(&mut self) {
        let mut seen = Vec::new();
        let mut cleaned = Vec::new();
        for entry in &self.backend_priority {
            if !seen.contains(&entry.kind) {
                seen.push(entry.kind);
                cleaned.push(entry.clone());
            }
        }
        for kind in CliKind::all() {
            if !seen.contains(&kind) {
                cleaned.push(BackendEntry {
                    kind,
                    enabled: kind != CliKind::Custom,
                });
            }
        }
        self.backend_priority = cleaned;
        self.sound_volume = self.sound_volume.min(100);
        if self.system_instruction.trim().is_empty() {
            self.system_instruction = DEFAULT_SYSTEM_INSTRUCTION.to_string();
        }
        for kind in [CliKind::Agy, CliKind::Codex, CliKind::Claude] {
            self.cli_profiles
                .entry(kind.display_name().to_string())
                .or_default();
        }
    }

    pub fn profile(&self, kind: CliKind) -> CliProfile {
        self.cli_profiles
            .get(kind.display_name())
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_profile(&mut self, kind: CliKind, profile: CliProfile) {
        self.cli_profiles
            .insert(kind.display_name().to_string(), profile);
    }

    /// Promote a specific CLI backend to the top of `backend_priority` and ensure it is enabled.
    pub fn promote_backend(&mut self, kind: CliKind) {
        self.backend_priority.retain(|e| e.kind != kind);
        self.backend_priority
            .insert(0, BackendEntry { kind, enabled: true });
        self.normalize();
    }

    pub fn primary_backend(&self) -> CliKind {
        self.backend_priority
            .iter()
            .find(|e| e.enabled)
            .map(|e| e.kind)
            .unwrap_or(CliKind::Agy)
    }

    /// Generate the concise instruction prompt for a built-in preset and translation target language.
    pub fn preset_instruction(preset: TaskPreset, lang: TranslateTargetLang) -> Option<String> {
        match preset {
            TaskPreset::Qa => Some(DEFAULT_SYSTEM_INSTRUCTION.to_string()),
            TaskPreset::Translate => Some(match lang {
                TranslateTargetLang::AutoZhEn => {
                    "自动识别语言：中文译为自然英文，非中文译为简体中文。仅输出纯文本译文，无解释、无Markdown。".to_string()
                }
                other => {
                    let t = other.prompt_target();
                    format!("自动识别源语言并翻译为{t}；若原文已是{t}则直接原样输出。仅输出纯文本结果，无解释、无Markdown。")
                }
            }),
            TaskPreset::Summarize => {
                Some("将内容提炼为简洁的纯文本要点，不添废话，不用Markdown。".to_string())
            }
            TaskPreset::Polish => {
                Some("保持原意和原语言，润色语句使其更自然流畅。仅输出修改后纯文本，无解释、无Markdown。".to_string())
            }
            TaskPreset::Explain => {
                Some("用简明通俗的纯文本解释概念、报错或代码含义，不用Markdown。".to_string())
            }
            TaskPreset::Custom => None,
        }
    }

    /// Resolve the active instruction text based on `task_preset` and `translate_target_lang`.
    pub fn effective_system_instruction(&self) -> String {
        Self::preset_instruction(self.task_preset, self.translate_target_lang)
            .unwrap_or_else(|| self.system_instruction.trim().to_string())
    }

    /// Short status badge for the tray menu header (e.g. `问答` or `翻译→英文`).
    pub fn mode_badge(&self) -> String {
        if self.task_preset == TaskPreset::Translate {
            format!("翻译→{}", self.translate_target_lang.short_name())
        } else {
            self.task_preset.short_name().to_string()
        }
    }

    pub fn config_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                if dir.join("portable.flag").exists() {
                    return dir.join("config.json");
                }
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("clip-ai").join("config.json");
        }
        PathBuf::from("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        let mut cfg = if let Ok(raw) = fs::read_to_string(&path) {
            serde_json::from_str::<Self>(&raw).unwrap_or_default()
        } else {
            Self::default()
        };
        cfg.normalize();
        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        let mut normalized = self.clone();
        normalized.normalize();
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("无法创建配置目录 {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&normalized)
            .map_err(|e| format!("序列化配置失败: {e}"))?;
        fs::write(&path, json).map_err(|e| format!("写入配置文件失败 {}: {e}", path.display()))
    }
}

/// Synchronize the HKCU Run registry entry for autostart at Windows sign-in.
pub fn apply_autostart(enabled: bool) -> Result<(), String> {
    let subkey = to_wide(RUN_REG_SUBKEY);
    let val_name = to_wide(RUN_VALUE_NAME);
    unsafe {
        let mut hkey = std::ptr::null_mut();
        let rc = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut hkey);
        if rc != ERROR_SUCCESS {
            return Err(format!("打开自启动注册表项失败 (错误码 {rc})"));
        }
        let result = if enabled {
            let exe = std::env::current_exe()
                .map_err(|e| format!("获取程序路径失败: {e}"))?;
            let cmd = format!("\"{}\" --autostart", exe.display());
            let wide_cmd = to_wide(&cmd);
            let byte_len = (wide_cmd.len() * 2) as u32;
            let set_rc = RegSetValueExW(
                hkey,
                val_name.as_ptr(),
                0,
                REG_SZ,
                wide_cmd.as_ptr() as *const u8,
                byte_len,
            );
            if set_rc != ERROR_SUCCESS {
                Err(format!("写入自启动注册表失败 (错误码 {set_rc})"))
            } else {
                Ok(())
            }
        } else {
            let del_rc = RegDeleteValueW(hkey, val_name.as_ptr());
            if del_rc != ERROR_SUCCESS && del_rc != 2 {
                // 2 = ERROR_FILE_NOT_FOUND (already absent)
                Err(format!("移除自启动注册表失败 (错误码 {del_rc})"))
            } else {
                Ok(())
            }
        };
        RegCloseKey(hkey);
        result
    }
}

/// Query whether the HKCU Run entry is currently registered.
pub fn is_autostart_registered() -> bool {
    let subkey = to_wide(RUN_REG_SUBKEY);
    let val_name = to_wide(RUN_VALUE_NAME);
    unsafe {
        let mut hkey = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let rc = RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        RegCloseKey(hkey);
        rc == ERROR_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_prioritizes_agy_and_skips_permissions() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.input_mode, InputSourceMode::SmartHybrid);
        assert!(cfg.hybrid_ctrl_c_fallback);
        assert!(cfg.pure_text_qa_mode);
        assert_eq!(cfg.primary_backend(), CliKind::Agy);
        assert_eq!(cfg.backend_priority[0].kind, CliKind::Agy);
        assert_eq!(cfg.backend_priority[1].kind, CliKind::Codex);
        assert_eq!(cfg.backend_priority[2].kind, CliKind::Claude);
        assert!(cfg.enable_fallback);
        assert!(cfg.skip_permissions);
        assert!(cfg.sound_enabled);
        assert_eq!(cfg.heartbeat_interval_secs, 0);
        assert!(cfg.strip_markdown);
    }

    #[test]
    fn normalize_restores_missing_backends_and_clamps_volume() {
        let mut cfg = AppConfig {
            backend_priority: vec![BackendEntry {
                kind: CliKind::Claude,
                enabled: true,
            }],
            sound_volume: 150,
            system_instruction: "   ".to_string(),
            ..AppConfig::default()
        };
        cfg.normalize();
        assert_eq!(cfg.backend_priority.len(), 4);
        assert_eq!(cfg.backend_priority[0].kind, CliKind::Claude);
        assert_eq!(cfg.sound_volume, 100);
        assert!(!cfg.system_instruction.trim().is_empty());
    }

    #[test]
    fn promote_backend_moves_target_to_front() {
        let mut cfg = AppConfig::default();
        cfg.promote_backend(CliKind::Codex);
        assert_eq!(cfg.primary_backend(), CliKind::Codex);
        assert_eq!(cfg.backend_priority[1].kind, CliKind::Agy);
    }

    #[test]
    fn legacy_json_without_input_mode_defaults_to_smart_hybrid() {
        let legacy = r#"{"enable_fallback":false,"sound_volume":50}"#;
        let mut cfg: AppConfig = serde_json::from_str(legacy).unwrap();
        cfg.normalize();
        assert_eq!(cfg.input_mode, InputSourceMode::SmartHybrid);
        assert!(cfg.hybrid_ctrl_c_fallback);
        assert!(cfg.pure_text_qa_mode);
        assert_eq!(cfg.task_preset, TaskPreset::Qa);
        assert_eq!(cfg.translate_target_lang, TranslateTargetLang::AutoZhEn);
        assert!(!cfg.enable_fallback);
        assert_eq!(cfg.sound_volume, 50);
    }

    #[test]
    fn translate_preset_updates_prompt_and_handles_same_language_input() {
        let mut cfg = AppConfig::default();
        cfg.task_preset = TaskPreset::Translate;
        cfg.translate_target_lang = TranslateTargetLang::AutoZhEn;
        assert!(cfg.effective_system_instruction().contains("中文译为自然英文"));
        assert_eq!(cfg.mode_badge(), "翻译→中英互译");

        cfg.translate_target_lang = TranslateTargetLang::Ja;
        let ja_prompt = cfg.effective_system_instruction();
        assert!(ja_prompt.contains("日文"));
        assert!(ja_prompt.contains("原样输出"));
        assert_eq!(cfg.mode_badge(), "翻译→日文");
    }
}
