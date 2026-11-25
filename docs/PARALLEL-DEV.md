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

## 租约：开工前登记你在改什么

worktree 解决了"同一条分支被两个线程抢"，但没有解决"两个线程在**不同** worktree 里改同一批文件"——那件事只能靠人在开工前就知道。租约把这条信息变成可查、可阻塞的东西：

```sh
script/lease claim crates/settings_content 'assets/locales/zh-CN/**' --note "设置项双语"
# token=l-1763...-1234-5678
export AMP_LEASE_TOKEN=l-1763...-1234-5678    # 让后续命令和提交钩子认得这是你的
...
script/lease release --all-mine
```

- `claim` 与任何**别人**的活动租约重叠时直接失败，打印持有者、它的 note、已持有多久、还剩多久。这是唯一一个"还没动手就知道撞车"的时刻。
- 长任务用 `heartbeat <token>` 续期，而不是一开始就 claim 一个很大的 ttl——线程意外死掉时，路径能早点放出来。
- `status` 列出所有活动租约并顺手清理过期的。

### 存储与格式

租约存在 `.git/amp-leases/<token>.lease`，跟着 `.git` 走：所有 worktree 共用一套（`git rev-parse --git-common-dir`），不进版本库，不污染工作区。

每个文件是**行导向的 `key=value`**，不是 JSON：

```
token=l-1763...
thread=T-01a20d9e-...
kind=work
note=设置项双语
created_at=1763000000
expires_at=1763007200
path=crates/settings_content/
path=assets/locales/zh-CN/
```

这样选的理由：纯 bash 就能正确解析（一行一个字段、`path=` 可重复），note 里有空格和引号也不用转义，不需要 jq、python 或 node。文件名用 `.lease` 而不是 `.json`，是因为内容确实不是 JSON，用 `.json` 的名字会误导下一个打开它的人。`--json` 是给脚本用的输出开关，与存储格式无关。

"查重 + 写入"不是一步，所以 `claim` 用 `mkdir .git/amp-leases/.claim-lock` 做原子守卫：`mkdir` 成功即获得锁，另一个人同一瞬间来 claim 会在重试里等它，只有一个能进去。锁在 claim 结束时释放；万一进程死在中间，超过 2 分钟的残锁会被下一个 claim 清掉。

### 路径重叠的判定（保守近似）

每条 claim 先归一成一个**目录前缀**，两个前缀互相包含就算重叠：

| 你 claim 的 | 归一成 |
| --- | --- |
| `crates/code_runner/**` | `crates/code_runner/` |
| `crates/settings_ui/src/*.rs` | `crates/settings_ui/src/` |
| `crates/settings_ui/src/a.rs` | `crates/settings_ui/src/` |
| `docs/x.md` | `docs/` |
| `**/*.rs` | `./`（整个仓库） |

**这是有意放宽的近似，会误报不会漏报。** 已知误报：

- 同一个目录下的两个不同文件会互相冲突（`a.rs` 与 `b.rs` 都归一到父目录）——这正是"文件精确路径按父目录参与比较"的代价；
- 中间带通配的 glob 只保留通配之前的部分，`crates/*/src/**` 退化成 `crates/`，会挡住 `crates/` 下的一切；
- `**/*.rs` 这类从仓库根开始通配的写法等于独占整个仓库。

宁可让人多说一句"这两个文件不冲突"，也不要让两个人各改一半。

有一条退化路径要提前写下来：归一化到父目录会让同一个大目录互斥，久而久之，负责大模块的线程会学着 claim 一个很大的目录，然后对所有告警免疫——那时租约退化成"每人大包一块"，只剩最粗的互斥。这是任何目录级租约的天然走向，不是这个实现的 bug；记在这里是为了它发生时能被认出来，而不是被当成"大家都不守规矩"。

### 过期不等于可以抢

租约过期只说明**持有它的线程可能已经死了**，不代表那段工作已经完成或可以覆盖。抢之前先看 `script/lease status` 里它留下的 note，以及那个 worktree 里的 `git status`——很可能有人改到一半的东西还在那儿。租约过期机制解决的是"别让死线程永久占着路"，不是"过期即安全"。

### 重构建锁

`release-fast` 构建和安装包打包必须持有全局唯一的 build 锁：

```sh
script/lease build claim     # 已有持有者时失败，并打印它持有了多久
script/lease build release
```

它和路径租约互不干扰（build 锁不占用任何路径），只有另一个 `build claim` 会与它冲突。原因和独立 target dir 一样：两个重构建同时跑会互相覆盖产物。

## 提交钩子

**由编排者在合并/发版窗口启用，不是每条线程自己开。**

```sh
script/setup-hooks      # 设置 core.hooksPath=.githooks，整个 clone 共享
```

`core.hooksPath` 是每个 clone 的共享配置：在四条线程同时跑着的时候打开它，会立刻改变它们的提交流程；而那些从没 claim 过路径的线程，会直接撞上别人租约的误报。所以它的位置是**合并与发版窗口**——编排者在合并前跑 `script/setup-hooks`，遇到带冲突的合并时用 `AMP_MAIN_COMMIT=1` 完成收尾提交；平时不开。`./script/pre-merge-check` 的第 10 项会报出当前状态。

启用后 `.githooks/pre-commit` 挡两件事：

1. **在主检出里手敲提交**——除非设置 `AMP_MAIN_COMMIT=1`。主检出是共享资源，随手提交是上面那个"merge commit 落到错误分支"事故的来源。
2. **提交里含别人持有租约的文件**——排除你自己 token（`AMP_LEASE_TOKEN`）或线程（`AMP_THREAD_ID`）的租约。提交这些文件会把持有者正在进行的工作劈成两半。

`AMP_MAIN_COMMIT=1` 同时会跳过租约检查：这类提交是收尾动作，合并本来就该由 `pre-merge-check` 在那之前放行。

### 钩子到底拦什么（临时仓库实测）

| 动作 | 经过哪个钩子 | 结果 |
| --- | --- | --- |
| 主检出里干净的 `git merge --no-ff <branch>` | 不经过：git 合并调用 `pre-merge-commit`，本仓库没提供它 | 通过，不需要变量 |
| 主检出里手敲 `git commit` | `pre-commit` | 拒绝 |
| 解冲突后手敲 `git commit` 结束合并 | `pre-commit` | 拒绝 |
| 同上，带 `AMP_MAIN_COMMIT=1` | `pre-commit` | 通过 |

看起来像"合并没被拦"，实际没有漏掉防御目的：要防的是**人在主检出里编辑并提交**，合并一直是允许的；而解冲突那一步恰是人正在手改文件、最容易把别人的半成品或错误分支带进提交的时刻，它落在 `pre-commit` 上，正好被挡住。所以 `AMP_MAIN_COMMIT=1` 只在你要手工提交时才需要，干净合并根本用不上——将来有人怀疑"合并怎么没被拦"，答案在这张表里，不用再跑一次实验。

**钩子是 worktree 里的文件**，不是全局安装的东西：`core.hooksPath` 只是配置，`.githooks/pre-commit` 必须在这个 worktree 的检出里存在且可执行。从一个还没有 `.githooks` 的旧分支（比如本分支合入之前的 `main`）开出来的 worktree，提交时**不会有任何拦截，也不会有任何提示**。`pre-merge-check` 的第 10 项会区分这两种情况：配置没开，还是配置开了但当前 worktree 里没有钩子。

### 钩子里的文本不要被 shell 展开

`.githooks/pre-commit` 里那句 "A clean `git merge` does not run this hook at all"
曾经让钩子在**每次拒绝提交时真的执行一次 `git merge`**：那段消息用的是未加引号的
`cat >&2 <<EOF`，而 heredoc 默认会做命令替换，正文里的反引号就被当成命令跑了。
本地 `main` 落后 `origin/main` 时，一次被拒的提交会顺手把 `main` 快进——这正是
"主检出只做合并、合并由人决定"要防的事，而且完全静默。

规则：**heredoc 的正文里要出现代码、命令或 `$()` 时，用引号版本 `<<'EOF'`。**
如果正文同时还需要展开某个变量（这段消息要展开 `$toplevel`），就把反引号转义成 `\``，
而不是为了躲开命令替换索性取消引号版本——那会把变量也变成字面量。

`script/hook-test` 盯着这一类：它把假的 `git` 放到 PATH 最前面，**直接运行钩子**并记录
每一次调用，断言没有 `merge`、也没有白名单之外的子命令（白名单目前是 `rev-parse`
和 `worktree`）。注意假 git 只有直接运行钩子时才生效：真提交时 git 会把自带的目录
前置到 PATH，假的 `git` 根本轮不到——所以这条测试不是靠"提交一次"来做的。

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
./script/pre-merge-check            # 日常
./script/pre-merge-check --release  # 发版/打包前
```

十项检查，逐条打印 PASS / FAIL / WARN。**只有 FAIL 影响退出码**，否则一个永远红的门禁最后会被所有人忽略。区分标准是"它会不会让当前这次操作出错"：

| 检查 | 类别 | 失败意味着 |
| --- | --- | --- |
| 主检出 `git status --short` 为空 | **阻塞** | 主检出是共享资源；合并会把别人没提交的东西卷进来，tag 也会指向被切走的分支 |
| 本地 `main` 与 `origin/main` 一致 | **阻塞** | 合并或打 tag 的基点不对；发版工作流构建的就是 tag 指向的 commit |
| 没有别人租约覆盖本分支改动的文件 | **阻塞** | 持有者还在改这些文件，合入会把他的工作劈成两半 |
| 当前分支包含 `origin/main` | 提示 | 要么从没合入的工作起的分支，要么只是落后；合并回来之前先把 `origin/main` 合进来 |
| 没有 worktree 停在已合入 `main` 的分支上 | 提示 | 活已经落地、目录还在；不影响这次操作的正确性，确认归属后 `git worktree remove` |
| 没有僵尸 worktree 元数据 | 提示 | 目录被手工删掉后留下的条目；`git worktree prune` 清掉 |
| 磁盘空闲 ≥ 60 GB | 提示（`--release` 时**阻塞**） | 编辑和合并不需要余量，但 `release-fast` 需要 25–30 GB 且链接时会涨 |
| 没有正在跑的 `cargo`/`rustc` | 提示 | 不影响这次合并，但会共用 target dir、拖慢自觉的发版 |
| build 锁空闲 | 提示（`--release` 时**阻塞**） | 有人正在做重构建，此刻发版会抢同一个 target dir |
| 提交钩子已启用 | 提示 | `core.hooksPath` 不是 `.githooks`，上面的两道提交拦截不会运行 |

`--release` 还会把残留 worktree 的清理命令逐条打印出来（`git worktree remove "<path>"  # <branch>`），省得手抄。

脚本不修任何东西：残留 worktree 只列路径和清理命令，由你确认后再执行。各项检查的判定细节看脚本头部注释。

关于第 5 项的边界：分支 tip 正好等于 main 的 worktree（刚创建、还没有自己的 commit）不会被列出，因为它和"刚刚被 fast-forward 合入 main"在 git 眼里完全一样，分不出来。反过来，一个刚创建的分支在 main 前进之后会出现在残留列表里——列表只是提示，删之前先确认那是不是别人正在用的工作区。

`PRE_MERGE_MIN_FREE_GB` 可以改磁盘阈值（比如换到更大的盘上做临时验证）。

## 租约机制的自测

```sh
script/lease-test
```

在临时仓库里覆盖：不重叠的租约共存、重叠被拒并指名持有者、过期后可再 claim、release 后可再 claim、`release --all-mine`、heartbeat 续期、build 锁唯一、`pre-commit` 在主检出上拒绝提交、`pre-commit` 在租约冲突时拒绝、`AMP_MAIN_COMMIT=1` 放行。跑完自删临时目录。
