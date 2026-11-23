<p align="center">English | <a href="README.zh-CN.md">中文</a></p>

<div align="center">
  <img src="crates/wu/resources/app-icon.png" alt="IDE" width="128">
  <h1>IDE</h1>
  <p>A fast, native code editor with built-in coding agents.</p>
  <p>Written in Rust. Batteries included.</p>
  <p><a href="#download"><strong>Download</strong></a> · <a href="./FORK.md">What changed from upstream</a></p>
</div>

---

> **This is a personal fork.** It is built from [dstrml/wu](https://github.com/dstrml/wu), which is itself a fork of [Zed](https://github.com/zed-industries/zed). It has been modified: the product is renamed to **IDE**, the built-in agent list changed, and it ships its own bundled agent runtimes. See [FORK.md](./FORK.md) for the full list of changes. Releases here are built by this repository's CI from a tagged commit, and the corresponding source is this repository.

IDE is a fast, native code editor written in Rust, with a clean, modern design and coding agents built in. It is a fork of Zed, but diverges heavily from it.

## Features

- ⚡ **Native and fast.** No Electron or webviews. Opens instantly and stays responsive on large views.
- 🍃 **Lightweight.** A lower memory footprint than VS Code and Zed. See [benchmarks](./docs/memory-benchmark.md).
- 👾 **Coding agents included.** Built-in support for Claude Code, Codex, OpenCode, DeepSeek, and Pi, each switchable in settings.
- 🧠 **Agents start on demand.** Each agent process is spawned for a turn and released when the turn ends, instead of staying resident.
- ✨ **Beautiful by default.** Clean, modern look with a delightful UX.
- 🤫 **No account.** Nothing to sign in to, and no analytics or crash reporting is built in.

## Download

Releases are on this repository's [releases page](https://github.com/Fenghu146/wuIDE/releases/latest).

- [macOS (Apple Silicon)](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-aarch64.dmg)
- [macOS (Intel)](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.dmg)
- [Windows x86-64](https://github.com/Fenghu146/wuIDE/releases/latest/download/IDE-x86_64.exe)
- [Linux x86-64](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-x86_64.tar.gz)
- [Linux AArch64](https://github.com/Fenghu146/wuIDE/releases/latest/download/ide-linux-aarch64.tar.gz)

On macOS and Linux you can also run:

```sh
curl -fsSL https://raw.githubusercontent.com/Fenghu146/wuIDE/main/script/install.sh | sh
```

### The builds are not code-signed

There is no Apple Developer certificate and no Windows code-signing certificate behind these releases, so both platforms will warn you the first time:

- **macOS** blocks the app after download. Either open it once and then allow it under **System Settings → Privacy & Security → Open Anyway**, or clear the quarantine flag:

  ```sh
  xattr -d com.apple.quarantine /Applications/IDE.app
  ```

- **Windows** shows a SmartScreen warning. Click **More info**, then **Run anyway**.

Because the binaries are unsigned, **verify the SHA-256 of the file you downloaded** before running it. Every release asset carries a digest that GitHub reports on the release page; compare it locally:

```sh
shasum -a 256 IDE-aarch64.dmg        # macOS
sha256sum IDE-x86_64.exe             # Linux
```

See [docs/INSTALL.md](./docs/INSTALL.md) for the per-platform details.

## Updates

Automatic updates are **on by default** and point at this repository's own releases, not upstream's. A check runs once at startup and then every 12 hours. It only notifies: the title bar shows **Update Available**, and a notification card offers **Download and Install** or **Skip This Version**. Nothing is installed until you click.

Two things worth knowing:

- The download is verified against the SHA-256 digest GitHub publishes for the asset. That catches a corrupted transfer, but it does **not** prove who built the file. If you would rather not trust a downloaded update, turn `auto_update` off in settings and install releases by hand after checking the digest.
- This repository has to stay **public** for updates to work: the client fetches release metadata anonymously, and a private repository's releases return 404 without a token.

## Network access

IDE sends no analytics, no crash reports, and no usage telemetry. It does make these outbound requests, which you can audit in the source:

| What | Where | When |
| --- | --- | --- |
| Release metadata for updates | `api.github.com/repos/Fenghu146/wuIDE/releases` | at startup, then every 12 hours |
| Extension index and extension downloads | `api.zed.dev` (via the `server_url` setting, still `https://zed.dev`) | when you open the extensions view or a language needs an extension |
| Node runtime download | `nodejs.org`, only while packaging, never at runtime | during a build |
| Your coding agents' own API calls | whatever provider you configured | when you send a message to an agent |

If you want the editor to stop talking to the extension registry, set `"server_url"` in your settings to an endpoint you control, or leave the extensions view alone.

## Building

IDE builds the same way Zed does. See the [Zed development docs](https://zed.dev/docs/development), [FORK.md](./FORK.md) for the fork-specific packaging notes (bundled agent runtimes, icon and bundle scripts), and [SYNC.md](./SYNC.md) for how this fork tracks upstream.

## Licensing

IDE is distributed under **GPL-3.0-or-later**, with Apache-2.0 components where marked. See [LICENSE-GPL](./LICENSE-GPL) and [LICENSE-APACHE](./LICENSE-APACHE), and [NOTICE](./NOTICE) for the upstream attribution this fork carries.

Bundled third-party runtimes have their own licenses, shipped alongside them:

- Node.js — MIT, plus the licenses of its own dependencies
- `@brokkai/dsh-acp` — MIT
- `pi-acp` — MIT
- `@earendil-works/pi-coding-agent` — MIT

Because this is GPL software, anyone you give a build to is entitled to the corresponding source. That source is this repository at the release tag — so when sharing a build, share the tag it was built from too.

## Acknowledgements

Thanks to the [Zed](https://github.com/zed-industries/zed) team for building an excellent editor and releasing it as open source, and to [dstrml](https://github.com/dstrml/wu) for the memory-focused fork this one is based on. Neither project endorses this fork.
