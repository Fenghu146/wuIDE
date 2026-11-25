## Install

All releases are available [here](https://github.com/Fenghu146/wuIDE/releases/latest).

None of these builds are code-signed. Check the SHA-256 of whatever you download against the digest GitHub lists for that release asset before you run it:

```sh
shasum -a 256 IDE-aarch64.dmg
```

### macOS (Apple Silicon and Intel)

1. Download the disk image for your Mac, open it, and drag WuIDE into your Applications folder.
    - [**Apple Silicon**](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-aarch64.dmg) (M1 and later)
    - [**Intel**](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.dmg)

   The build is not signed with an Apple Developer certificate, so macOS will block it the first time you open it.
2. Open Terminal and run:

   ```sh
   xattr -d com.apple.quarantine /Applications/IDE.app
   ```

> If you'd rather not use Terminal: open WuIDE once (you'll see a "WuIDE can't be opened" or "Apple could not verify" message), then go to **System Settings → Privacy & Security**, scroll down, and click **Open Anyway** next to WuIDE. Confirm with your password.

3. Open WuIDE normally.

#### Command line

To open files and folders from Terminal with `ide`, choose **WuIDE → Install CLI** from the menu bar. It adds `ide` to `/usr/local/bin` and may ask for your password.

If that fails, for example because your account isn't an admin, add it yourself without a password:

```sh
mkdir -p ~/.local/bin
ln -sf /Applications/IDE.app/Contents/MacOS/cli ~/.local/bin/ide
```

If `ide` still isn't found, add `~/.local/bin` to your `PATH` by adding this line to `~/.zshrc`, then open a new Terminal window:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Linux (x86-64 and AArch64)

Run this in a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/Fenghu146/wuIDE/main/script/install.sh | sh
```

It installs WuIDE into `~/.local`, adds the `ide` command, and adds WuIDE to your app menu with its icon. Run it again any time to reinstall. WuIDE keeps itself up to date after that, as long as this repository stays public.

The script reads `WU_VERSION=1.2.3` to install a specific release and `WU_BUNDLE_PATH` to install a local tarball instead of downloading one.

#### Manual install

1. Download the tarball for your platform.
    - [**Linux x86-64**](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-x86_64.tar.gz)
    - [**Linux AArch64**](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-aarch64.tar.gz)

2. Unpack its contents into `~/.local`:

```sh
tar -xzf ide-linux-$(uname -m).tar.gz -C ~/.local
ln -sf ~/.local/ide.app/bin/ide ~/.local/bin/ide
```

Make sure `~/.local/bin` is on your `PATH`, then run `ide`. This doesn't add WuIDE to your app menu; the install command above does.

#### WSL2

On WSL2, WuIDE draws with the CPU by default because the WSL GPU driver isn't fully standards-compliant. To try the GPU instead, start WuIDE with:

```sh
WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1 ide
```

### Windows (x86-64)

Download the [installer](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.exe) and click to run. The installer isn't code-signed, so Windows SmartScreen may warn you. Click **More info**, then **Run anyway**.

The installer registers the app as WuIDE but keeps the identifier the earlier Wu builds used, so upgrading in place works and your settings carry over.

### Uninstalling

`script/uninstall.sh` in this repository removes the app, the `ide` and `wu` command shims, and — after asking — your preferences. It handles both the current WuIDE install and older Wu installs.

## Bundled agent runtimes

WuIDE ships the runtimes its built-in agents need, so none of them require Node or a global npm install:

| Agent | Runtime | Data directory |
| --- | --- | --- |
| Claude Code, Codex, OpenCode | the CLI already on your machine | your usual global config |
| DeepSeek | bundled native `dsh-acp` | `Wu/agents/deepseek` under your user data directory |
| Pi | bundled Node + `pi-acp` + `pi-coding-agent` | `Wu/agents/pi-home` under your user data directory |

The per-user data directories are, by platform:

- macOS — `~/Library/Application Support/WuIDE/agents`
- Windows — `%LOCALAPPDATA%\WuIDE\agents`
- Linux — `$XDG_DATA_HOME/wuide/agents`, falling back to `~/.local/share/wuide/agents`

Set `WU_AGENT_DATA_DIR` to put them somewhere else.

The runtimes are found beside the executable: `agents` next to `IDE.exe` on Windows, `libexec/agents` in the Linux bundle, and `Contents/Resources/agents` inside the macOS app. If you copy just the executable out of the bundle — for example to try a single-file build — copy the `agents` folder next to it as well, or set `WU_AGENT_BIN_DIR` to its path. A local build can use `sync-agents.ps1`, kept next to the build output, which mirrors `crates/wu/resources/agents` into that folder and does nothing when the copy is already current. Otherwise DeepSeek and Pi report that they are unavailable and list the folders they searched.

Pi's isolated home is seeded from `~/.pi/agent` on every launch: `auth.json`, `models.json`, `models-store.json` and a `settings.json` with globally installed `packages` and `enabledModels` removed. That means a provider switch written by cc-switch reaches the bundled Pi without you having to clear anything, but it also means a model you pick inside WuIDE is replaced by your global Pi default the next time an agent starts.

## Program output in the terminal

The terminal renders UTF-8, and a Windows console starts on the system code page — 936 on a Chinese system — so every Windows step, build or run, sets `[Console]::OutputEncoding` to UTF-8 before it runs its own command. Assigning that property moves the console output code page to 65001, and whatever the step starts inherits it: a program writes UTF-8, and a compiler that localizes its messages writes them as UTF-8 too. That is how MSVC's Chinese errors reach the terminal as `语法错误: 缺少“;”` instead of GBK bytes it cannot decode. For a UTF-8 program the same setting is what stops `张三` from arriving as GBK mojibake (`寮犱笁`). Switching the code page with `chcp 65001` alone is not enough for the program whose output PowerShell decodes, because .NET takes the code page it started with; the property is what lands in time.

That leaves one thing the editor cannot decide for you: a program that writes something other than UTF-8.

- **g++, Cargo, Python and Node** write UTF-8 and need nothing.
- **MSVC with GBK source files** prints garbled Chinese, because its string literals and its output are GBK bytes. Compile with `/utf-8` so the program speaks UTF-8 too.
- **A program that has to write GBK** — to a legacy consumer, say — is not supported in this terminal. Nothing here detects an encoding from the bytes, because guessing would corrupt the UTF-8 output that works today; write to a file or run that program in another terminal.
