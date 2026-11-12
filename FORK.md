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
- Pi 额外准备私有 `$HOME`（`pi-home/.pi/...`），并在每次启动时从用户已有的 `~/.pi/agent` 镜像 `auth.json` / `models.json` / `models-store.json` 与去掉 `packages`、`enabledModels` 的 `settings.json`——保留模型与凭据，不装载需要 `npm` 的全局扩展。cc-switch 切换配置后写的就是 `~/.pi/agent`，所以下一次对话就会用上新供应商/模型，不需要清空隔离目录。

## 4. 图标

- Windows / macOS / Linux 的图标全部换成分叉作者提供的图片：
  - `crates/wu/resources/app-icon.png`、`app-icon@2x.png`、`app-icon-dev.png`、`app-icon-dev@2x.png`
  - `crates/wu/resources/windows/app-icon.ico`、`app-icon-dev.ico`
  - `crates/wu/resources/Document.icns`
- 欢迎页/引导页沿用的 `assets/images/wu_icon.png` 现在是 `app-icon.png` 的逐字节副本（`sha256` 相同），旧鹿头图已删除。仓库里仍以 Wu 命名的图形资源只剩 `assets/images/wu_icon.png`、`assets/images/wu_logo.svg`、`assets/icons/wu_mark.svg`（内容均已换新）与已无代码引用的 `assets/icons/wu.svg`。
- **来源与授权**：这套图标是作者从公开分享渠道保存的插画，来源方并未授予再分发许可。仓库因此不能作为可公开分发的软件看待：要么保持 private 自用，要么在公开之前把图标换成自己拥有权利的图（`app-icon*.png`、`windows/app-icon*.ico`、`Document.icns`、`assets/images/wu_icon.png` 这 7 个文件一起换）。

## 5. 自动更新指向本仓库的 release

- 自动更新查的是 `https://api.github.com/repos/Fenghu146/wu/releases`，也就是本仓库自己发布的版本。上游 `dstrml/wu` 的 release（资产名还是 `Wu-*`）不再被考虑——否则会把本地改动覆盖回上游版本。因为客户端是匿名 HTTP，没有 token 机制，**这个仓库必须保持 public**：私有仓库的 release 匿名拉不到（404）。
- 默认开启（`assets/settings/default.json` 里 `auto_update` 为 `true`）：启动时检查一次，之后每 12 小时一次；设置里关掉只会停止轮询，手动点“检查更新”仍可用。
- 自动检查只发通知、不直接装：状态进 `AutoUpdateStatus::Available`，标题栏出现“有可用更新”按钮，同时弹一张带“下载并安装” / “跳过这个版本”的通知卡。点了才下载（有 digest 时校验 SHA-256）、覆盖当前安装，下次启动提示已更新。菜单里的“检查更新”算手动检查，一路装到底。
- “跳过这个版本”写进 kvp 的 `auto-updater-skipped-version`，只有出现更高版本才会再提示；同一版本在一次会话里也不会重复弹卡。
- 资产名与打包脚本对齐：`IDE-{arch}.dmg`、`ide-linux-{arch}.tar.gz`、`IDE-{arch}.exe`（`github_asset_name`）。改名后对不上会让 `release_asset` 直接报 “has no asset named …”。资产是在版本比对**之后**才找的，所以一个不比当前版本新的 release 即使没有对应资产，也不会让检查失败。
- macOS 的挂载点改为解析 `hdiutil attach` 的输出，不再写死卷名：卷名取自 bundle 名，重命名成 IDE 之后写死的 `Wu` 会让 `rsync` 指向不存在的路径。Linux 同理，包内目录是 `ide.app` + `libexec/ide`。

## 6. 打包

- `script/bundle-windows.ps1`：把 `crates/wu/resources/agents/*.exe`、`node_modules/` 与 `licenses/` 复制进安装包；`crates/wu/resources/windows/zed.iss` 增加 `agents\*`、`LICENSE.txt`、`NOTICE` 条目。
- `script/bundle-mac`：把对应架构的 `dsh-acp-darwin-*` 复制到 `Contents/Resources/agents/dsh-acp`。
- `script/bundle-linux`：把 `dsh-acp-linux-*` 复制到 `libexec/agents/dsh-acp`，并带上 `node` 与 `node_modules/`。
- `script/fetch-node`：macOS / Linux 打包时下载固定版本（当前 `24.22.1`，与仓库里的 `node.exe` 对齐）的官方 Node，并按 `SHASUMS256.txt` 校验；Windows 直接使用仓库里的 `node.exe`。
- 三个平台的打包脚本都会在缺少 `dsh-acp` 或 `node_modules/` 时直接失败，避免发出一个“设置里能看到智能体、点开却起不来”的包；缺少 `node` 只警告，因为 macOS/Linux 可以退回到 `PATH` 里的系统 Node。
- 运行期在 exe 同级的 `agents/`、`resources/agents/`、macOS `Contents/Resources/agents/`、Linux `libexec/agents/` 中查找内置二进制。
- 许可证随包分发（GPL 要求）：三个平台的脚本都把 `LICENSE-GPL`、`LICENSE-APACHE`、`NOTICE` 放进包里（macOS `Contents/Resources/`、Linux `share/licenses/`、Windows 安装目录），Windows 安装器还用拼好的 `LICENSE.txt` 作为 `LicenseFile` 在安装前展示。内置运行时的许可证在 `crates/wu/resources/agents/licenses/`，版本与校验方式记在 `agents/VERSIONS.md`，缺文件时打包直接失败。

## 7. 产品名称改为 IDE

- 窗口标题、菜单、关于窗口、设置页、对话框、CLI 帮助、`user_agent`/系统信息里的产品名从 Wu 改为 **IDE**（macOS bundle 名为 `IDE` / `IDE Dev`）。
- Windows 上真正落盘的可执行文件也叫 `IDE.exe`：`crates/wu/Cargo.toml` 的 `[[bin]] name` / `default-run` 改为 `IDE`，Windows 版本资源（ProductName / FileDescription）改为 IDE / IDE Dev。
- `paths::APP_NAME` 改为 `IDE`，所以用户档案目录（设置、会话、日志）现在是 `%LOCALAPPDATA%\IDE`、`~/Library/Application Support/IDE`、`~/.config/ide`。**代价**：已经在用 Wu 的机器升级后是空档案，旧配置不会自动迁移；想保留旧档案就单独回退 `paths::APP_NAME` 那一行。
- 刻意保留的标识符：`wu://` URL scheme、`wu` CLI 名（Linux 上是 `ide` 的软链，Windows 上是 `ide.exe` 的副本）、crate 名、bundle id / AppId、单实例互斥体、动作与设置 ID（`wu::*`，命令面板里因此仍显示 `wu:` 前缀）、Windows 安装目录 `{autopf}\IDE`（仅升级时沿用旧的 `{autopf}\Wu`）。`base_keymap` 的显示名已改为 IDE，但存储值仍是 `Zed`，不影响旧设置。主题名不再是保留项，见下文。
- 界面标识统一：欢迎页/引导页 logo 用的是 `assets/images/wu_icon.png`，它与 `crates/wu/resources/app-icon.png` 是同一张图（逐字节相同），所以界面 logo 与安装包图标始终一致；欢迎页 tagline 统一为“不打扰你的编辑器”；DAP `client_name`、更新助手窗口标题、Appx COM surrogate 的 `DisplayName` 也改为 IDE。
- 安装器侧跟进：`bundle-windows.ps1` 打包 `IDE.exe`，`zed.iss` 的 `AppExeName` 为 IDE；显示名（Add/Remove Programs、开始菜单、右键菜单）与 Linux `.desktop` 的 `Name` 也改为 IDE，但只改标签、不改 ID 与路径。
- 打包链路全部跟着改名对齐：`bundle-mac` / `bundle-linux` / `check-translations` 用 `--bin IDE` 与 `release/IDE`，macOS 可执行文件为 `Contents/MacOS/IDE`（dSYM 也改名为 `IDE.dSYM`），`explorer_command_injector` 的 Appx 清单与右键菜单指向 `IDE.exe`，`uninstall.sh` 清 `IDE.app` 与 `~/.config/ide`、`~/.local/share/ide`。
- `wu` CLI 会先找 `IDE.exe`，再回退到 `Wu.exe`，所以改名之前安装的机器也能用。
- 智能体的运行期目录仍按旧名：`acp.rs` 里各 agent 的数据根目录还是 `Wu`/`wu`（`WU_AGENT_DATA_DIR` 可覆盖），所以 Pi/DeepSeek 的凭据与插件不会因改名失效。
- 仍然叫 Wu 的还有：命令行里的 `wu`（安装目录里的 `bin/wu.exe` / `bin/wu` 是 `ide` 的兼容副本）、`wu://` URL scheme、crate 名、AppId 与单实例互斥体、数据目录里的 `wu-<channel>.sock` 与 `.wu_server` / `.wu_wsl_server`。发版文件名（`IDE-*.dmg`、`ide-linux-*.tar.gz`、`IDE-x86_64.exe`）与 Linux bundle 内的 `libexec/ide` 已经是 IDE；安装目录也只在升级时沿用旧的 `{autopf}\Wu`。
- 主题与图标主题名已随 IDE 改名：`assets/themes/wu/wu.json` 里是 `IDE Dark` / `IDE Light` / `IDE Dark Translucent`，用户设置中的旧名由 `crates/theme_settings/src/settings.rs` 的 `migrate_theme_name` 和 `crates/theme/src/registry.rs` 的 `ThemeRegistry::get` 兜底迁移；图标主题是 `Symbols Dark` / `Symbols Light`（`assets/icon_themes/symbols/icon_theme.json`，`assets/settings/default.json` 默认用它们），代码内置的兜底图标主题名是 `IDE Classic`（`crates/theme/src/icon_theme.rs` 的 `DEFAULT_ICON_THEME_NAME`），`Wu Classic` / `Wu (Default)` 只作为 legacy 常量存在，由 `ThemeRegistry::get_icon_theme` 映射过来。
- 智能体聊天里的思考动画用 `assets/icons/wu_mark.svg` 的单色遮罩（`crates/agent_chat/src/chat_style.rs` 的 `LOGO_PATH`，渐变扫光保留）；它与 `assets/icons/wu.svg` 的 `viewBox`（`205 1 1053 1551`）和文件结构一致，已是同一套人物剪影，不再是鹿头轮廓。`assets/icons/agent_pi.svg` 是 Pi 的方块像素图标。
- `README.md` / `README.zh-CN.md` / `docs/INSTALL.md` 已改成本分叉的文案：正确的下载资产名、未签名构建的处理方式与 SHA-256 核对步骤、更新机制、出站请求清单、许可证与源码获取方式，并链接回本文件。

## 已知限制

- **Pi 的 Node 运行时**：Windows 使用入库的 `node.exe`；macOS / Linux 由 `script/fetch-node` 在打包时下载对应架构的官方 Node（`crates/wu/resources/agents/node` 被 `.gitignore` 忽略）并校验 SHA256，因此打包机需要能访问 `nodejs.org`。打包脚本在缺少 `node`、`dsh-acp` 或 `node_modules` 时都会失败，不再发出缺少智能体的包。
- 仓库直接提交了 `crates/wu/resources/agents/` 下的运行时（`node_modules` 约 16k 个文件、`node.exe` 约 90MB）。这样做是因为打包脚本不做 `npm install`；如果要瘦身，需要先把“构建时下载/安装运行时”接进 `bundle-*` 与 CI，并重写 git 历史。
- **DeepSeek** 需要 `DEEPSEEK_API_KEY`（或 `dsh-acp --api-key`）；当前通过环境变量传入，后续可在设置页补充输入框。
- Pi 每次启动都会从 `~/.pi/agent` 镜像模型、凭据和设置；若本机没有 Pi 配置，则需要在终端执行一次 `pi` 登录/配置，或用 cc-switch 写入一份。因为每次都会重新镜像，在 IDE 里换的模型会被全局 Pi 默认值覆盖回去。
- Pi 的隔离 `$HOME`（`Wu/agents/pi-home`）里存着 API 凭据和完整会话记录，现在目录是 `0700`、文件是 `0600`（`acp.rs` 的 `restrict_to_owner`）。在修复之前它跟 umask 走，默认是 `0755`/`0644`，同机其他用户可读。
- **自动更新只校验完整性，不校验身份**：客户端比对的是 GitHub 为资产发布的 SHA-256 digest，能发现传输损坏，但不能证明文件是谁构建的。任何能向本仓库发 release 的人（token 泄露、CI 被投毒）都能把代码推到所有安装上。

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
