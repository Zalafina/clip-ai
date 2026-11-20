//! Headless CLI execution engine with Windows Job Object process-tree isolation,
//! executable auto-discovery, and customizable priority fallback.

use crate::config::{AppConfig, CliKind};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const AGY_INLINE_MAX_UTF16: usize = 28_000;

/// Safe wrapper around a Win32 Job Object handle.
struct JobHandle(HANDLE);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Handle shared with the UI thread to immediately abort the active CLI process tree & fallback loop.
#[derive(Clone)]
pub struct CancelHandle {
    cancelled: Arc<AtomicBool>,
    active_job: Arc<Mutex<Option<Arc<JobHandle>>>>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            active_job: Arc::new(Mutex::new(None)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.active_job.lock() {
            if let Some(job) = guard.as_ref() {
                unsafe {
                    TerminateJobObject(job.0, 1);
                }
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn set_active_job(&self, job: Option<Arc<JobHandle>>) {
        if let Ok(mut guard) = self.active_job.lock() {
            *guard = job;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDetection {
    pub kind: CliKind,
    pub available: bool,
    pub resolved_display: String,
}

#[derive(Debug, Clone)]
pub enum ExecutionOutcome {
    Success {
        backend: CliKind,
        answer: String,
        attempt_notes: Vec<String>,
        duration_ms: u64,
    },
    Cancelled {
        attempt_notes: Vec<String>,
        duration_ms: u64,
    },
    Failed {
        reason: String,
        attempt_notes: Vec<String>,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedBinary {
    DirectExe(PathBuf),
    NodeScript { node_exe: PathBuf, script: PathBuf },
    CmdBatch(PathBuf),
}

impl ResolvedBinary {
    fn display_path(&self) -> String {
        match self {
            Self::DirectExe(p) | Self::CmdBatch(p) => p.display().to_string(),
            Self::NodeScript { script, .. } => script.display().to_string(),
        }
    }
}

/// Inspect all backends configured in `cfg` and report which ones are currently runnable.
pub fn detect_all_backends(cfg: &AppConfig) -> Vec<BackendDetection> {
    CliKind::all()
        .into_iter()
        .map(|kind| match resolve_backend_binary(kind, cfg) {
            Ok(bin) => BackendDetection {
                kind,
                available: true,
                resolved_display: bin.display_path(),
            },
            Err(err) => BackendDetection {
                kind,
                available: false,
                resolved_display: err,
            },
        })
        .collect()
}

/// Execute the prompt across enabled backends in priority order, notifying `on_started` once
/// the first runnable candidate begins execution.
pub fn run_with_fallback<F>(
    cfg: &AppConfig,
    prompt: &str,
    cancel: &CancelHandle,
    mut on_started: F,
) -> ExecutionOutcome
where
    F: FnMut(CliKind),
{
    let total_start = Instant::now();
    let enabled: Vec<CliKind> = cfg
        .backend_priority
        .iter()
        .filter(|e| e.enabled)
        .map(|e| e.kind)
        .collect();

    if enabled.is_empty() {
        return ExecutionOutcome::Failed {
            reason: "未启用任何 CLI 后端，请在设置中至少勾选一个".to_string(),
            attempt_notes: vec![],
            duration_ms: total_start.elapsed().as_millis() as u64,
        };
    }

    let candidates = if cfg.enable_fallback {
        enabled
    } else {
        vec![enabled[0]]
    };

    let sandbox_dir = match ensure_sandbox_dir() {
        Ok(d) => d,
        Err(e) => {
            return ExecutionOutcome::Failed {
                reason: e,
                attempt_notes: vec![],
                duration_ms: total_start.elapsed().as_millis() as u64,
            };
        }
    };

    let mut attempt_notes: Vec<String> = Vec::new();
    let mut started_notified = false;

    for kind in candidates {
        if cancel.is_cancelled() {
            return ExecutionOutcome::Cancelled {
                attempt_notes,
                duration_ms: total_start.elapsed().as_millis() as u64,
            };
        }

        let binary = match resolve_backend_binary(kind, cfg) {
            Ok(b) => b,
            Err(err) => {
                attempt_notes.push(format!("{}: 跳过 ({err})", kind.display_name()));
                continue;
            }
        };

        if !started_notified {
            started_notified = true;
            on_started(kind);
        }

        let attempt_start = Instant::now();
        match execute_single(kind, &binary, cfg, prompt, &sandbox_dir, cancel) {
            Ok(answer) => {
                let sec = attempt_start.elapsed().as_secs_f32();
                attempt_notes.push(format!("{} 成功 ({sec:.1}s)", kind.display_name()));
                return ExecutionOutcome::Success {
                    backend: kind,
                    answer,
                    attempt_notes,
                    duration_ms: total_start.elapsed().as_millis() as u64,
                };
            }
            Err(AttemptError::Cancelled) => {
                attempt_notes.push(format!("{} 已由用户取消", kind.display_name()));
                return ExecutionOutcome::Cancelled {
                    attempt_notes,
                    duration_ms: total_start.elapsed().as_millis() as u64,
                };
            }
            Err(AttemptError::Failed(msg)) => {
                let sec = attempt_start.elapsed().as_secs_f32();
                attempt_notes.push(format!("{} 失败 ({sec:.1}s): {msg}", kind.display_name()));
            }
        }
    }

    let reason = if attempt_notes.is_empty() {
        "未找到可用的 CLI 工具，请确认 agy / codex / claude 已安装并在 PATH 中".to_string()
    } else {
        attempt_notes.join(" | ")
    };
    ExecutionOutcome::Failed {
        reason,
        attempt_notes,
        duration_ms: total_start.elapsed().as_millis() as u64,
    }
}

#[derive(Debug)]
enum AttemptError {
    Cancelled,
    Failed(String),
}

struct PreparedInvocation {
    args: Vec<String>,
    env_vars: Vec<(&'static str, &'static str)>,
    stdin_payload: Option<String>,
    output_file: Option<PathBuf>,
}

fn execute_single(
    kind: CliKind,
    binary: &ResolvedBinary,
    cfg: &AppConfig,
    prompt: &str,
    sandbox_dir: &Path,
    cancel: &CancelHandle,
) -> Result<String, AttemptError> {
    let prep = build_invocation(kind, cfg, prompt, sandbox_dir).map_err(AttemptError::Failed)?;

    let mut cmd = match binary {
        ResolvedBinary::DirectExe(exe) => {
            let mut c = Command::new(exe);
            c.args(&prep.args);
            c
        }
        ResolvedBinary::NodeScript { node_exe, script } => {
            let mut c = Command::new(node_exe);
            c.arg(script);
            c.args(&prep.args);
            c
        }
        ResolvedBinary::CmdBatch(bat) => {
            let mut c = Command::new("cmd.exe");
            c.arg("/d").arg("/c").arg(bat);
            c.args(&prep.args);
            c
        }
    };

    cmd.current_dir(sandbox_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if prep.stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for &(k, v) in &prep.env_vars {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AttemptError::Failed(format!("启动进程失败: {e}")))?;

    let job = attach_to_job(&child).ok().map(Arc::new);
    cancel.set_active_job(job.clone());

    let result = run_child_to_completion(
        kind,
        &mut child,
        prep.stdin_payload,
        prep.output_file.as_deref(),
        cfg.timeout_secs,
        cancel,
        job.as_deref(),
    );

    cancel.set_active_job(None);
    if let Some(ref out_file) = prep.output_file {
        let _ = fs::remove_file(out_file);
    }
    result
}

fn run_child_to_completion(
    kind: CliKind,
    child: &mut Child,
    stdin_payload: Option<String>,
    output_file: Option<&Path>,
    timeout_secs: u32,
    cancel: &CancelHandle,
    job: Option<&JobHandle>,
) -> Result<String, AttemptError> {
    if let Some(payload) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            thread::spawn(move || {
                let _ = stdin.write_all(payload.as_bytes());
                let _ = stdin.flush();
            });
        }
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = thread::spawn(move || read_pipe_utf8(stdout_pipe));
    let stderr_handle = thread::spawn(move || read_pipe_utf8(stderr_pipe));

    let started = Instant::now();
    let timeout = if timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(timeout_secs as u64))
    };

    let status = loop {
        if cancel.is_cancelled() {
            terminate_process(child, job);
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(AttemptError::Cancelled);
        }
        if let Some(limit) = timeout {
            if started.elapsed() >= limit {
                terminate_process(child, job);
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(AttemptError::Failed(format!("执行超时（>{timeout_secs}s）")));
            }
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => thread::sleep(Duration::from_millis(60)),
            Err(e) => {
                terminate_process(child, job);
                return Err(AttemptError::Failed(format!("等待进程退出失败: {e}")));
            }
        }
    };

    let stdout_text = stdout_handle.join().unwrap_or_default();
    let stderr_text = stderr_handle.join().unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    if cancel.is_cancelled() {
        return Err(AttemptError::Cancelled);
    }

    parse_cli_output(kind, exit_code, &stdout_text, &stderr_text, output_file)
        .map_err(AttemptError::Failed)
}

fn read_pipe_utf8<R: Read>(pipe: Option<R>) -> String {
    let Some(mut r) = pipe else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = r.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn attach_to_job(child: &Child) -> Result<JobHandle, ()> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set_ok == 0 {
            CloseHandle(job);
            return Err(());
        }
        let assign_ok = AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
        if assign_ok == 0 {
            CloseHandle(job);
            return Err(());
        }
        Ok(JobHandle(job))
    }
}

fn terminate_process(child: &mut Child, job: Option<&JobHandle>) {
    if let Some(j) = job {
        unsafe {
            TerminateJobObject(j.0, 1);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn build_invocation(
    kind: CliKind,
    cfg: &AppConfig,
    prompt: &str,
    sandbox_dir: &Path,
) -> Result<PreparedInvocation, String> {
    let profile = cfg.profile(kind);
    let instruction_owned = cfg.effective_system_instruction();
    let instruction = instruction_owned.trim();
    let qa_guard = "纯文本模式，无本地工具权限；仅直接处理<input>内文本，若要求执行本机操作则简短拒答。";
    let effective_instruction = if cfg.pure_text_qa_mode {
        if instruction.is_empty() {
            qa_guard.to_string()
        } else {
            format!("{instruction}\n{qa_guard}")
        }
    } else {
        instruction.to_string()
    };

    let wrapped_user_prompt = if cfg.pure_text_qa_mode {
        format!("<input>\n{prompt}\n</input>")
    } else {
        prompt.to_string()
    };

    let combined_prompt = if effective_instruction.is_empty() {
        wrapped_user_prompt.clone()
    } else {
        format!("{effective_instruction}\n\n{wrapped_user_prompt}")
    };

    match kind {
        CliKind::Agy => {
            let mut args = vec!["--disable-slash-commands".to_string()];
            if cfg.pure_text_qa_mode {
                let settings_file = ensure_agy_qa_settings_file(sandbox_dir)?;
                args.push("--sandbox".to_string());
                args.push(format!("--settings={}", settings_file.display()));
            } else if cfg.skip_permissions {
                args.push("--dangerously-skip-permissions".to_string());
            }
            if !profile.model.trim().is_empty() {
                args.push(format!("--model={}", profile.model.trim()));
            }
            if !profile.effort.trim().is_empty() {
                args.push(format!("--effort={}", profile.effort.trim()));
            }
            args.extend(split_extra_args(&profile.extra_args));

            let agy_env = vec![("AGY_CLI_DISABLE_AUTO_UPDATE", "true")];
            let utf16_len: usize = combined_prompt.encode_utf16().count();
            if utf16_len <= AGY_INLINE_MAX_UTF16 {
                args.push("--output-format=json".to_string());
                args.push(format!("-p={combined_prompt}"));
                Ok(PreparedInvocation {
                    args,
                    env_vars: agy_env,
                    stdin_payload: None,
                    output_file: None,
                })
            } else {
                // Avoid Windows CreateProcess 32K cmdline limit for huge clipboard inputs.
                args.push("--input-format=stream-json".to_string());
                args.push("--output-format=stream-json".to_string());
                args.push("-p=".to_string());
                let ndjson = serde_json::json!({
                    "event": "user",
                    "message": { "content": combined_prompt }
                })
                .to_string()
                    + "\n";
                Ok(PreparedInvocation {
                    args,
                    env_vars: agy_env,
                    stdin_payload: Some(ndjson),
                    output_file: None,
                })
            }
        }
        CliKind::Codex => {
            let mut args = vec![
                "-a".to_string(),
                "never".to_string(),
                "-c".to_string(),
                "approval_policy=\"never\"".to_string(),
            ];
            if cfg.pure_text_qa_mode {
                args.extend([
                    "--disable".to_string(),
                    "shell_tool".to_string(),
                    "--disable".to_string(),
                    "unified_exec".to_string(),
                    "--disable".to_string(),
                    "apps".to_string(),
                    "-c".to_string(),
                    "mcp_servers={}".to_string(),
                ]);
            }
            if !profile.model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(profile.model.trim().to_string());
            }
            if !profile.effort.trim().is_empty() {
                args.push("-c".to_string());
                args.push(format!(
                    "model_reasoning_effort=\"{}\"",
                    profile.effort.trim()
                ));
            }
            args.push("exec".to_string());
            args.push("--skip-git-repo-check".to_string());
            args.push("--ephemeral".to_string());
            if !cfg.pure_text_qa_mode && cfg.skip_permissions {
                args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
            } else {
                args.push("-s".to_string());
                args.push("read-only".to_string());
            }
            args.push("--color".to_string());
            args.push("never".to_string());

            let out_file = sandbox_dir.join(format!(
                "codex_last_{}.txt",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            args.push("-o".to_string());
            args.push(out_file.display().to_string());
            args.extend(split_extra_args(&profile.extra_args));
            args.push("-".to_string());

            Ok(PreparedInvocation {
                args,
                env_vars: Vec::new(),
                stdin_payload: Some(combined_prompt),
                output_file: Some(out_file),
            })
        }
        CliKind::Claude => {
            let mut args = vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                "--no-session-persistence".to_string(),
                "--disable-slash-commands".to_string(),
            ];
            if cfg.pure_text_qa_mode {
                args.extend([
                    "--tools".to_string(),
                    "".to_string(),
                    "--strict-mcp-config".to_string(),
                    "--permission-mode".to_string(),
                    "dontAsk".to_string(),
                ]);
            } else {
                args.push("--permission-mode".to_string());
                if cfg.skip_permissions {
                    args.push("bypassPermissions".to_string());
                    args.push("--dangerously-skip-permissions".to_string());
                } else {
                    args.push("dontAsk".to_string());
                }
            }
            if !effective_instruction.is_empty() {
                args.push("--append-system-prompt".to_string());
                args.push(effective_instruction);
            }
            if !profile.model.trim().is_empty() {
                args.push("--model".to_string());
                args.push(profile.model.trim().to_string());
            }
            if !profile.effort.trim().is_empty() {
                args.push("--effort".to_string());
                args.push(profile.effort.trim().to_string());
            }
            args.extend(split_extra_args(&profile.extra_args));

            Ok(PreparedInvocation {
                args,
                env_vars: Vec::new(),
                stdin_payload: Some(wrapped_user_prompt),
                output_file: None,
            })
        }
        CliKind::Custom => {
            let tmpl = cfg.custom_cli.args_template.trim();
            let mut args = Vec::new();
            let stdin_payload = if cfg.custom_cli.use_stdin {
                if !tmpl.is_empty() {
                    args.extend(split_extra_args(tmpl));
                }
                Some(combined_prompt)
            } else {
                for tok in split_extra_args(tmpl) {
                    args.push(
                        tok.replace("{prompt}", &wrapped_user_prompt)
                            .replace("{instruction}", &effective_instruction),
                    );
                }
                if tmpl.is_empty() {
                    args.push(combined_prompt);
                }
                None
            };
            Ok(PreparedInvocation {
                args,
                env_vars: Vec::new(),
                stdin_payload,
                output_file: None,
            })
        }
    }
}

fn ensure_agy_qa_settings_file(sandbox_dir: &Path) -> Result<PathBuf, String> {
    let path = sandbox_dir.join("agy_qa_settings.json");
    let content = r#"{"permissions":{"deny":["command(*)","write_file(*)","read_file(*)","mcp(*)"]}}"#;
    fs::write(&path, content)
        .map_err(|e| format!("无法写入 agy 安全配置 {}: {e}", path.display()))?;
    Ok(path)
}

fn parse_cli_output(
    kind: CliKind,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    output_file: Option<&Path>,
) -> Result<String, String> {
    let err_hint = summarize_error(stderr, stdout);

    match kind {
        CliKind::Agy => {
            // Try parsing single JSON object first, then NDJSON lines (for stream-json).
            if let Some(val) = parse_last_json_object(stdout) {
                let status = val.get("status").and_then(Value::as_str).unwrap_or("");
                let resp = val
                    .get("response")
                    .or_else(|| val.get("result"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let denied = val
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);

                if exit_code == 0 && (status.eq_ignore_ascii_case("SUCCESS") || status.is_empty()) && !resp.is_empty() {
                    return Ok(resp.to_string());
                }
                if denied && resp.is_empty() {
                    return Err(format!(
                        "工具权限被拒绝且无正文输出{}",
                        if err_hint.is_empty() { String::new() } else { format!(" ({err_hint})") }
                    ));
                }
                if let Some(err_msg) = val.get("error").and_then(Value::as_str) {
                    return Err(truncate_chars(err_msg, 180));
                }
            }
            if exit_code != 0 {
                return Err(if err_hint.is_empty() {
                    format!("退出码 {exit_code}")
                } else {
                    format!("退出码 {exit_code}: {err_hint}")
                });
            }
            if !stdout.trim().is_empty() && !stdout.trim_start().starts_with('{') {
                return Ok(stdout.trim().to_string());
            }
            Err(if err_hint.is_empty() {
                "未返回有效回答（可能未登录或触发了空响应）".to_string()
            } else {
                err_hint
            })
        }
        CliKind::Codex => {
            if exit_code != 0 {
                return Err(if err_hint.is_empty() {
                    format!("退出码 {exit_code}")
                } else {
                    format!("退出码 {exit_code}: {err_hint}")
                });
            }
            if let Some(path) = output_file {
                if let Ok(content) = fs::read_to_string(path) {
                    if !content.trim().is_empty() {
                        return Ok(content.trim().to_string());
                    }
                }
            }
            if !stdout.trim().is_empty() {
                return Ok(stdout.trim().to_string());
            }
            Err(if err_hint.is_empty() {
                "Codex 未生成答复内容".to_string()
            } else {
                err_hint
            })
        }
        CliKind::Claude => {
            if let Some(val) = parse_last_json_object(stdout) {
                let is_err = val.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let result = val
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if exit_code == 0 && !is_err && !result.is_empty() {
                    return Ok(result.to_string());
                }
                if !result.is_empty() && is_err {
                    return Err(truncate_chars(result, 180));
                }
            }
            if exit_code != 0 {
                return Err(if err_hint.is_empty() {
                    format!("退出码 {exit_code}")
                } else {
                    format!("退出码 {exit_code}: {err_hint}")
                });
            }
            if !stdout.trim().is_empty() && !stdout.trim_start().starts_with('{') {
                return Ok(stdout.trim().to_string());
            }
            Err(if err_hint.is_empty() {
                "Claude 未返回有效内容".to_string()
            } else {
                err_hint
            })
        }
        CliKind::Custom => {
            if exit_code != 0 {
                return Err(if err_hint.is_empty() {
                    format!("退出码 {exit_code}")
                } else {
                    format!("退出码 {exit_code}: {err_hint}")
                });
            }
            if !stdout.trim().is_empty() {
                Ok(stdout.trim().to_string())
            } else {
                Err(if err_hint.is_empty() {
                    "自定义 CLI 标准输出为空".to_string()
                } else {
                    err_hint
                })
            }
        }
    }
}

fn parse_last_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    for line in trimmed.lines().rev() {
        let l = line.trim();
        if l.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                if v.is_object() {
                    // For stream-json, prefer the final result object if present.
                    if v.get("response").is_some() || v.get("result").is_some() || v.get("status").is_some() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn summarize_error(stderr: &str, stdout: &str) -> String {
    let clean_err = strip_ansi(stderr);
    let picked: Vec<&str> = clean_err
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("warning:"))
        .take(3)
        .collect();
    if !picked.is_empty() {
        return truncate_chars(&picked.join(" / "), 180);
    }
    let clean_out = strip_ansi(stdout);
    let out_line = clean_out
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('{'))
        .unwrap_or("");
    truncate_chars(out_line, 180)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for nc in chars.by_ref() {
                    if ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let collected: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{collected}...")
    } else {
        collected
    }
}

fn resolve_backend_binary(kind: CliKind, cfg: &AppConfig) -> Result<ResolvedBinary, String> {
    if kind == CliKind::Custom {
        let cmd_str = cfg.custom_cli.command.trim();
        if cmd_str.is_empty() {
            return Err("未配置自定义 CLI 命令".to_string());
        }
        return resolve_path_or_on_path(cmd_str, CliKind::Custom);
    }

    let profile = cfg.profile(kind);
    let custom_path = profile.exe_path.trim();
    if !custom_path.is_empty() {
        return resolve_path_or_on_path(custom_path, kind);
    }

    let names: &[&str] = match kind {
        CliKind::Agy => &["agy.exe", "agy"],
        CliKind::Codex => &["codex.exe", "codex.cmd", "codex"],
        CliKind::Claude => &["claude.exe", "claude.cmd", "claude"],
        CliKind::Custom => unreachable!(),
    };

    for name in names {
        if let Ok(resolved) = resolve_path_or_on_path(name, kind) {
            return Ok(resolved);
        }
    }

    // Extra well-known Windows install locations if PATH wasn't refreshed.
    if kind == CliKind::Agy {
        if let Ok(local) = env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
            if p.is_file() {
                return Ok(ResolvedBinary::DirectExe(p));
            }
        }
    }
    if let Ok(appdata) = env::var("APPDATA") {
        let npm_dir = PathBuf::from(appdata).join("npm");
        if kind == CliKind::Claude {
            let native = npm_dir.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
            if native.is_file() {
                return Ok(ResolvedBinary::DirectExe(native));
            }
        }
        if kind == CliKind::Codex {
            let js = npm_dir.join("node_modules/@openai/codex/bin/codex.js");
            if js.is_file() {
                if let Some(node) = find_on_path("node.exe") {
                    return Ok(ResolvedBinary::NodeScript {
                        node_exe: node,
                        script: js,
                    });
                }
            }
        }
    }

    Err(format!("未在 PATH 中找到 {}", kind.display_name()))
}

fn resolve_path_or_on_path(spec: &str, kind: CliKind) -> Result<ResolvedBinary, String> {
    let p = PathBuf::from(spec);
    let found = if p.is_absolute() || spec.contains('\\') || spec.contains('/') {
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    } else {
        find_on_path(spec)
    };

    let Some(path) = found else {
        return Err(format!("找不到程序: {spec}"));
    };

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "cmd" || ext == "bat" {
        if let Some(unwrapped) = try_unwrap_npm_shim(&path, kind) {
            return Ok(unwrapped);
        }
        return Ok(ResolvedBinary::CmdBatch(path));
    }

    Ok(ResolvedBinary::DirectExe(path))
}

/// Unwrap npm `.cmd` shims into direct `claude.exe` or `node.exe + codex.js` to bypass cmd.exe.
fn try_unwrap_npm_shim(cmd_path: &Path, kind: CliKind) -> Option<ResolvedBinary> {
    let dir = cmd_path.parent()?;
    match kind {
        CliKind::Claude => {
            let native = dir.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
            if native.is_file() {
                return Some(ResolvedBinary::DirectExe(native));
            }
        }
        CliKind::Codex => {
            let script = dir.join("node_modules/@openai/codex/bin/codex.js");
            if script.is_file() {
                let local_node = dir.join("node.exe");
                let node_exe = if local_node.is_file() {
                    Some(local_node)
                } else {
                    find_on_path("node.exe")
                }?;
                return Some(ResolvedBinary::NodeScript { node_exe, script });
            }
        }
        _ => {}
    }
    None
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    let has_ext = Path::new(name).extension().is_some();
    let exts: &[&str] = if has_ext {
        &[""]
    } else {
        &[".exe", ".cmd", ".bat"]
    };

    for dir in env::split_paths(&path_var) {
        for ext in exts {
            let candidate = if ext.is_empty() {
                dir.join(name)
            } else {
                dir.join(format!("{name}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn ensure_sandbox_dir() -> Result<PathBuf, String> {
    let dir = env::temp_dir().join("clip-ai-sandbox");
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建沙盒目录 {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Split shell-style extra arguments supporting single and double quotes.
pub fn split_extra_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                has_token = true;
            }
            Some(_) => {
                current.push(ch);
            }
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has_token = true;
            }
            None if ch.is_whitespace() => {
                if has_token || !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(ch);
                has_token = true;
            }
        }
    }
    if has_token || !current.is_empty() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_output_rejects_empty_response_even_on_exit_zero() {
        let json = r#"{"status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}"#;
        let res = parse_cli_output(CliKind::Agy, 0, json, "", None);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("拒绝"));
    }

    #[test]
    fn agy_output_extracts_valid_response_and_stream_json() {
        let json = r#"{"status":"SUCCESS","response":"HTTP 429 表示请求过于频繁。\n"}"#;
        let res = parse_cli_output(CliKind::Agy, 0, json, "", None).unwrap();
        assert_eq!(res, "HTTP 429 表示请求过于频繁。");

        let ndjson = "{\"event\":\"thinking\"}\n{\"status\":\"SUCCESS\",\"response\":\"OK\"}\n";
        let res2 = parse_cli_output(CliKind::Agy, 0, ndjson, "", None).unwrap();
        assert_eq!(res2, "OK");
    }

    #[test]
    fn claude_output_handles_success_and_error() {
        let ok = r#"{"is_error":false,"result":"答案是 42"}"#;
        assert_eq!(
            parse_cli_output(CliKind::Claude, 0, ok, "", None).unwrap(),
            "答案是 42"
        );
        let err = r#"{"is_error":true,"result":"Not logged in"}"#;
        assert!(parse_cli_output(CliKind::Claude, 0, err, "", None).is_err());
    }

    #[test]
    fn pure_qa_mode_locks_tools_and_unrestricted_mode_respects_skip_permissions() {
        let sandbox = env::temp_dir().join("clip-ai-test-sandbox");
        let _ = fs::create_dir_all(&sandbox);

        // Default: pure_text_qa_mode = true
        let cfg = AppConfig::default();
        let agy_prep = build_invocation(CliKind::Agy, &cfg, "1+1=?", &sandbox).unwrap();
        assert!(!agy_prep.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(agy_prep.args.contains(&"--sandbox".to_string()));
        assert!(agy_prep.args.iter().any(|a| a.starts_with("--settings=")));
        assert!(agy_prep.args.iter().any(|a| a.contains("<input>")));
        assert!(agy_prep
            .env_vars
            .contains(&("AGY_CLI_DISABLE_AUTO_UPDATE", "true")));
        let qa_json = fs::read_to_string(sandbox.join("agy_qa_settings.json")).unwrap();
        assert!(!qa_json.contains("edit_file(*)"));

        let codex_prep = build_invocation(CliKind::Codex, &cfg, "1+1=?", &sandbox).unwrap();
        assert!(!codex_prep.args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(codex_prep.args.contains(&"read-only".to_string()));
        assert!(codex_prep.args.contains(&"shell_tool".to_string()));

        let claude_prep = build_invocation(CliKind::Claude, &cfg, "1+1=?", &sandbox).unwrap();
        assert!(!claude_prep.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(claude_prep.args.contains(&"--strict-mcp-config".to_string()));
        assert!(claude_prep.args.contains(&"--tools".to_string()));
        assert!(claude_prep.stdin_payload.as_deref().unwrap().contains("<input>"));

        // Unrestricted mode: pure_text_qa_mode = false, skip_permissions = true
        let mut free_cfg = AppConfig::default();
        free_cfg.pure_text_qa_mode = false;
        let agy_free = build_invocation(CliKind::Agy, &free_cfg, "1+1=?", &sandbox).unwrap();
        assert!(agy_free.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!agy_free.args.contains(&"--sandbox".to_string()));
        assert!(agy_free
            .env_vars
            .contains(&("AGY_CLI_DISABLE_AUTO_UPDATE", "true")));

        let codex_free = build_invocation(CliKind::Codex, &free_cfg, "1+1=?", &sandbox).unwrap();
        assert!(codex_free.args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));

        let claude_free = build_invocation(CliKind::Claude, &free_cfg, "1+1=?", &sandbox).unwrap();
        assert!(claude_free.args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!claude_free.args.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn split_extra_args_respects_quotes() {
        let parsed = split_extra_args(r#"--model "gpt-6" -c 'foo=bar baz' --fast"#);
        assert_eq!(parsed, vec!["--model", "gpt-6", "-c", "foo=bar baz", "--fast"]);
    }

    #[test]
    fn fallback_skips_missing_primary_and_runs_next_available_backend() {
        use crate::config::BackendEntry;
        let mut cfg = AppConfig::default();
        let mut agy_prof = cfg.profile(CliKind::Agy);
        agy_prof.exe_path = "C:\\__clip_ai_missing_binary__\\agy.exe".to_string();
        cfg.set_profile(CliKind::Agy, agy_prof);
        cfg.custom_cli.command = "cmd.exe".to_string();
        cfg.custom_cli.args_template = "/c echo fallback_ok".to_string();
        cfg.custom_cli.use_stdin = false;
        cfg.backend_priority = vec![
            BackendEntry { kind: CliKind::Agy, enabled: true },
            BackendEntry { kind: CliKind::Custom, enabled: true },
            BackendEntry { kind: CliKind::Codex, enabled: false },
            BackendEntry { kind: CliKind::Claude, enabled: false },
        ];

        let cancel = CancelHandle::new();
        let mut started_kinds = Vec::new();
        let outcome = run_with_fallback(&cfg, "ignored", &cancel, |k| started_kinds.push(k));
        assert_eq!(started_kinds, vec![CliKind::Custom]);
        match outcome {
            ExecutionOutcome::Success { backend, answer, attempt_notes, .. } => {
                assert_eq!(backend, CliKind::Custom);
                assert_eq!(answer, "fallback_ok");
                assert!(attempt_notes.iter().any(|n| n.contains("agy: 跳过")));
            }
            other => panic!("expected fallback success, got {other:?}"),
        }
    }

    #[test]
    fn cancel_handle_aborts_immediately_without_running_backends() {
        let cfg = AppConfig::default();
        let cancel = CancelHandle::new();
        cancel.cancel();
        let mut started = false;
        let outcome = run_with_fallback(&cfg, "hello", &cancel, |_| started = true);
        assert!(!started);
        assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
    }

    #[test]
    #[ignore = "Requires live agy CLI authentication and network access"]
    fn live_agy_smoke_test() {
        let mut cfg = AppConfig::default();
        cfg.enable_fallback = false;
        let cancel = CancelHandle::new();
        let mut started_with = None;
        let outcome = run_with_fallback(
            &cfg,
            "1+1等于几？只回复一个数字，不要加标点或解释。",
            &cancel,
            |k| started_with = Some(k),
        );
        assert_eq!(started_with, Some(CliKind::Agy));
        match outcome {
            ExecutionOutcome::Success { backend, answer, .. } => {
                assert_eq!(backend, CliKind::Agy);
                let cleaned = crate::markdown_clean::clean_markdown(&answer);
                println!("LIVE AGY RESPONSE: {cleaned:?}");
                assert!(cleaned.contains('2'));
            }
            other => panic!("live agy test failed: {other:?}"),
        }
    }
}
