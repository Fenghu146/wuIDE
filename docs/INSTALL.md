## Install

All releases are available [here](https://github.com/Fenghu146/wu/releases/latest).

None of these builds are code-signed. Check the SHA-256 of whatever you download against the digest GitHub lists for that release asset before you run it:

```sh
shasum -a 256 IDE-aarch64.dmg
```

### macOS (Apple Silicon and Intel)

1. Download the disk image for your Mac, open it, and drag IDE into your Applications folder.
    - [**Apple Silicon**](https://github.com/Fenghu146/wu/releases/latest/download/IDE-aarch64.dmg) (M1 and later)
    - [**Intel**](https://github.com/Fenghu146/wu/releases/latest/download/IDE-x86_64.dmg)

   The build is not signed with an Apple Developer certificate, so macOS will block it the first time you open it.
2. Open Terminal and run:

   ```sh
   xattr -d com.apple.quarantine /Applications/IDE.app
   ```

> If you'd rather not use Terminal: open IDE once (you'll see an "IDE can't be opened" or "Apple could not verify" message), then go to **System Settings → Privacy & Security**, scroll down, and click **Open Anyway** next to IDE. Confirm with your password.

3. Open IDE normally.

#### Command line

To open files and folders from Terminal with `ide`, choose **IDE → Install CLI** from the menu bar. It adds `ide` to `/usr/local/bin` and may ask for your password.

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
curl -fsSL https://raw.githubusercontent.com/Fenghu146/wu/main/script/install.sh | sh
```

It installs IDE into `~/.local`, adds the `ide` command, and adds IDE to your app menu with its icon. Run it again any time to reinstall. IDE keeps itself up to date after that, as long as this repository stays public.

The script reads `WU_VERSION=1.2.3` to install a specific release and `WU_BUNDLE_PATH` to install a local tarball instead of downloading one.

#### Manual install

1. Download the tarball for your platform.
    - [**Linux x86-64**](https://github.com/Fenghu146/wu/releases/latest/download/ide-linux-x86_64.tar.gz)
    - [**Linux AArch64**](https://github.com/Fenghu146/wu/releases/latest/download/ide-linux-aarch64.tar.gz)

2. Unpack its contents into `~/.local`:

```sh
tar -xzf ide-linux-$(uname -m).tar.gz -C ~/.local
ln -sf ~/.local/ide.app/bin/ide ~/.local/bin/ide
```

Make sure `~/.local/bin` is on your `PATH`, then run `ide`. This doesn't add IDE to your app menu; the install command above does.

#### WSL2

On WSL2, IDE draws with the CPU by default because the WSL GPU driver isn't fully standards-compliant. To try the GPU instead, start IDE with:

```sh
WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1 ide
```

### Windows (x86-64)

Download the [installer](https://github.com/Fenghu146/wu/releases/latest/download/IDE-x86_64.exe) and click to run. The installer isn't code-signed, so Windows SmartScreen may warn you. Click **More info**, then **Run anyway**.

The installer registers the app as IDE but keeps the identifier the earlier Wu builds used, so upgrading in place works and your settings carry over.

### Uninstalling

`script/uninstall.sh` in this repository removes the app, the `ide` and `wu` command shims, and — after asking — your preferences. It handles both the current IDE install and older Wu installs.

## Bundled agent runtimes

IDE ships the runtimes its built-in agents need, so none of them require Node or a global npm install:

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

Pi's isolated home is seeded from `~/.pi/agent` on every launch: `auth.json`, `models.json`, `models-store.json` and a `settings.json` with globally installed `packages` and `enabledModels` removed. That means a provider switch written by cc-switch reaches the bundled Pi without you having to clear anything, but it also means a model you pick inside IDE is replaced by your global Pi default the next time an agent starts.
