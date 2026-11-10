# Wu 私人分叉：改动说明

本仓库是 [dstrml/wu](https://github.com/dstrml/wu) 的私人分叉，目标是让这个内存友好的原生编辑器用起来更顺手。以下是相对上游的全部改动。

## 1. 新增两个内置智能体：DeepSeek 与 Pi

- 之前内置三种智能体（Claude Code / Codex / OpenCode）且无法在界面上选择开关；现在新增 **DeepSeek** 和 **Pi** 两种，并在设置页提供每个智能体的显示开关（`agent_chat.agents.*`，至少保留一个可见）。
- 新增通用 ACP（Agent Client Protocol）驱动：`crates/agent_harness/src/acp.rs`，复用仓库已有的换行分隔 JSON-RPC 层（`jsonrpc.rs`），把 ACP 的 `session/update` 翻译成 Wu 的 `AgentEvent` 流。
- 两种智能体通过 ACP 适配器运行：
  - **DeepSeek**：内置 `dsh-acp` 原生二进制（`@brokkai/dsh-acp`，无需 Node）。
  - **Pi**：内置 Node 运行时 + `pi-acp` + `@earendil-works/pi-coding-agent`（`crates/wu/resources/agents/node_modules`）。
- 类型系统改动：`HarnessId::{Dsh,Pi}`、`AgentKind::{DeepSeek,Pi}`、`AgentChatAgent::{DeepSeek,Pi}`、设置项 `agent_chat.agents.{dsh,pi}`，所有穷举 match 已补齐。

## 2. 智能体按需启动、闲置即退出

- 每个智能体在发起一轮对话时才 spawn 子进程，一轮结束即等待/终止（`kill_on_drop` + `interrupt_grace` 2s / `kill_grace` 3s），不常驻，保持内存友好。

## 3. 插件 / 配置与本机隔离

- 智能体的插件、凭据、会话状态默认落在自己的数据目录，不碰本机全局安装的智能体：
  - Windows：`%LOCALAPPDATA%\Wu\agents\<agent>`
  - macOS：`~/Library/Application Support/Wu/agents/<agent>`
  - Linux：`$XDG_DATA_HOME/wu/agents/<agent>`（回退 `~/.local/share/wu/agents`）
  - 可用环境变量 `WU_AGENT_DATA_DIR` 覆盖。
- DeepSeek 通过 `--data-dir` 指向该目录。
- Pi 额外准备私有 `$HOME`（`pi-home/.pi/...`），并首次运行时从用户已有的 `~/.pi/agent` 复制 `auth.json` / `models.json` / `models-store.json` 与去掉 `packages`、`enabledModels` 的 `settings.json`——保留模型与凭据，不装载需要 `npm` 的全局扩展。

## 4. 图标

- Windows / macOS / Linux 的图标全部换成分叉作者提供的图片：
  - `crates/wu/resources/app-icon.png`、`app-icon@2x.png`、`app-icon-dev.png`、`app-icon-dev@2x.png`
  - `crates/wu/resources/windows/app-icon.ico`、`app-icon-dev.ico`
  - `crates/wu/resources/Document.icns`

## 5. 关闭官方自动更新

- `crates/auto_update/src/auto_update.rs` 的 `poll()` 直接返回 Idle：私人分叉自己发版，不跟随上游 `dstrml/wu` 的 release，避免官方更新覆盖本地修改。

## 6. 打包

- `script/bundle-windows.ps1`：把 `crates/wu/resources/agents/*.exe` 与 `node_modules/` 复制进安装包；`crates/wu/resources/windows/zed.iss` 增加 `agents\*` 条目。
- `script/bundle-mac`：把对应架构的 `dsh-acp-darwin-*` 复制到 `Contents/Resources/agents/dsh-acp`。
- 运行期在 exe 同级的 `agents/`、`resources/agents/`、macOS `Contents/Resources/agents/` 中查找内置二进制。

## 已知限制

- **macOS 的 Pi 运行时**需要为对应架构打包一份 `node` 可执行文件；当前仓库只内置了 Windows 的 `node.exe`，macOS 上的 Pi 需要在打包机放置 `agents/node`（可执行）。
- **DeepSeek** 需要 `DEEPSEEK_API_KEY`（或 `dsh-acp --api-key`）；当前通过环境变量传入，后续可在设置页补充输入框。
- Pi 首次运行会复制用户 `~/.pi/agent` 的模型/凭据；若本机没有 Pi 配置，则需要在终端执行一次 `pi` 登录/配置。

## 构建（Windows）

```powershell
# 需要 Rust 1.97.1、VS BuildTools 2022、Windows SDK
cd script
.\bundle-windows.ps1   # 打包安装器
# 或仅开发构建
cargo build -p wu --bin wu
```

> **重要：Windows 构建产物不能带 Low 完整性标签。**
> 如果构建目录在受限环境（例如 DSH 沙箱）里生成，`target\debug`、`wu.exe` 和 `agents\*` 会被打上
> `Mandatory Label\Low Mandatory Level`。以 Low IL 启动的 Wu 无法写入 `%LOCALAPPDATA%\Wu`
> 与 `%APPDATA%\Wu`，表现为启动提示“无法加载数据库文件”、设置开关全部失效、日志不落盘、
> Pi 初始化失败。打包或分发前执行：
>
> ```powershell
> icacls "$wu\target\debug\wu.exe" /setintegritylevel Medium
> icacls "$wu\target\debug\agents" /setintegritylevel "(OI)(CI)Medium" /T /C
> ```
>
> `target\debug\agents` 还必须同时具备 `node.exe` 与 `node_modules\`（Pi 的运行时）。
