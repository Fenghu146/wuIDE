# `script/lease` 的 Go 试点

用 Go 重写 `script/lease`，作为"外围工具换语言"的第一个试点。目标不是替换，而是
**同时存在、互相看得见、用同一套测试对照**，再决定要不要切换。

- 分支：`feat/lease-go`
- 基线：`origin/main` @ `f88814ea`
- 二进制**不进仓库**，产物在仓库外（见"构建"）
- `.githooks/pre-commit` 与 `script/pre-merge-check` **未改动**，仍调用 bash 版

## 构建与运行

```sh
# 构建（产物在仓库外）
go build -C script/lease-go -o D:/Amp/builds/wulease.exe .

# 对照测试：必须显式用 Git Bash
"C:/Program Files/Git/bin/bash.exe" script/lease-test --compare D:/Amp/builds/wulease.exe

# 单跑一边（默认就是 bash 版，行为与改造前一致）
script/lease-test
LEASE_BIN=D:/Amp/builds/wulease.exe script/lease-test
```

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
| `script/lease-go/main.go` | 967（空行 89、注释 54，代码 824） | Go 实现，仅用标准库 |
| `script/lease-go/go.mod` | 2 | `module wulease`, `go 1.22` |
| `script/lease-test` | 390（非空 323，原 183） | 改为参数化 runner，默认行为不变 |

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

第 22–26 项走的是 `.githooks/pre-commit`，而钩子调的是 `$toplevel/script/lease`。
runner 在被测实现是二进制时，会在那个临时 worktree 里放一个
`exec` shim，所以钩子也被导向被测实现——否则这 5 项永远只测 bash 版。

## 性能对照

同一台机器（4 核），同一批租约（直接按上面的格式铺设 20 / 50 个 `work` 租约，
`expires_at` 在 2 小时后），每次先跑一遍预热，再取 5 次的中位数，单位秒。

| 租约数 | 操作 | bash | Go | 倍率 |
| ---: | --- | ---: | ---: | ---: |
| 20 | `status` | 1.0026 | 0.3491 | 2.9× |
| 20 | `claim` | 1.6065 | 0.3840 | 4.2× |
| 20 | `claim --json` | 2.2593 | 0.3668 | 6.2× |
| 50 | `status` | 0.9890 | 0.3716 | 2.7× |
| 50 | `claim` | 1.5930 | 0.3939 | 4.0× |
| 50 | `claim --json` | 2.2467 | 0.4102 | 5.5× |

### 这些数字说明什么

**1. bash 版的耗时与租约数量无关。** 20 → 50 个租约，`status` 1.0026 → 0.9890，
`claim` 1.6065 → 1.5930。也就是说当初 9.8s → 1.3s 那轮优化已经把"每租约成本"
消干净了，现在剩下的全是固定开销。靠加租约数压不垮它，靠减也救不了它。

**2. 剩下的固定开销是 fork 和进程启动。** 单项拆解（同机、5 次中位数）：

| 单项 | 秒 |
| --- | ---: |
| Git Bash 启动（`bash -c ':'`） | 0.2250 |
| `git rev-parse --git-common-dir` | 0.2100 |
| `bash -c 'date +%s'`（一次 fork） | 0.2869 |
| `wulease.exe status`（空租约库） | 0.3013 |

一次 fork 约 0.06s，而 bash 版 `status` 至少有 `git`、`date`、`mkdir` 三处子进程，
`claim` 还要多一处 `$(normalize ...)`；`--json` 更进一步，每个字段一次
`$(json_string ...)`，所以比 `claim` 多出约 0.65s。

**3. Go 版的地板是它自己那次 `git` 子进程。** 空租约库下 `wulease.exe status` 是
0.3013s，其中 `git rev-parse` 占 0.2100s。也就是说 Go 版 0.35–0.41s 的耗时里，
**六成是那一次 `git` 调用，与语言无关**。

**4. `--json` 对 Go 几乎免费**（0.3840 → 0.3668，在噪声内），对 bash 是 0.65s。

### 与"20 个租约 claim 约 1.3 秒"的差异

本次实测 20 个租约时 bash `claim` 中位数 **1.6065s**，带 `--json` 是 **2.2593s**，
都高于记录里的 1.3s。没有找到能归因的代码路径，所以只报事实、不猜原因：可能是
当初测量的机器状态不同（本轮跑时机器空闲、无 `cargo`/`rustc` 在跑），也可能是
测量方式不同（例如是否计入 shell 启动、是否带 `--json`）。**两边的相对差距不受
这一点影响**，因为每一对都是在同一轮、同一台机器上紧挨着测的。

## 行为差异清单

### 唯一一处真实差异：仓库怎么定位

| | 定位方式 |
| --- | --- |
| bash | `cd "$(dirname "$0")/.."` —— 先回到**脚本所在仓库**的根 |
| Go | 用**当前工作目录**，在那里调 `git rev-parse --git-common-dir` |

理由：Go 二进制是构建产物（`target/` 或 `D:\Amp\builds\`），跟仓库没有固定位置
关系，`dirname $0/..` 对它没有意义；而按 cwd 定位是 git 工具的常规做法。

影响面：只在"从 A 仓库里调用 B 仓库的 `script/lease`"时不同——bash 会作用在 B，
Go 会作用在 A（A 不是仓库则报错退出）。所有正常用法（在仓库根调用、pre-commit
以 worktree 根为 cwd 调用）两边一致。

> 这个差异有实际后果，本轮踩到过：写一次性验证脚本时直接调了
> `D:/wuIDE-golease/script/lease`，它按脚本位置回到了 `feat/lease-go` 所在仓库，
> 把两条租约写进了真实主检出的共享 `.git/amp-leases`（已逐条删净，只删了自己造的
> 两条，删除前该目录只有这两条）。`script/lease-test` 之所以不受影响，是因为它会
> 把 bash 版**复制**进临时 worktree，`$0` 因此在里面。

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

### 已知未验证

- 并发只在测试自带的 5 轮 × 2 线程下验证过，没做更高并发的压测。
- 只在 Windows + Git Bash 下跑过；代码本身不依赖平台，但 Linux/macOS 未验证。
- 未与真实 `release-fast` 构建联调（本轮按要求不跑 `cargo`）。
- `script/shellcheck-scripts` 没跑：本机和 WSL 里都没装 `shellcheck`。`script/lease-test`
  只做了 `bash -n` 语法检查（通过）。CI 配置里没有 shellcheck 步骤，所以不阻塞。

## 结论：值不值得把 `script/` 继续迁过去

**值得继续，但不该按"整个 `script/` 一起迁"的方式推；只对高频调用的脚本值得。**

1. **真正的收益不是"每租约更快"，而是去掉对 bash 的运行时依赖。** 这台机器 PATH
   上的 `bash` 是 WSL 的（5.2.21，Linux），Git Bash 在另一个路径。`.githooks/pre-commit`
   依赖 `"$lease"` 的 shebang 能解析到 Git Bash；一旦解析到 WSL，钩子就在另一个
   文件系统视图里跑，**静默失效**——不报错，只是再也拦不住东西。原生 exe 消掉
   这一整类问题，这个价值比省 1 秒大。
2. **性能收益真实但不均匀。** 2.7–6.2×，绝对值从 1.0–2.3s 降到 0.35–0.41s，对每次
   commit 都会跑的 pre-commit 有意义；但它**不会**随租约数量增长而放大（见上，
   bash 侧本来就是平的）。
3. **下一步更该做的是"别调 git"，而不是"换语言"。** 两个实现的大头都是那次
   `git rev-parse`（bash 1.0s 里有 0.21s 是它，Go 0.30s 里有 0.21s 也是它）。直接读
   `.git`（或 `.git` 文件里的 `gitdir:` / `commondir`）能把这 0.21s 也去掉，做到
   ~10–20ms，而这件事**跟语言无关**。如果目标是"让 claim/status 快到无感"，先做
   这个，性价比比换语言高。
4. **代价要算清。** 626 行 bash → 967 行 Go（824 行非空非注释），代码量没减少；
   给一台本来只需要 Rust + Git 的机器又引入一条 Go 工具链；切换完成前要同时维护
   两套实现。所以判断标准应该是**调用频率**：`lease`（每次 commit、每个 agent 都会
   调）值得；`bundle-*`、`release`、`install-*` 这类一次性的不值得，bash 够用。
5. **如果决定切换，前提是这次的双列对照能长期跑。** 建议：把 `--compare` 加进 CI，
   保留 bash 版一个版本作为回退；`pre-commit` / `pre-merge-check` 改成调
   `wulease.exe` 时，要么让 CI 负责构建产物，要么在 `script/` 里加一个
   "构建到 targets 目录并回退到 bash" 的薄封装。

本轮不切换实现，也不改任何调用方。
