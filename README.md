<p align="center">
  <img src="assets/clip-ai.png" alt="clip-ai icon" width="96" />
</p>

# clip-ai

[![CI](https://github.com/Zalafina/clip-ai/actions/workflows/ci.yml/badge.svg)](https://github.com/Zalafina/clip-ai/actions/workflows/ci.yml)
[![Release](https://github.com/Zalafina/clip-ai/actions/workflows/release.yml/badge.svg)](https://github.com/Zalafina/clip-ai/actions/workflows/release.yml)

一个轻量级、原生 Windows 无界面剪贴板 & 划词 AI 快速问答托盘工具（Rust + 原生 Win32 API，编译为单文件 `.exe`，零外部运行库依赖）。

无需切换窗口，适合全屏游戏、阅读文档、浏览网页或专注编码时随时通过快捷键向本地已登录的 CLI AI Agent 提问，并将纯文本答案自动写回剪贴板。

---

## 核心特性

- **零焦点打扰（Fullscreen / Focus-Safe）**
  - 触发提问、后台运行、完成与报错全流程不弹窗、不抢前台窗口焦点、不弹出控制台黑框、不显示托盘悬停 Tips 或气泡提示。
- **三档提示词输入来源**（托盘右键菜单与设置窗口均可一键切换）
  - **智能混合（默认）**：优先通过 Windows UI Automation（`IUIAutomation` 焦点与鼠标指针单点探测）无侵入读取高亮划词；未命中且开启辅助取词且前台非终端窗口时，可使用受保护的 `Ctrl+C` 抓词（失败/取消时自动恢复原剪贴板）；仍无划词时回退读取剪贴板。
  - **仅剪贴板**：仅读取剪贴板文本，不探测前台窗口或发送任何模拟按键。
  - **仅划词高亮**：严格只通过 Windows UIA 读取当前高亮选区，**绝不触发 `Ctrl+C`，也绝不回退读取旧剪贴板**。
- **多场景精炼提示词与智能翻译**（托盘右键菜单与设置窗口双端实时联动）
  - 内置 **纯文本问答（默认）**、**智能翻译**、**提炼总结**、**润色改写**、**概念/代码解释** 与 **自定义提示词**。
  - **智能翻译**：自动识别源语言并翻译为用户选定的目标语言（支持 `中英互译`、`中日互译`、`简体中文`、`English`、`日本語`、`한국어`、`繁體中文`、`Français`、`Deutsch`）；若输入语言已与目标语言一致则原样输出纯文本；在托盘右键“翻译目标语言”点击任意语言即可一键切入翻译模式。
- **纯文本安全问答模式（默认开启）**
  - 从 CLI 参数与 `<user_input>` 定界层硬性禁止调用本地工具、终端命令及文件读写（`agy` 启用 `--sandbox` + 会话级 `permissions.deny`；`codex` 强制 `-s read-only` + `--disable shell_tool/unified_exec/apps` + `mcp_servers={}`；`claude` 启用 `--tools "" --strict-mcp-config --permission-mode dontAsk`）。
  - 可在设置或托盘菜单随时关闭该开关，恢复自由 Agent 模式。
- **动态四态托盘图标**
  - **蓝色**：待机
  - **橙色旋转动画**：处理中
  - **绿色**：完成（4 秒后自动回落待机）
  - **红色**：出错（保留状态直至下次触发、取消或打开设置排查）
- **可自定义 CLI 优先级与 Fallback 回退**
  - 默认按 `agy` → `codex` → `claude` 优先级顺序尝试，支持在设置窗口自定义上下排序、启用开关、模型、推理强度（如 `agy` 默认 `effort = low`）、额外参数及自定义 CLI 模板。
- **全局快捷键、冲突检测与专属取消快捷键**
  - 默认触发快捷键 `Ctrl + Alt + Q`，专属取消快捷键 `Ctrl + Alt + X`（通过 Win32 Job Object 立即终止整棵子进程树并中止后续回退）；设置快捷键时实时检测系统占用冲突。
- **内置合成提示音 & 纯文本清理**
  - 内存合成柔和 WAV 提示音（开始、完成、出错、忙音/取消及可选处理中间隔音，默认关闭间隔音），支持音量调节或自定义 `.wav` 文件；支持轻量清理常见 Markdown 格式符号并保留代码内容。

---

## 快速使用

1. 从 [GitHub Releases](https://github.com/Zalafina/clip-ai/releases) 下载最新 `clip-ai-*-x86_64-windows.zip` 并解压（或自行从源码构建）。
2. 确保本机已安装并登录至少一个支持的 CLI 工具（`agy` / `codex` / `claude`，或配置自定义 CLI）。
3. 启动 `clip-ai.exe` 后，任务栏托盘区会出现蓝色待机图标：
   - **提问**：鼠标划词高亮一段文本（或在仅剪贴板模式下先复制文本），按下 `Ctrl + Alt + Q`。
   - **等待**：听到开始提示音、图标变为橙色旋转动画；完成后听到完成提示音、图标变绿，答案已自动写入剪贴板可直接 `Ctrl + V` 粘贴。
   - **取消**：任务进行中按 `Ctrl + Alt + X` 即可立即中止。
   - **设置**：右键或双击托盘图标打开原生设置窗口（支持开机自启动与最近运行诊断/日志查看）。

### 配置与日志路径

- 默认保存于 `%APPDATA%\clip-ai\`（`config.json` 与 `clip-ai.log`）。
- **绿色便携模式**：在 `clip-ai.exe` 同目录新建一个空文件 `portable.flag`，配置与日志将改存为同目录下的 `config.json` 与 `clip-ai.log`。

---

## 从源码构建

### 环境要求

- Windows 10 / 11（`x86_64`）
- Rust 工具链（`x86_64-pc-windows-msvc`）与 Visual Studio Build Tools / Windows SDK

### 测试与构建命令

```powershell
# 运行单元测试与 UIA / Win32 GUI 测试
cargo test

# 运行 agy CLI 真实端到端冒烟测试（需本地已登录 agy）
cargo test live_agy_smoke_test -- --ignored --nocapture

# 编译发布版单文件（输出至 target\release\clip-ai.exe）
cargo build --release
```

### 自动云端构建与发布（GitHub Actions）

- 每次推送到 `main` 会自动触发 [CI](.github/workflows/ci.yml) 在 `windows-latest` 上运行全量测试与构建检查。
- 推送语义化版本标签即可自动编译、打包 `.zip` 与 `SHA256SUMS.txt` 并发布至 [GitHub Releases](https://github.com/Zalafina/clip-ai/releases)：

```powershell
git tag v0.1.1
git push origin v0.1.1
```

也可以在仓库的 **Actions → Release → Run workflow** 手动触发云端打包发布。

---

## License

本项目基于 [MIT License](LICENSE) 开源。
