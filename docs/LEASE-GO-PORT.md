# `script/lease` 的 Go 试点

用 Go 重写 `script/lease`，作为"外围工具换语言"的第一个试点。目标不是替换，而是
**同时存在、互相看得见、用同一套测试对照**，再决定要不要切换。

- 分支：`feat/lease-go`
- 基线：`origin/main` @ `f88814ea`
- 二进制**不进仓库**，产物在仓库外（见"构建"）
- `.githooks/pre-commit` 与 `script/pre-merge-check` **未改动**，仍调用 bash 版

## 构建与运行

```sh
# 正常安装：建钩子，并把编译版建到共享 git 目录里（不进工作区）
script/setup-hooks

# 也可以单独构建到仓库外，方便对照
(cd script/lease-go && go build -o D:/Amp/builds/wulease.exe .)

# 对照测试：必须显式用 Git Bash
"C:/Program Files/Git/bin/bash.exe" script/lease-test --compare D:/Amp/builds/wulease.exe

# 单跑一边（不给 LEASE_BIN 就是 bash 版）
script/lease-test
LEASE_BIN=D:/Amp/builds/wulease.exe script/lease-test
```

`setup-hooks` 把产物放到**共享 git 目录**下的 `amp-bin/wulease.exe`，也就是
`$(git rev-parse --git-common-dir)/amp-bin/`。放这里而不是 `<repo>/.git/`：工作树的
`.git` 是一个文件而不是目录，写不进去；而且放共享目录里，后加的 worktree 不用再建一次。

`.githooks/pre-commit` 与 `script/pre-merge-check` **优先用这个二进制，找不到就回落到
bash 版并打印一行提示**（不静默、也不失败）。

Windows 上 PATH 里的 `bash` 常常是 **WSL** 的 bash（这台机器是 5.2.21，Linux），Git Bash
在 `C:\Program Files\Git\bin\bash.exe`。对照实验必须显式调 Git Bash，否则两边的
文件系统视图不一样，数字没有意义。

## 为什么两个实现能共存

两边都用 `git rev-parse --git-common-dir` 定位租约目录，所以都落在
**主检出的** `.git/amp-leases/`，所有 worktree 共享同一份。`wulease.exe` 因此不是
"另一套存储"，而是同一份租约的另一个读者/作者。

> 这是 Go 版的硬性实现约束：必须用 `--git-common-dir`，不能用 `--git-dir` 或
> `--git-path`。后者在 linked worktree 里会指到 `.git/worktrees/<name>/`，两边就
> 看不见对方了。

文件格式逐字节一致，键序固定为：

```
token=l-<unix>-<pid>-<rand>
thread=<AMP_THREAD_ID 或 token>
kind=work|build
note=<单行>
created_at=<unix>
expires_at=<unix>
path=<归一化前缀>        # build 锁没有 path 行
```

## 新增与改动的文件

| 文件 | 行数 | 说明 |
| --- | --- | --- |
| `script/lease-go/main.go` | 1026（空行 92、注释 65，代码 869） | Go 实现，仅用标准库 |
| `script/lease-go/go.mod` | 2 | `module wulease`, `go 1.22` |
| `script/lease-test` | 393（原 183） | 改为参数化 runner，默认行为不变 |
| `script/setup-hooks` | 49（原 25） | 增加构建编译版的步骤，Go 缺失时降级 |
| `.githooks/pre-commit` | 76（原 48） | 优先用编译版，回落 bash 时提示 |
| `script/pre-merge-check` | 337（原 300） | 同上，一处解析供两节使用 |

`script/lease-test` 的 diff 是 +314 / −141，但**忽略空白后只有 +194 / −21**：26 个用例
的断言体一行没改，增量基本来自"整段搬进 `run_suite` 函数"带来的缩进。

`script/lease-go/` 只有源码，没有二进制，也不带 `vendor/`（零外部依赖）。

## 26 项对照测试

`script/lease-test --compare <bin>` 对两个实现各跑一遍同一套用例，输出两列。

```
#    case                                                     lease    wulease.exe
--------------------------------------------------------------------------------
1    setup-hooks points core.hooksPath at .githooks           PASS     PASS
2    claim prints a token                                     PASS     PASS
3    a non-overlapping claim succeeds                         PASS     PASS
4    an overlapping file claim is refused and names the holder PASS     PASS
5    status shows the holder's note                           PASS     PASS
6    a claim inside someone's glob is refused                 PASS     PASS
7    claim --json emits a parseable result                    PASS     PASS
8    release frees the paths                                  PASS     PASS
9    the same path can be claimed after release               PASS     PASS
10   release --all-mine drops that thread's leases            PASS     PASS
11   the dropped paths are free again                         PASS     PASS
12   an expired lease no longer blocks a claim                PASS     PASS
13   status drops the expired lease                           PASS     PASS
14   heartbeat extends a lease                                PASS     PASS
15   a heartbeated lease still blocks after its original ttl  PASS     PASS
16   two claims for the same paths at the same instant: one wins (5 rounds) PASS     PASS
17   the build lock can be taken                              PASS     PASS
18   a second build claim is refused and says how long it has been held PASS     PASS
19   the build lock does not block path claims                PASS     PASS
20   the build lock is released                               PASS     PASS
21   the build lock can be taken again                        PASS     PASS
22   a commit in the shared main checkout is refused          PASS     PASS
23   AMP_MAIN_COMMIT=1 allows a commit there                  PASS     PASS
24   a commit touching another thread's lease is refused      PASS     PASS
25   the refusal points at script/lease status                PASS     PASS
26   the commit goes through after the lease is released      PASS     PASS
--------------------------------------------------------------------------------
     total                                                    26/26    26/26
```

第 22–26 项走的是 `.githooks/pre-commit`。runner 对被测的编译版做两件事：在临时
worktree 的 `script/lease` 放一个 `exec` shim（测试里直接调用它），并把它复制到
`.git/amp-bin/wulease.exe`（钩子优先找的位置）。所以这 5 项测到的是**钩子真正会走的
那条路**，而不是 shim 兜出来的。测 bash 版时不存在 `amp-bin`，钩子于是走回落路径——
两条路都覆盖到了。

## 切换默认实现：钩子优先用编译版

`script/setup-hooks` 现在多做一步：把 `script/lease-go` 建到
`$(git rev-parse --git-common-dir)/amp-bin/wulease.exe`。注意是**进模块目录**再
`go build .`——仓库根没有 `go.mod`，`go build ./script/lease-go` 会报
"cannot find main module"。

实测输出（一次性仓库，`script/lease-go` 与 `setup-hooks` 拷进去）：

没有 Go 时——降级，不失败：

```
core.hooksPath=.githooks
Enabled hooks: pre-commit
Go is not on PATH; hooks use script/lease instead.
Install Go and rerun this script to switch to the compiled lease tool.
exit=0
amp-bin present? no
```

有 Go 时：

```
core.hooksPath=.githooks
Enabled hooks: pre-commit
Built /tmp/setup.XXXXXX/r/.git/amp-bin/wulease.exe
exit=0
amp-bin present? yes
built binary answers: Usage: script/lease <command> [args]
```

提交路径实测（链接工作树里的 `.githooks/pre-commit`）：

| 场景 | 钩子输出 | 结果 |
| --- | --- | --- |
| 没有 `amp-bin` | `pre-commit: using script/lease; run script/setup-hooks to build the compiled lease tool` | 提交成功 |
| 有 `amp-bin` | 无提示 | 提交成功 |
| 有 `amp-bin`，别人持有 `blocked/` | holder / token / note / `overlaps blocked/ (theirs) and blocked/ (yours)` + 说明块 | 拒绝，exit 1 |
| 持有者释放后、同一笔提交 | 无提示 | 提交成功 |

`amp-bin/` 在 `.git` 里面，`git status` 看不到它，也不会被提交。

## 性能对照

同一台机器（16 个逻辑核，与别的进程共用），同一批租约（直接按上面的格式铺设
20 / 50 个 `work` 租约，`expires_at` 在 2 小时后），每次先预热一遍，再取 **9 次**的
中位数，单位秒。Go 侧已经是"不调 git"的版本。

| 租约数 | 操作 | bash | Go | 倍率 |
| ---: | --- | ---: | ---: | ---: |
| 20 | `status` | 1.0222 | 0.1793 | 5.7× |
| 20 | `claim` | 1.5747 | 0.1895 | 8.3× |
| 20 | `claim --json` | 1.9142 | 0.1996 | 9.6× |
| 50 | `status` | 1.0023 | 0.2133 | 4.7× |
| 50 | `claim` | 1.3574 | 0.1740 | 7.8× |
| 50 | `claim --json` | 2.2022 | 0.2374 | 9.3× |

去掉 `git` 子进程之前，同一个 Go 构建在 20 个租约下是 `status` 0.3491、`claim` 0.3840，
所以这一步把它砍掉了一半左右。

### 这些数字说明什么

**1. bash 版的耗时与租约数量无关。** 20 → 50 个租约，`status` 1.0222 → 1.0023，
`claim` 1.5747 → 1.3574（后者偏低是噪声）。当初 9.8s → 1.3s 那轮优化已经把"每租约
成本"消干净了，现在剩下的全是固定开销。

**2. bash 的固定开销是 fork 和进程启动。** 单项拆解（同机）：

| 单项 | 秒 |
| --- | ---: |
| Git Bash 启动（`bash -c ':'`） | 0.2250 |
| `git rev-parse --git-common-dir` | 0.2100 |
| `bash -c 'date +%s'`（一次 fork） | 0.2869 |

一次 fork 约 0.06s，而 bash 版 `status` 至少有 `git`、`date`、`mkdir` 三处子进程，
`claim` 还要多一处 `$(normalize ...)`；`--json` 更进一步，每个字段一次
`$(json_string ...)`，所以比 `claim` 多出 0.3–0.85s。

**3. Go 版的地板现在就是"启动一个原生进程"。** 用 `cmd.exe /c exit` 量出**从 Git Bash
启动任何原生 exe 本身**要 0.0963s，而空租约库下 `wulease.exe status` 是 0.0948s——
也就是程序自己的活儿已经落在噪声里。换个原生父进程（PowerShell）启动同一个
`wulease.exe status`，只要 **0.0301s**。

| 量的是什么 | 秒 |
| --- | ---: |
| `cmd.exe /c exit`（纯启动，从 Git Bash） | 0.0963 |
| `wulease.exe status`，空库（从 Git Bash） | 0.0948 |
| `wulease.exe status`，空库（从 PowerShell） | 0.0301 |
| `wulease.exe status`，200 个租约（从 PowerShell） | 0.2791 |

所以 Go 剩下的 0.17–0.24s 里，约 0.10s 是"从 shell 启动进程"，约 1ms/租约是读文件
（这台机器上有常驻的安全软件，每次打开文件都要过一遍）。

**4. `--json` 对 Go 几乎免费**（0.1895 → 0.1996），对 bash 是 0.3–0.85s。

**5. 数字会随机器状态浮动。** 同一套测试在机器别处忙着的时候，bash 侧曾测到
`status` 0.59 / `claim` 1.27。所以**倍率要按同一轮、紧挨着测的一对一对看**，不要跨轮
比较绝对值。

### 与"20 个租约 claim 约 1.3 秒"的差异

本轮 9 次的中位数是 **1.5747s**（带 `--json` 1.9142s），仍高于记录里的 1.3s；更早一轮
5 次测到 1.6065s，两轮彼此吻合。没有找到能归因的代码路径，所以只报事实、不猜原因：
可能是测量方式不同（是否计入 shell 启动、是否带 `--json`），也可能是机器状态不同。

## 行为差异清单

### 唯一一处真实差异：仓库怎么定位

| | 定位方式 |
| --- | --- |
| bash | `cd "$(dirname "$0")/.."` —— 先回到**脚本所在仓库**的根 |
| Go | 用**当前工作目录**，从那里向上找 `.git` |

理由：Go 二进制是构建产物（共享 git 目录里的 `amp-bin/`，或 `D:\Amp\builds\`），跟
仓库没有固定位置关系，`dirname $0/..` 对它没有意义；按 cwd 定位也是 git 工具的常规做法。

影响面：只在"从 A 仓库里调用 B 仓库的 `script/lease`"时不同——bash 会作用在 B，
Go 会作用在 A（A 不是仓库则报错退出）。所有正常用法（在仓库根调用、钩子以 worktree
根为 cwd 调用）两边一致。

#### 注意事项：脚本按自身位置定位仓库，会把租约写进那个仓库

这不是理论差别，本轮踩到过：写一次性验证脚本时直接调了
`D:/wuIDE-golease/script/lease`，它按脚本位置回到了 `feat/lease-go` 所在的仓库，
把 2 条租约写进了**真实主检出的共享** `.git/amp-leases`（已逐条删净，只删了自己造的
两条，删除前该目录只有这两条）。

所以：要用某个实现的租约，先 `cd` 到那个仓库再调用，或者用编译版（它按 cwd 走）。
`script/lease-test` 不受影响，因为它在临时 worktree 里放的是一份 bash 版的**副本**，
`$0` 因此指向里面。

### Go 版不再调用 git

`git rev-parse --git-common-dir` 曾经占掉每次运行 0.21s，比 Go 程序自己干的活还多。
现在 Go 版直接读仓库布局：

- 从当前目录向上找第一个 `.git`；
- 它是**目录** → 那就是 git 目录；
- 它是**文件**（链接工作树）→ 读 `gitdir: <path>`，再读 `<path>/commondir`
  （相对路径拼在 `<path>` 上）得到真正共享的 git 目录；
- 一路找不到 → 报错退出。

Bash 版没有跟着改：它的固定开销主要花在 bash 启动上，这一项对它是小头，改了也得付
同样的启动成本。

**对齐验证**（5 种布局 × 连跑 3 次，全部通过）：普通仓库的根、普通仓库里往下三层、
链接工作树的根、链接工作树里往下三层、`.git` 文件指向独立 git 目录；另外确认在
**任何仓库之外**会报错退出，而不是默默建一个空租约库。每种布局都验证了四件事：
bash 写的租约落在 `git rev-parse --git-common-dir` 指的位置、Go 写的落在同一位置、
两边互相看得见、Go 拒绝 bash 持有的前缀。

**已知差距**（都不影响本仓库的用法）：

- 不读 `GIT_DIR` / `GIT_COMMON_DIR`。实测 git 给 pre-commit 钩子**不导出** `GIT_DIR`
  （只有 `GIT_INDEX_FILE`），所以钩子路径不受影响；手工 `GIT_DIR=x ...` 调用时两边行为
  会不同。
- 不处理 bare 仓库（没有 `.git` 条目，会报"不在仓库里"）。
- `core.worktree` 等把工作树挪走的配置不参与。

### 环境造成的差异（不是代码差异）

从 Git Bash 调用原生 exe 时，MSYS 会把以 `/` 开头的参数改写成 Windows 路径，
例如 `/x/y.rs` 变成 `X:/y.rs`，然后才交给 `wulease.exe`。所以同一组输入在 Git Bash
下对比会出现假 DIFF。从 cmd/PowerShell 直接调用时两边完全一致：

| 输入 | 两边输出 |
| --- | --- |
| `/x/y.rs` | `/x/` |
| `//a` | `//` |
| `a//b.rs` | `a//` |
| `D:/x/y.rs` | `D:/x/` |

租约用的是仓库相对路径，不受影响。

### 已实测为等价的部分

- **归一化**：17 个输入逐字节一致，含退化形态——`a.rs → a.rs/`、`docs/x.md → docs/`、
  `a/b/c/d.rs → a/b/c/`、`a/b*/c → a/`、`**/*.rs → ./`、`.` 与 `./` → `./`、
  `src\win\file.rs → src/win/`、`a//b.rs → a//`、`//a → //`、`a[b].rs → ./`、
  `a?b/c → ./`、空串 → `./`。
  注意 `a.rs → a.rs/` 而不是 `./`：单段普通路径去掉"文件名"后没有父目录可留，
  于是保留自己。这与 `docs/PARALLEL-DEV.md` 的表格一致，也解释了为什么同目录
  两个文件会互相冲突（有意放宽、只误报不漏报）。
- **租约文件**：键序、键数、值形态一致；双向可见（bash 写 → Go 读得到，
  Go 写 → bash 读得到）；交叉冲突双向生效；`check --paths` 也生效。
- **`--ttl`**：14 个取值（`30s 2m 3h 1d 45 90m 25h 3d 0s 1s 59s 60m 24h 86400`）
  的 `expires_at - created_at` 完全一致。
- **humanize**：`30s` `2m` `1h30m` `1d01h` `3d00h` 一致。
- **退出码**：claim 冲突 1、`check` 冲突 1、`check --build` 持锁 1、各种 `die` 1，
  无冲突 0。pre-commit 依赖的正是这套语义。
- **文本**：`help` 全文、全部错误消息、空库输出（`No active leases.` /
  `{"leases":[]}`）一致。
- **锁语义**：`mkdir` 原子锁、100 次 × 0.1s 超时、超过 2 分钟的锁视为遗弃并回收、
  `claim`/`build claim` 在锁内 `prune_expired`。
- **build 锁**：全局唯一，连自己的 build 锁也会挡住第二次 claim；不阻挡 work 租约。
- **仓库发现**：5 种布局（普通仓库根 / 往下三层 / 链接工作树根 / 往三层 / `.git` 文件指向
  独立 git 目录）都与 `git rev-parse --git-common-dir` 一致，仓库外会报错退出。

### 钩子里的 bash 到底是哪一个（本轮实测）

这条值得单独写清楚，因为最直觉的担心恰恰不成立：

```
hook runs under      : Git 自带的 bash
command -v bash      : /usr/bin/bash
bash --version       : GNU bash, version 5.3.15(2)-release (x86_64-pc-cygwin)
PATH head            : /ucrt64/libexec/git-core:/ucrt64/bin:/usr/bin
GIT_DIR              : <unset>      (只有 GIT_INDEX_FILE=.git/index)
cwd                  : 仓库根
```

也就是说：**钩子由 git 自己的 shell 跑，git 还会把自带的目录前置到 PATH**，所以钩子里
`#!/usr/bin/env bash` 解析到的是 Git 的 bash，不会是 WSL 的。风险不在钩子路径。

风险在**人手调用**：在 PowerShell / cmd 里执行 `bash script/lease ...`，`bash` 会解析到
`C:\Windows\system32\bash.exe`（WSL），脚本就在 Linux 的文件系统视图里跑，看不到
`D:\...` 这些路径。原生 exe 消掉的是这一类问题。

### 已知未验证

- 并发只在测试自带的 5 轮 × 2 线程下验证过，没做更高并发的压测。
- 只在 Windows + Git Bash 下跑过；代码本身不依赖平台，但 Linux/macOS 未验证。
- 未与真实 `release-fast` 构建联调（本轮按要求不跑 `cargo`）。
- `script/shellcheck-scripts` 没跑：本机和 WSL 里都没装 `shellcheck`。改动过的三个
  shell 文件只做了 `bash -n` 语法检查（通过）。CI 配置里没有 shellcheck 步骤，不阻塞。
- `setup-hooks` 只在一次性仓库里实测过：在主检出里跑它会写共享的 `core.hooksPath`，
  那会影响所有 worktree，属于"合并/发版窗口"才该做的事。

## 结论：值不值得把 `script/` 继续迁过去

**值得，但要按调用频率挑，不要整个 `script/` 一起迁。** 本轮已经把默认路径切到 Go，
回退到 bash 是一条明确的、会吭声的路径。

1. **收益是性能 + 少一层运行时依赖，但性能的大头不在语言上。** 5.7–9.6×，绝对值从
   1.0–2.2s 降到 0.17–0.24s。不过其中一半来自"不再调 git"，而"不再调 git"这件事
   对 bash 版同样可做——之所以先做在 Go 版上，是因为 Go 版已经是个可以安全重写的
   新东西，而改 bash 版的定位逻辑风险更大、收益更小（它还得付 bash 启动）。
2. **剩下的地板是"从 shell 启动一个原生进程"（约 0.10s），不是程序本身。** 从
   PowerShell 直接启动只要 0.030s。这意味着继续抠这个数字没有意义了，除非把调用方
   也换掉。
3. **正确性上，真正被消掉的是"人手调用"那类问题，不是钩子。** 钩子一直安全（见上），
   所以不要用"钩子会跑错 bash"来论证迁移——那会站不住。真正的理由是：`script/lease`
   要求调用方有一个能看见本仓库的 bash，而原生 exe 只要求能执行文件。
4. **代价要算清。** 626 行 bash → 869 行 Go 代码，代码量没减少；给一台本来只需要
   Rust + Git 的机器又引入一条 Go 工具链（且 `setup-hooks` 现在依赖它，缺了会降级
   而不是失败）；切换完成前要同时维护两套实现。判断标准应该是**调用频率**：`lease`
   （每次 commit、每个 agent 都会调）值得；`bundle-*`、`release`、`install-*` 这类
   一次性的不值得，bash 够用。
5. **降级是明确的。** `setup-hooks` 在 Go 缺失或构建失败时打印一行原因并继续，钩子
   在没有 `amp-bin/wulease.exe` 时回落到 `script/lease` 并打印一行提示。两条路都不会
   让提交悄悄失效。

## 附：一个与本轮无关的老 bug

`.githooks/pre-commit` 第 23 行用的是**未加引号**的 `<<EOF`，而第 34 行的
"A clean `` `git merge` `` does not run this hook at all." 里那对反引号没有转义，于是
bash 会**真的执行 `git merge`**，再把空输出填进那句话。实测：

```
$ cat >&2 <<EOF
finishes a conflicted merge. A clean `git merge` does not run this hook at all.
EOF
fake git was called with: [merge]
finishes a conflicted merge. A clean  does not run this hook at all.
```

后果：在主检出里手敲提交、且没设 `AMP_MAIN_COMMIT=1` 时，钩子会先跑一次 `git merge`。
若当前分支有 upstream 且落后，这一次 `merge` 会真的合进来。改成 `<<'EOF'` 即可。

这是 `f88814ea` 上就存在的问题，本轮没有改它。
