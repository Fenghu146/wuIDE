# Wiki 交接文档：约定管线 + 仓库地图（P0）+ 符号级地图与上下文注入（P1）

> 分支 `wiki-p0` ｜ worktree `D:\wuIDE-main\wuIDE-wiki-p0` ｜ 基线 `d890f9b4` → 已 rebase 到 `c02f07c7`（main）
> 主回话（RemotePanel）的工作未被触碰；本分支独立开发、独立验证。
>
> **合并状态（2026-11-19）**：本分支已合并入 main（含 Pi-in-worktree 与交接文档），审查中发现并修复了刷新时符号构建标记残留的 bug（`4d8adb3d`）；仓库地图与 Remote Explorer 两个面板共存。

## 一、这个 P0 做了什么

对齐 Qoder 调研结论（Repo Wiki + 统一约定 + IDE 级上下文供给）的第一期落地：

1. **约定管线（AGENTS.md 契约层）**
   - 新 crate `repo_wiki` 的 `conventions` 模块：聚合「各 worktree 根目录的 AGENTS.md」+「用户全局 `%APPDATA%\<app>\AGENTS.md`」，附来源路径，单文件 64KB 截断保护。
   - 智能体聊天新增 `/conventions` 命令：把上述内容展开进输入框，**用户可审阅后再发送**（透明注入，不做黑盒）。
   - 仓库地图面板提供 AGENTS.md 状态行：存在 → 点击打开；缺失 → 点击创建中文模板（写入后自动刷新 worktree 快照并打开）。
   - 事实说明：Claude Code / Codex / OpenCode 在 worktree 根运行时**本来就原生读取 AGENTS.md**，所以 IDE 不对这三家重复注入；`/conventions` 的价值是让 DeepSeek / Pi 等非原生 harness 与用户本人拿到同一份约定。全局 AGENTS.md 此前在 `paths::agents_file()` 中是**死代码（无任何消费者）**，本 P0 让它真正生效。

2. **仓库地图面板（1a 结构树，零 LLM、零索引服务）**
   - 新 dock 面板 `RepoMapPanel`（左侧停靠，活动栏图标 ListTree）。
   - 纯由内存中的 worktree 快照构建（`Entry.size` 聚合）：目录按权重（字节）降序、文件按大小降序，逐级展开/收起，点击文件即在编辑器打开。
   - 每个 worktree 头部：总文件数/总大小 + 手动刷新按钮；激活面板时自动重建。
   - 不常驻任何后台索引/服务——展开状态仅存内存，面板关闭即无成本，符合本项目"内存友好原生编辑器"定位。

## 二、改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/repo_wiki/`（新增 crate） | `conventions.rs`（聚合/模板/截断 + 8 个单测）、`repo_map_panel.rs`（面板全部逻辑 + 3 个单测）、`repo_wiki.rs`（lib 根 + init） |
| `Cargo.toml` | members + workspace.dependencies 增加 `repo_wiki` |
| `Cargo.lock` | 随之更新 |
| `crates/agent_chat/Cargo.toml` | 依赖 `repo_wiki` |
| `crates/agent_chat/src/slash_commands.rs` | `AppCommand::Conventions`（第 9 个内置命令，名称 `conventions`；与 harness 命令重名时自动改前缀 `wu:conventions`） |
| `crates/agent_chat/src/chat_view/completion.rs` | `run_app_command` 新分支 → `insert_conventions` |
| `crates/agent_chat/src/chat_view.rs` | `insert_conventions`：后台读取 → 展开进输入框；已有文字则以空行拼接；无约定时 toast 提示 |
| `crates/wu/src/wu.rs` | `repo_wiki::init(cx)`；`initialize_panels` 加载并注册面板 |
| `crates/wu/Cargo.toml` | 依赖 `repo_wiki` |
| `crates/workspace/src/activity_bar.rs` | `DEFAULT_ORDER` 追加 `"RepoMapPanel"`（合并后数组共 8 项） |
| `assets/locales/zh-CN/strings/repo_wiki.json` | 新增（面板 5 条文案） |
| `assets/locales/zh-CN/strings/agent_chat.json` | 追加 2 条并整体按码点排序（`/conventions` 描述 + 无约定提示） |
| `assets/locales/zh-CN/actions/repo_map_panel.json` | 新增（`repo_map_panel::ToggleFocus` 名称/描述） |
| `assets/locales/zh-CN/action_namespaces.json` | 新增 `"repo_map_panel": "仓库地图"` |

## 三、与主回话（RemotePanel）的冲突面：**已实际解决**

主回话已将 RemotePanel 提交为 `c02f07c7`。本分支已 `git rebase main` 并解决全部冲突，当前历史为：

```
c02f07c7  Add a Remote Explorer panel to the sidebar   (main)
c94a746f  Add repo map panel and project conventions pipeline  (wiki-p0)
9eda43f3  Rank the repo map by symbol references instead of file size  (wiki-p0)
4d8adb3d  Clear in-flight symbol markers when refreshing the repo map  (wiki-p0)
```

**该分支现已可直接 fast-forward 合并，无需再解冲突。**

实际冲突与解决（供后续同类面板参考）：

| 文件 | 冲突点 | 解决方式 |
|---|---|---|
| `crates/wu/src/wu.rs` | `initialize_panels` 的 `let ..._panel = ...load(...)` 相邻行 | 两行都保留（`remote_panel`、`repo_map_panel`） |
| `assets/locales/zh-CN/action_namespaces.json` | 同一插入位置 | 两行都保留（`remote_panel`、`repo_map_panel`） |
| `crates/workspace/src/activity_bar.rs` | **git 自动合并但结果语义错误**：条目数变成 8，长度标注仍是 7 | 手动改为 `[&str; 8]` |
| `assets/keymaps/*.json` | 主回话加了 RemotePanel 键位；本分支未改 keymap | 自动合并，无冲突 |
| `assets/locales/zh-CN/strings/agent_chat.json` | 本分支重排该文件排序 + 2 条新键；主回话未触碰 | 无冲突 |
| `crates/recent_projects/*` | 主回话独有 | 无冲突 |

> **踩坑提醒（写给未来的自己）**：`activity_bar.rs` 的 `DEFAULT_ORDER` 是定长数组，两处分支各加一项时 git 会「干净地」自动合并成一个超长数组而**不报冲突**，但长度标注必须手动同步，否则编译期才暴露。凡是定长数组/`[T; N]`，合并后一律回读确认 N。

活动栏默认顺序（合并后）：`ProjectPanel, RemotePanel, SearchPanel, GitPanel, OutlinePanel, DebugPanel, AgentPanel, RepoMapPanel`。用户拖动后的顺序另有持久化，不受影响。

## 四、验证记录（本分支）

- `cargo check -p wu`：0 错误 0 警告（含新 crate、agent_chat、workspace、wu 全链路）。
- `cargo test -p repo_wiki`：16/16 通过（P1 后为 17/17）（聚合/去重/截断/模板/树构建/排序/格式化）。
- `cargo clippy -p repo_wiki -p agent_chat`：干净。
- `cargo test -p i18n`：12/12 通过（含 `simplified_chinese_strings_are_complete_and_consistent`——所有新 `t!` 均有 zh-CN 翻译且占位符一致）。
- `cargo test -p agent_chat`：108 通过 / 17 失败，**失败集与干净 HEAD 基线逐一相同**（stash 前后对比确认），全部为 SYNC.md 已记录的预存失败（依赖外部 harness 假件/协议），与本 P0 无关。
- **rebase 到 `c02f07c7`（含 RemotePanel）后重跑**：`cargo check -p wu` 通过、`repo_wiki` 8/8、`i18n` 12/12、clippy 干净——两个面板共存无回归。

## 五、已知限制（P0 范围内有意不做）

1. **无默认键位**：`repo_map_panel::ToggleFocus` 未绑定快捷键（主回话已用 `ctrl-alt-r` 绑给 `remote_panel::ToggleFocus`，本分支不抢键位；如需绑定，三个 `assets/keymaps/default-*.json` 各加一行即可）。命令面板可搜「仓库地图」，活动栏按钮直接可用。
2. **`/conventions` 仅支持独立命令**：整条消息就是 `/conventions` 时触发；不支持混排在长消息中（P1 可仿 extension 命令的行内展开机制补齐）。
3. **面板状态不持久化**：展开层级/激活状态不写盘（保持零成本；后续需要时仿 OutlinePanel 的 KeyValueStore 序列化）。
4. **无文件系统事件订阅**：结构在面板激活/手动刷新时重建，编辑过程中不自动跟随 fs 变化。
5. **`script/check-translations` 全量动作校验**需要先构建 IDE 二进制（`--dump-all-actions`）；本分支已按格式补全 actions JSON 与 namespaces，i18n 字符串测试全绿，但**合并后请跑一次完整脚本确认**。

## 五点五、P1 追加内容（符号级仓库地图 + 上下文注入）

### 做了什么

1. **符号级 repo map（`repo_wiki/src/symbol_map.rs`）**
   - 直接用 grammar 的 **outline query**（与大纲面板同一套查询，`Grammar.outline_config`）在**原生 tree-sitter** 上解析，不经过 Buffer/GPUI，全程后台线程。
   - 每个文件提取定义（名字/类型/行号）+ 标识符提及集合；用「A 提到了 B 定义的标识符」建跨文件引用图，跑 24 轮 **PageRank** 排序。
   - 预算三道闸：单文件 ≤256KB、单次最多 **900 个文件**、单次累计 ≤24MB。超限文件计入 `stats.too_large`。
   - 选片策略是**按顶层目录分桶轮转**（桶内按体积降序），避免「只取路径排序前 900 个」导致地图只描述仓库的字母序一角。
   - `repo_wiki::symbol_map::symbol_map_task(project, cx)` 返回可取消的 `Task<SymbolMap>`。

2. **面板接入**：文件行可展开显示定义列表（名字/kind/行号）、显示 `N 个符号` 与 PageRank 占比；标题行显示「正在扫描符号…」；工作树刷新按钮会重建符号表。点一下展开、双击开文件（浏览地图不抢编辑器焦点）。

3. **统一上下文注入语义**：`/conventions` 与新增的 `/repo-map` **都只写进输入框、绝不自动发送**——用户在发送前能看见 agent 到底拿到了什么。两条命令都支持**行内展开**：消息里单独一行写 `/repo-map`，发送时该行被替换为生成的 markdown，其余文字原样保留。

### 未做（诚实记录）

- **系统提示词级注入未做**：本仓库的 5 个 harness 都是外部 CLI，`RunRequest` 里没有 system prompt 字段，`claude.rs` 也没接 `--append-system-prompt`。要真做系统级注入得逐 harness 加 CLI 参数，属于新功能而非接线，故 P1 用「可审阅的输入框注入」替代。
- **无磁盘缓存**：每次面板激活/命令都会重新解析，切分支后需要重新扫描（约数秒）。缓存到 KV（key: 路径+mtime）留待 P2。
- **符号类型是 tree-sitter 节点名**（`function_item` 等），未映射到统一图标/分类。

### 验证

- `cargo test -p repo_wiki`：**17/17**（2026-11-19 复核）（含新增：真实 Rust 源文件提取定义、真实目录端到端构建、分桶选片、字节预算）。
- 端到端实测：对 `path/src` + `repo_wiki/src` + `text/src` 真实构建通过，0.32s。
- `cargo test -p agent_chat`：17 个预存失败不变，通过数 108 → **111**（新增 3 个上下文命令测试）。
- `cargo test -p i18n`：12/12。
- `cargo check -p wu` + `clippy`：0 错误 0 警告。

## 六、后续路线（已在调研报告中论证）

- ~~**P1**：tree-sitter 符号级 repo map + `/conventions` 行内展开 + 上下文注入~~ → 已完成（见五点五），其中「`RunRequest` 系统提示词注入」因 harness 无该字段而未做。
- **P2**：LLM wiki 生成（按需、按 git commit 增量、导出入库为 markdown，与 AGENTS.md 共用注入管线）+ 符号表磁盘缓存（key: 路径+mtime，需处理缓存失效）。
- 合并后建议在 `SYNC.md` 登记：`repo_wiki` 为 fork 自有 crate（低冲突面），活动栏/wu.rs 注册点为共享冲突区。

## 七、构建产物说明

- **更正（P1 期间实测）**：`CARGO_TARGET_DIR=D:\cargo-target` 由环境变量全局指定，本 worktree 与主回话**共用同一个 target 目录**（`.cargo/config.toml` 里没有单独覆盖）。P0 文档此前写的“worktree 本地 target”是错的。共用没有造成问题（cargo 有锁），但意味着两边并行构建会互相等待，且切换分支时增量编译会失效。
- worktree 目录内没有独立 target（共用上面的全局目录），所以 `D:\wuIDE-main\wuIDE-wiki-p0` 只有一个 git 检出的大小。合并后可直接 `git worktree remove ../wuIDE-wiki-p0`（先确认分支已合并或打 tag）。
