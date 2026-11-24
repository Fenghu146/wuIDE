# 多线程并行开发的工作区约定

这个仓库经常同时有多个执行线程在改：人、多个 agent 会话、以及它们各自打开的 worktree。它们共用一台机器、一份 `.git`。主检出（`D:\wuIDE-main\wuIDE-main`）是其中最容易被踩坏的东西，因为它同时是发版工具的工作目录。

`AGENTS.md` 里有一份精简版规则，这里是展开说明和理由。

## 为什么主检出必须干净

主检出不是某个线程的工作目录，它是发版工具的工作目录：

- `script/release` 只在 `main` 上跑，一上来就要求工作区干净，否则 `fail "working tree is not clean"`；
- 发版要 `git tag`，tag 打在 `HEAD` 上——HEAD 被别人切走过的分支，tag 就落到别处；
- `.github/workflows/release.yml` 和 `mac-dmg.yml` 构建的是 tag 指向的 commit，tag 错了发出去的就是错的代码；
- 主检出里同时躺着两份未提交改动时，只能靠 `git hash-object` 逐个比对才能分清谁的改动是谁的，这已经真实发生过一次（`settings_content.rs` / `page_data.rs` / `default.json` / `settings_ui.json`）。

所以主检出只做三件事：**合并、发版、打 tag**。编辑一律在独立 worktree 里。

另一个已经发生过的事故是两个线程在主检出里抢分支切换：一个线程 `git checkout -b` 之后，另一个线程的提交落到了前者的分支上，产生一个落在错误 feature 分支上的 merge commit。独立 worktree 从根上消除了这个问题——每个 worktree 有自己的 `HEAD`，切分支不再影响别人。

## 开一个线程

```sh
git -C D:\wuIDE-main\wuIDE-main worktree add D:\wuIDE-<name> -b <branch> origin/main
```

- 一律从 `origin/main` 起分支。从别人未合入的分支起，会把他的半成品带进你的 diff。
- 一个线程一个目录、一个分支。在同一个 worktree 里再切分支，等于让两个线程共用一个 `HEAD`，回到上面那个事故。
- 收工后 `git worktree remove D:\wuIDE-<name>`。分支合入 main 之后，这个 worktree 就只剩占地方和误导人（`./script/pre-merge-check` 会把它们列出来，但不会替你删）。

## 构建

```powershell
$env:CARGO_TARGET_DIR = "D:\cargo-target\<name>"
```

或者给单次命令加 `--target-dir D:\cargo-target\<name>`，用完删掉目录。

共享 target dir 的问题不是"慢"，而是**错**：并发构建会互相覆盖中间产物，报错会指向已经不存在的代码。典型症状是"源码里明明存在的字段报缺失"，这类错误看起来像代码问题，实际是产物问题，排查成本很高。

`release-fast` 一次要 25–30 GB，链接阶段可能翻倍，所以 `./script/pre-merge-check` 要求至少 60 GB 空闲；磁盘不足时优先清理自己名下的 target dir。

## 提交前

```sh
git status --short
```

只应看到你自己改的文件。看到别人的文件时：

- **停下来报告**，不要 `git checkout --`、`git stash`、`git reset`——那是别人还没提交的工作，清掉就没了；
- 也不要把它们顺手带进你的 commit。`git add -A` 是这类误带的常见来源，按具体路径 `git add <file>`。

不要顺手 `cargo fmt` 别人的文件：整文件格式化会把无关行卷进你的 diff，扩大冲突面。

## 用户可见文案与枚举设置

新增或修改界面文案必须同时给英文（默认）和简体中文。`script/check-translations` 是发版门禁，mac 打包和 release workflow 都会跑，缺中文会失败。

枚举类设置的取值要同时改三处，漏一处 `settings_ui` 的翻译测试就会红：

1. `crates/settings_ui/src/components/dropdown.rs` 的 `translated_setting_value` 里加 match 分支；
2. `assets/locales/zh-CN/strings/settings_ui.json` 里加 `"setting value|<下拉显示值>"`；
3. 该设置的 schema 描述。

个别取值故意保留英文时，写进同文件里的 `UNTRANSLATED_SETTING_VALUES` 白名单，而不是让测试红。

## 合并前

```sh
./script/pre-merge-check
```

五项检查，逐条打印 PASS/FAIL，任一 FAIL 退出码非 0：

| 检查 | 失败意味着 |
| --- | --- |
| 主检出 `git status --short` 为空 | 有人正在主检出里改东西，合并会把它卷进去 |
| 没有 worktree 停在已合入 `main` 的分支上 | 有线程的活已经合入，目录还在；确认归属后 `git worktree remove` |
| 本地 `main` 与 `origin/main` 一致 | 合并或打 tag 的基点不对，先 fast-forward 或 push |
| 所在磁盘空闲 ≥ 60 GB | `release-fast` 可能中途失败（可用 `PRE_MERGE_MIN_FREE_GB` 改阈值） |
| 没有正在跑的 `cargo`/`rustc` | 可能在共用 target dir，产物和耗时都不可信 |

脚本不修任何东西：残留 worktree 只列路径和清理命令，由你确认后再执行。各项检查的判定细节看脚本头部注释。

关于第 2 项的边界：分支 tip 正好等于 main 的 worktree（刚创建、还没有自己的 commit）不会被列出，因为它和"刚刚被 fast-forward 合入 main"在 git 眼里完全一样，分不出来。反过来，一个刚创建的分支在 main 前进之后会出现在残留列表里——列表只是提示，删之前先确认那是不是别人正在用的工作区。
