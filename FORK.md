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

- Windows / macOS / Linux 的图标全部换成分叉作者提供的插画：
  - `crates/wu/resources/app-icon.png`、`app-icon@2x.png`、`app-icon-dev.png`、`app-icon-dev@2x.png`
  - `crates/wu/resources/windows/app-icon.ico`、`app-icon-dev.ico`
  - `crates/wu/resources/Document.icns`
- 欢迎页/引导页沿用的 `assets/images/wu_icon.png` 现在是 `app-icon.png` 的逐字节副本（`sha256` 相同），旧鹿头图已删除。仓库里以 Wu 命名的图形资源只剩 `assets/images/wu_icon.png`、`assets/images/wu_logo.svg`、`assets/icons/wu_mark.svg`、`assets/icons/wu.svg` 与 `assets/badge/v0.json` 内嵌的 `logoSvg`；后四个是同一份从新图标提炼的单色人物剪影，不再是鹿角 W。`assets/images/wu_logo.svg` 只被组件库预览（`crates/ui/src/components/image.rs` 的 `VectorName::WuLogo`）引用。
- **来源与授权**：这套图标（`app-icon*.png`、`windows/app-icon*.ico`、`Document.icns`、`assets/images/wu_icon.png`，以及上面那几个剪影图形）由项目作者使用 AI 图像生成工具制作，风格参考自公开发表的作品，再由作者选稿、裁切与定稿，作为本项目的一部分按 GPL-3.0 一并分发。作者不对"风格"主张排他权利，也不在此作超出本项目许可证的授权；若有第三方认为其中某张图与其权利相冲突，请通过仓库 issue 联系，核实后会替换或移除。
- `assets/badge/v0.json` 是 shields.io endpoint 格式的徽章定义（`label` / `message` / `logoSvg` / `logoWidth` 等），仓库内没有代码读取它，只在该文件被外部徽章链接指向时生效；其中的 `logoSvg` 同样换成了新剪影。

## 5. 自动更新指向本仓库的 release

- 自动更新查的是 `https://api.github.com/repos/Fenghu146/wuIDE/releases`，也就是本仓库自己发布的版本。常量直接写仓库现名 `wuIDE`，不再靠 GitHub 改名留下的 301 跳转。上游 `dstrml/wu` 的 release（资产名还是 `Wu-*`）不再被考虑——否则会把本地改动覆盖回上游版本。因为客户端是匿名 HTTP，没有 token 机制，**这个仓库必须保持 public**：私有仓库的 release 匿名拉不到（404）。
- **默认开启**（`assets/settings/default.json` 里 `auto_update` 为 `true`）；不想自动查就在设置里关掉。自动检查失败是静默的（只写日志、状态回 Idle），手动点「检查更新」失败才会在标题栏留下一个红色错误按钮。启动时查一次，之后每 12 小时一次，失败不退避。
- 自动检查只发通知、不直接装：状态进 `AutoUpdateStatus::Available`，标题栏出现“有可用更新”按钮，同时弹一张带“下载并安装” / “跳过这个版本”的通知卡。点了才下载（有 digest 时校验 SHA-256）、覆盖当前安装，下次启动提示已更新。菜单里的“检查更新”算手动检查，一路装到底。
- “跳过这个版本”写进 kvp 的 `auto-updater-skipped-version`，只有出现更高版本才会再提示；同一版本在一次会话里也不会重复弹卡。
- 资产名与打包脚本对齐：`IDE-{arch}.dmg`、`ide-linux-{arch}.tar.gz`、`IDE-{arch}.exe`（`github_asset_name`）。改名后对不上会让 `release_asset` 直接报 “has no asset named …”。资产是在版本比对**之后**才找的，所以一个不比当前版本新的 release 即使没有对应资产，也不会让检查失败。
- macOS 的挂载点改为解析 `hdiutil attach` 的输出，不再写死卷名：卷名取自 bundle 名，重命名成 WuIDE 之后写死的 `Wu` 会让 `rsync` 指向不存在的路径。Linux 同理，包内目录是 `ide.app` + `libexec/ide`。

## 6. 打包

- `script/bundle-windows.ps1`：把 `crates/wu/resources/agents/*.exe`、`node_modules/` 与 `licenses/` 复制进安装包；`crates/wu/resources/windows/zed.iss` 增加 `agents\*`、`LICENSE.txt`、`NOTICE` 条目。
- `script/bundle-mac`：把对应架构的 `dsh-acp-darwin-*` 复制到 `Contents/Resources/agents/dsh-acp`。
- `script/bundle-linux`：把 `dsh-acp-linux-*` 复制到 `libexec/agents/dsh-acp`，并带上 `node` 与 `node_modules/`。
- `script/fetch-node`：macOS / Linux 打包时下载固定版本（当前 `24.22.1`，与仓库里的 `node.exe` 对齐）的官方 Node，并按 `SHASUMS256.txt` 校验；Windows 直接使用仓库里的 `node.exe`。
- 三个平台的打包脚本都会在缺少 `dsh-acp` 或 `node_modules/` 时直接失败，避免发出一个“设置里能看到智能体、点开却起不来”的包；缺少 `node` 只警告，因为 macOS/Linux 可以退回到 `PATH` 里的系统 Node。
- 运行期在 exe 同级的 `agents/`、`resources/agents/`、macOS `Contents/Resources/agents/`、Linux `libexec/agents/` 中查找内置二进制。
- **单独拷 exe 使用时，必须把 `agents/` 一起拷到 exe 旁边（或设置 `WU_AGENT_BIN_DIR`）**，否则 DeepSeek 与 Pi 不可用。查找顺序：`WU_AGENT_BIN_DIR` → exe 旁的发版位置（同上）→ 从 exe 目录、进程 cwd 与当前会话的项目根目录向上找 `crates/wu/resources/agents` 或 `agents/` → `PATH`。成功解析一次后在同一进程内记住，后续轮次不再依赖 cwd；解析失败时错误信息会列出尝试过的目录。
- 许可证随包分发（GPL 要求）：三个平台的脚本都把 `LICENSE-GPL`、`LICENSE-APACHE`、`NOTICE` 放进包里（macOS `Contents/Resources/`、Linux `share/licenses/`、Windows 安装目录），Windows 安装器还用拼好的 `LICENSE.txt` 作为 `LicenseFile` 在安装前展示。内置运行时的许可证在 `crates/wu/resources/agents/licenses/`，版本与校验方式记在 `agents/VERSIONS.md`，缺文件时打包直接失败。

## 7. 产品名称统一为 WuIDE

- 显示名为 **WuIDE**（stable）/ **WuIDE Dev**（dev），来自 `ReleaseChannel::display_name()`：窗口标题、macOS 菜单栏与应用菜单、关于窗口、通知、设置页、命令面板、CLI 帮助、`--system-specs` 与扩展提示都用它。`Wu` 是更早一代显示名，`IDE` 是中间一代。
- 所有中英文界面字符串同步改过（`assets/locales/zh-CN` 里的键与译文都按新英文原文更新），`cargo test -p i18n` 校验“每个字符串都有中文、每个中文都有出处”。
- Windows：可执行文件仍叫 `IDE.exe`，`crates/wu/Cargo.toml` 的 `[[bin]] name` / `default-run` 仍是 `IDE`；改的是用户能看到的版本资源（`crates/windows_resources` 的 ProductName / FileDescription = WuIDE / WuIDE Dev）与安装器显示名（Add/Remove Programs、开始菜单与桌面快捷方式、右键菜单、向导里的 “Installing…”）。
- 安装目录仍是 `{autopf}\IDE`：`bundle-windows.ps1` 新增 `$appDirName`，`zed.iss` 的 `AppName` 用显示名、`DefaultDirName` 用 `{autopf}\{#AppDirName}`，所以安装器文案是 WuIDE，落盘路径不变，升级时继续沿用已有目录。
- macOS：bundle 目录仍是 `IDE.app`，`Contents/MacOS/IDE` 不变；`script/bundle-mac` 在 `cargo bundle` 之后用 `PlistBuddy` 把 `CFBundleDisplayName` / `CFBundleName` 写成 `WuIDE` / `WuIDE Dev`，Finder、Dock、菜单栏与“关于”面板显示新名字。
- Linux：`.desktop` 的 `Name=` 是 WuIDE / WuIDE Dev，`APP_ID`（`me.farshed.Wu` / `-Dev`，同时也是 .desktop 文件名）、`Icon=ide`、`Exec=ide` 与 `Categories=…;IDE;` 都不动——桌面环境靠它们记住固定项与默认应用。
- `paths::APP_NAME` 早已是 `WuIDE`，所以用户档案目录是 `%LOCALAPPDATA%\WuIDE`、`~/Library/Application Support/WuIDE`、`~/.config/wuide`；`migrate_legacy_data_dir` / `migrate_legacy_config_dir` 会把 `IDE` / `ide` / `Wu` / `wu` 旧目录**复制**过来（源目录保留）。
- 刻意不跟随显示名改的标识符：`IDE.exe` 与 macOS `Contents/MacOS/IDE`、发版资产名 `IDE-{arch}.dmg` / `IDE-{arch}.exe` / `ide-linux-{arch}.tar.gz`（`github_asset_name` 硬编码，老客户端靠它找资产）、安装目录 `{autopf}\IDE`、crate 名 `wu`、`wu`/`ide` CLI、`wu://` 与 `ide` URL scheme、bundle id / AppId（`me.farshed.Wu*`、Inno 的 AppId）、单实例互斥体、AppUserModelID、动作与设置 ID（`wu::*`）、`base_keymap` 的存储值 `Zed`、`user_agent` 与 ACP/DAP 的 client 标识。
- 主题名保留 `IDE Dark` / `IDE Light` / `IDE Dark Translucent`（图标主题 `IDE Classic`）：它们是写进用户 `settings.json` 的值，改名要再加一层别名迁移，收益只是措辞，故本轮不动。
- 界面标识统一：欢迎页/引导页 logo 用的是 `assets/images/wu_icon.png`，它与 `crates/wu/resources/app-icon.png` 是同一张图（逐字节相同），所以界面 logo 与安装包图标始终一致；欢迎页 tagline 统一为“不打扰你的编辑器”；DAP `client_name`、更新助手窗口标题、Appx COM surrogate 的 `DisplayName` 也改为 WuIDE。
- 智能体聊天里的思考动画用 `assets/icons/wu_mark.svg` 的单色遮罩（`crates/agent_chat/src/chat_style.rs` 的 `LOGO_PATH`，渐变扫光保留）；它和 `assets/icons/wu.svg` 内容一致，都是 `viewBox="0 0 24 24"` 的单个 `currentColor` 路径（所以 `LOGO_ASPECT` 是 1:1），只保留鲍伯短发、刘海转角、圆脸和肩线这些能认出人像的轮廓，眼睛高光、衬衫纽扣、小花等在遮罩里会糊成一团的细节都去掉了。`assets/icons/agent_pi.svg` 是 Pi 的方块像素图标。
- `README.md` / `README.zh-CN.md` / `docs/INSTALL.md` / `NOTICE` / `.github/release/body.md` 里的产品名与文案同步为 WuIDE；下载链接和资产名保持原样。

按上面的规则归类：

| 类别 | 是否改成 WuIDE | 例子 |
| --- | --- | --- |
| 用户可见的显示名 | 是 | 窗口标题、关于面板、菜单 |
| 持久化的设置/主题值 | 否（迁移成本） | 主题 `IDE Dark` |
| 二进制/资产/scheme 标识符 | 否（兼容性） | `IDE.exe`、`wu://`、`wu::*` |
| 翻译字符串 | 是（通过 locale 文件） | 条目数随版本变化，见 `assets/locales/zh-CN/` |

## 已知限制

- **Pi 的 Node 运行时**：Windows 使用入库的 `node.exe`；macOS / Linux 由 `script/fetch-node` 在打包时下载对应架构的官方 Node（`crates/wu/resources/agents/node` 被 `.gitignore` 忽略）并校验 SHA256，因此打包机需要能访问 `nodejs.org`。打包脚本在缺少 `node`、`dsh-acp` 或 `node_modules` 时都会失败，不再发出缺少智能体的包。
- 仓库直接提交了 `crates/wu/resources/agents/` 下的运行时（`node_modules` 约 16k 个文件、`node.exe` 约 90MB）。这样做是因为打包脚本不做 `npm install`；如果要瘦身，需要先把“构建时下载/安装运行时”接进 `bundle-*` 与 CI，并重写 git 历史。
- **DeepSeek** 需要 `DEEPSEEK_API_KEY`（或 `dsh-acp --api-key`）；当前通过环境变量传入，后续可在设置页补充输入框。
- Pi 每次启动都会从 `~/.pi/agent` 镜像模型、凭据和设置；若本机没有 Pi 配置，则需要在终端执行一次 `pi` 登录/配置，或用 cc-switch 写入一份。因为每次都会重新镜像，在 WuIDE 里换的模型会被全局 Pi 默认值覆盖回去。
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