# 跟进上游

本仓库是 [dstrml/wu](https://github.com/dstrml/wu) 的分叉。这份文档记录跟进方式、当前进度和冲突高发区。

## 当前状态

| 项目 | 值 |
| --- | --- |
| 共同祖先 | `41b71ce4`（2026-11-11，上游 `Bump to 1.2.2`） |
| 上游领先 | 0 个 commit —— 已同步到上游最新 |
| 本分叉领先 | 36 个 commit |
| 同步标记 | `sync/upstream-41b71ce4`（上一次是 `sync/upstream-12c1985`） |
| `upstream` remote | 已配置（2026-11-12） |

最近一次合并：2026-11-12，把上游 9 个 commit 合入，冲突 4 个文件。

2026-11-12 再次检查：上游 `main` 仍然停在 `41b71ce4`，没有新 commit，无需合并。

查看实时状态：

```sh
script/sync-upstream          # 汇总 + 上游缺失的 commit + 冲突高发文件
script/sync-upstream --fetch  # 先 fetch 再汇总
```

## 一次性配置

```sh
git remote add upstream https://github.com/dstrml/wu.git
git fetch upstream
```

## 每次跟进

```sh
script/sync-upstream --fetch   # 先看要合什么、可能冲突在哪
git merge upstream/main
git tag -a sync/upstream-$(git rev-parse --short upstream/main) -m 'Merged upstream'
```

用 `merge` 不要用 `rebase`：分叉已经 32 个 commit，rebase 会重写历史，出事时看不出边界，也失去 `sync/*` 标记的对照意义。

## 冲突高发区

按改动密度排序，这些文件两边都动过，合并时最可能冲突：

| 区域 | 为什么冲突 |
| --- | --- |
| `crates/agent_harness/` | 本分叉在这里加了 ACP 驱动（`acp.rs`），上游同时在改 `claude.rs` / `codex.rs` |
| `crates/agent_chat/` | 本分叉加了 DeepSeek / Pi 两种智能体，上游在改会话与模型选择 |
| `crates/wu/Cargo.toml` | 本分叉改了 `[[bin]] name = "IDE"` 与 bundle 元数据 |
| `assets/locales/zh-CN/` | 本分叉补了大量中文翻译，上游也在加 |
| `script/bundle-linux` | 两边都在改打包流程 |
| `Cargo.lock` | 必然冲突；冲突时删掉自己的那半，合并后跑 `cargo check` 让 Cargo 重算 |
| `.github/release/CHANGELOG.md` | 两边都在顶部加条目 |

`Cargo.lock` 冲突的处理：

```sh
git checkout --theirs Cargo.lock   # 或手工删掉冲突标记
cargo check                        # 让 Cargo 重新解析
```

## 上游的 Zed 同步 PR（不要合）

上游把「同步 Zed 上游版本」做成 PR，而不是直接推 `main`。截至 2026-11-12，有两个这样的 PR：

| PR | 标题 | 状态 |
| --- | --- | --- |
| [#42](https://github.com/dstrml/wu/pull/42) | Sync Zed v1.27.0 | closed，**未合并** |
| [#28](https://github.com/dstrml/wu/pull/28) | Sync Zed v1.26.2 | closed，**未合并** |

两个 PR 的作者都在描述里写了同一句话：

> Some files were committed with conflict markers. Check out this branch, resolve them, amend, and push.

也就是**它们带着未解决的冲突标记**，上游自己也没接受。实测合 PR#42 会产生 109 个冲突文件，
其中大量是 `crates/gpui/`、`crates/editor/` 这类上游同步过来的大改动，而本分叉只改了其中少数几处。

判断原则：

- **不要**把 `refs/pull/*/head` 合进本分叉。它们是上游内部的候选分支，上游还没接受。
- `script/sync-upstream` 只看 `upstream/main`，这是本分叉跟进上游的唯一来源。
- 想看上游有没有在准备 Zed 同步，看 PR 列表和 `script/UPSTREAM_VERSION` 的值变化即可。
  2026-11-12 双方都是 `v1.22.0`；PR#42 会把它提到 `v1.27.0`。

## 已知冲突的解法（2026-11-12 合并实测）

这 4 个文件每次合并大概率还会冲突，按下面的方式解：

### `crates/wu/Cargo.toml` 与 `Cargo.lock`

只冲突 `version` 那一行。**保留本分叉的版本号**（当前 `1.3.2`）。本分叉版本线已经越过上游（`1.2.2`），取上游会让版本倒退，`release.yml` 的 tag 校验也会对不上。

`Cargo.lock` 其余部分让 Cargo 重新解析：

```sh
# 解完冲突标记后
script/build-macos check -p wu --bin IDE
```

### `script/bundle-linux`

冲突在构建 IDE 二进制的那一行。两边都要保留：

- 本分叉的 `--bin IDE`（上游是 `--bin wu`）
- 上游的 aarch64 ICF 保护（`icf_args`），否则 ARM Linux 上会启动崩溃

```sh
icf_args=()
if [[ "$(uname -m)" != "aarch64" ]]; then
    icf_args=(-C llvm-args=-addrsig -C link-arg=-Wl,--icf=safe)
fi
cargo --config .cargo/bundle-config.toml rustc --release --target "${target_triple}" \
    --package wu --bin IDE -- -C linker="$CC" -C link-arg=-fuse-ld=lld ${icf_args[@]+"${icf_args[@]}"}
```

### `crates/agent_chat/src/agent_panel.rs`

上游把每个智能体的图标和色调抽成了 `agent_logo(kind)` / `agent_tint(kind)` 两个函数。**采用上游的函数化写法，但要把 DeepSeek 和 Pi 补进这两个函数**——上游版本不认识它们，直接取上游会编译失败（match 不穷尽）。

```rust
fn agent_logo(kind: AgentKind) -> IconName {
    match kind {
        AgentKind::Claude => IconName::AgentClaude,
        AgentKind::Codex => IconName::AgentCodex,
        AgentKind::Opencode | AgentKind::DeepSeek => IconName::AgentOpencode,
        AgentKind::Pi => IconName::AgentPi,
    }
}
```

同理，任何上游新抽出来的 `match AgentKind` 辅助函数，都要检查是否覆盖了 5 个变体。

### `.github/release/CHANGELOG.md`

上游会加一个 `## 1.2.x` 小节，但本分叉的版本线里没有这个号。把上游的条目放进 `## Unreleased`，并标注 `(from upstream)`，不要引入一个本分叉永远不会发布的版本号。

## 合并后要检查的事

1. **`paths::APP_NAME`** 是否还是 `"WuIDE"`。上游不会动这一行，但如果合并时整块取了上游版本，改名会失效。
2. **`LEGACY_DATA_DIR_NAMES`** 是否还在 `crates/paths/src/paths.rs` 里，以及 `init_paths()` 里的两次 `migrate_legacy_*` 调用是否还在。
3. **`auto_update` 默认值**是否还是 `false`（`assets/settings/default.json`）。
4. **三个打包脚本**是否还带许可证复制逻辑（`LICENSE-GPL` / `LICENSE-APACHE` / `NOTICE` / `agents/licenses/`）。
5. **ACP 驱动**：`crates/agent_harness/src/acp.rs` 是否还在，`HarnessId::{Dsh, Pi}` 的穷举 match 是否还完整。
6. 跑一遍：

   ```sh
   cargo check -p agent_harness -p paths
   cargo test -p agent_harness -p paths
   cargo clippy -p agent_harness -p paths
   ```

   `cargo clippy` 要带 `--all-targets --all-features -- --deny warnings`，和 `script/clippy` 一致。
   不带这些参数时 `-p agent_harness` 的 lib target 不会被检查到。

   上游 `crates/agent_harness/src/usage.rs` 里的 `read_claude_credentials()` 在 Linux 上会触发
   `clippy::redundant_clone`（macOS 的 keychain 分支要用 `config_dir`，Linux 上不用）。
   本分叉已改成按 `as_deref()` 分支取值，两边都干净。

上游那些修复通常值得拿，比如 `Fix segfault on linux aarch64`、`Spawn console programs with CREATE_NO_WINDOW on windows`、`Fix model list order`。遇到与本分叉功能冲突时，优先保留上游的修复、把自己的改动挪到旁边。

## 版本号

本分叉的版本号（`crates/wu/Cargo.toml`）目前是 `1.3.2`，上游是 `1.2.2`。两条线已经交叉，所以**不要用版本号判断谁新**——用上面的 `script/sync-upstream` 和 `sync/*` 标记。

版本号也**不能**加 build metadata（`1.3.2+wu.1`）：Windows 安装器的 `VersionInfoVersion` 要求纯数字 `x.y.z.w`，加了会让打包失败。要区分分叉，用 git tag 而不是 crate 版本。

发版时用 `v<version>` 打 tag，`release.yml` 会校验 tag 与 `crates/wu/Cargo.toml` 一致，并检查 `.github/release/CHANGELOG.md` 里有对应条目。

## 上游远程现在指向哪里

`origin` 是本仓库（`Fenghu146/wu`），`upstream` 是 `dstrml/wu`。`git push` 默认推 `origin`，不会误推到上游。

## 本机构建环境的一个坑（macOS 27 + rustc 1.97.1）

直接 `cargo build` 可能报：

```
error[E0463]: can't find crate for `sqlez_macros`
```

这不是代码问题，也不是 `Cargo.lock` 问题。原因是 rustc 1.97.1 在 macOS 27 上生成的 proc-macro dylib，一旦被 strip 掉 debug info，dyld 会拒绝加载，报 `mis-aligned LINKEDIT string pool`，于是所有用到该宏的 crate 都"找不到 crate"。

绕过方式是用 `script/build-macos` 代替 `cargo`：

```sh
script/build-macos check -p db --lib
script/build-macos test -p agent_harness
script/build-macos build -p wu --bin IDE
```

它只是额外传了 `-C strip=none`。打包脚本本来就会 strip 发布产物（`bundle-mac` 里的 `strip -x`、`bundle-linux` 里的 `llvm-objcopy`），所以这个绕过只影响本地 debug 构建，不会让发出去的二进制变大。

另外，`cargo build -p wu` 还需要完整 Xcode（Metal 工具链），只有 Command Line Tools 会在 `gpui_apple` 的 shader 编译处失败：`xcrun: error: unable to find utility "metal"`。`cargo check` 不受影响。
