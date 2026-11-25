# Changelog

All notable user-facing changes to Wu are listed here, newest first.

## Unreleased

## 1.5.0 - 2026-11-26

- `script/lease` is a Go program now: claiming is 8–9× faster and no longer needs bash, which on Windows meant WSL and a path it could not parse. It also fixes a silent widening — `claim crates/diagnostics` without a trailing slash used to claim all of `crates/`, because the path was never stat'ed.
- The pre-commit hook's refusal message quoted `git merge`, and an unquoted heredoc ran it: every refused commit executed `git merge` on a stale `main`. The text is escaped, and `script/hook-test` fails when a hook runs a git subcommand it does not need.
- The repository-map MCP server is type-checked (`@ts-check` + JSDoc, `strict`). A renamed snapshot field now fails the pre-merge check instead of rendering a page that silently drops it; `script/check-mcp-types` only warns, and prints `skipped: tsc unavailable` when the compiler cannot be fetched.
- License generation runs under a watchdog (1200s, `LICENSES_TIMEOUT_SECONDS` overrides) and the crates are fetched as their own visible step. 1.4.7's macOS aarch64 job sat on `cargo about` for five hours until the job timed out; it now fails in twenty minutes, naming the step.
- A What's New page shows release notes inside the app in the interface language. Chinese notes live in `CHANGELOG.zh.md`: a version with no Chinese section falls back to English and says so, and a test keeps both files from drifting on version, date and entry count.
- Chinese error messages from `cl.exe` are written as UTF-8, so the terminal shows 语法错误 instead of mojibake. The console output encoding is set once, in the same prefix used by build, run and debug.
- Run, build and debug labels, the terminal task summary, agent turn errors and relative timestamps now follow the interface language — 3,768 Chinese strings in total. English output is unchanged.
- A diagnostic can be translated in place: click 译成中文, the message turns Chinese where it stands, and 原文 / 译文 switches back. It reuses the one-shot model channel behind commit messages, so no chat session is opened. Off by default (`diagnostics.translate_with_model`); a failed translation shows one line where the message was.
- Compile is its own button (`ctrl-shift-b`, `cmd-shift-b` on macOS), and Run builds out of sight: a failed build brings its terminal back, a successful one keeps the build terminal and opens the program. `code_runner.enabled` hides the run, debug and compile entries while keeping the configured maps.
- The release workflow builds the remote server in its own matrix job, installs cargo-about once, and folds the editor and CLI into one cargo invocation. Linux binaries get ICF and an rpath; macOS hides Rust exports. The release gate covers every job that uploads an asset, `workflow_dispatch` can run the pipeline without publishing, and a stale numbered `/Volumes/IDE 1` mount is detached before the DMG is built.

## 1.4.7 - 2026-11-24

- **Stop** no longer ends the whole application. Clicking it used to abort the process and take any unsaved work with it: the terminal panel read the workspace and the editor while they were already being updated, and a panic inside a window callback cannot unwind on Windows, so the process died. Both sites now pass down what they already have.
- Program output in the terminal is decoded as UTF-8 on Windows, so a program printing Chinese no longer shows `鐮庡簥 30`. Programs that print GBK — an MSVC build without `/utf-8` — are the known exception, and the setting's description and `docs/INSTALL.md` say so.
- **Compile** is its own button now (`Ctrl-Shift-B`), and **Run** opens a single terminal: the build happens out of sight and only appears when it fails. `code_runner.enabled` hides the run, debug and compile entries without discarding `executor_map`.
- A `$` or a backtick in a file name is no longer expanded by the shell, so `a$b.py` opens instead of running `a.py`. Rust runs the binary for the current file (`cargo run --bin`) or compiles a lone `.rs` with `rustc`; Go and Java have default commands; a `.tsx` file says it needs a transform instead of failing at run time.
- The repository map has three modes and defaults to **Basic**: it builds when you open its panel or ask an agent for it, and never adds a digest to a chat. **Off** builds nothing, and **Full** restores the old behaviour of building a project on open.
- Chats can be archived, grouped by project, and renamed. Once more than 30 unarchived chats exist, the ones untouched for 30 days are archived automatically, with an undo line under the list. Titles come from the first message; an optional smart title asks your configured model for one and never replaces a name you typed.
- The repository map settings had no Chinese descriptions while the settings page showed them, which the release's translation gate would have caught.

## 1.4.6 - 2026-11-24

- Run, Debug and Stop are now separate buttons in the editor's top-right toolbar, in the **Run** menu and in the status bar, instead of sitting on the file tab; `F5` debugs, `Ctrl-F5` or `Ctrl-Alt-N` runs the current file, and `Ctrl-Alt-M` stops it.
- Fix Run doing nothing at all: the task was scheduled through a window lookup that can never succeed from inside the window's own update, and the error was dropped; every run and debug entry point now uses the window the action already has.
- C and C++ files without `main` compile their whole directory into a binary named after the directory, and a directory where nothing defines `main` says so instead of handing you a linker error; a CMake project configures, builds and then runs the binary the build produced.
- Build and run are two separate executions now, so the step that fails carries its own exit code and a `file:line:col` you can click; a missing `gdb` greys out Debug and explains itself in its tooltip instead of covering the editor with a toast.
- The product is **WuIDE** wherever you can see it: window title, About, menus, notifications, the installer and uninstaller display names, the macOS bundle name and the Linux `.desktop` entry.
- A new app icon ships as a Windows `.ico` with seven sizes, is generated when macOS is packaged, and matches the Linux and website artwork, with a simplified drawing for 16–32 px; the agent's thinking animation and the component preview draw the same figure as one monochrome silhouette.
- The Windows installer drops from 84.19 MiB to 69.71 MiB (−17.2%), and an install from 430.2 MiB to 347.8 MiB by unpacking 5,804 files instead of 16,214.
- The installer now ships one `dsh-acp` per architecture, leaves type declarations, docs and source maps out of `node_modules`, compresses with `lzma2/ultra64`, and hard-links the CLI to the main binary so both share one copy.
- **Keep Session Snapshots** in settings showed as off while snapshots were still being taken, because the default settings file had no entry for it.
- DeepSeek no longer borrows OpenCode's icon, and Help → Documentation opens this repository's docs directory instead of the upstream site.

## 1.4.5 - 2026-11-23

- Repository map: an edge now needs an API another file can name in the same language, so test modules, the icon enum and the `settings_content` files no longer crowd the top of the map.
- Repository map: `in` and `out` now count only distinctive mentions — a name written by more than twenty files is vocabulary and makes no edge, the rest weigh by how rare the name is, and only an API another file can write (not a field, an enum variant, a local or a JSON key) can receive references. A new `deps N` counts certain dependencies instead: the files whose `mod`, `use`, `#include` or relative `import` resolves to this one.
- Repository map: files up to 1 MiB are parsed instead of 256 KiB, which brings `editor.rs` and `workspace.rs` into the map; the size-and-mtime cache keeps a cold map of a large repository at about twenty seconds, an already-built map at 0.02 s, and a map after one edit at about a second.
- Diagnostics: fix the summary freezing while updates arrive in a burst — every event used to replace the pending refresh timer, so the count never caught up; several problems at one position now share a row with a `×N` badge.
- Diagnostics: the `diagnostics` tool says how old its snapshot is and asks for a refresh past five minutes; the Problems panel gives every row a checkbox and an **Add Selected to Chat (N)** action, and ticks now survive a click made with no chat open.
- Diagnostics: the **Add to Chat** button in an editor's inline diagnostic block renders again, since the block finally knows its file path, and a repeat click answers **Already added to the chat** instead of doing nothing; a batch after a single adds only the problems that have no chip yet.
- Add **Run** and **Debug** to an editor tab, on `Ctrl-F5` and `Ctrl-Shift-F5`: C, C++, Rust, Python and Node/TypeScript work out of the box, any other extension can be given a command under `code_runner.executor_map`, and a C or C++ file in a CMake project builds the whole project instead of one file.
- The command about to run stays visible in the terminal and can be edited for that file; a debug build writes its own `*.dbg` binary beside the run one, and a missing `gdb` is reported in a toast instead of failing inside the session.
- Add session snapshots: when an agent starts and finishes a run, the worktree's uncommitted files are stored zstd-compressed in the local SQLite database, so a working tree with no commit behind it can be put back; files over 2 MB and binary files are listed with the reason they were skipped.
- **Session Snapshots** in the project panel lists the copies by chat and restores one file or a whole snapshot, warns first when an open buffer has unsaved changes, and keeps 64 MB and 7 days, dropping the oldest.
- Check for updates by default again: the release metadata is public now, so installs notice new releases without being asked, and a channel that never polls no longer shows a **Check for Updates** item that does nothing.
- A Windows update now replaces the bundled agent runtimes too, so Pi and DeepSeek stop running the previous release's Node and `dsh-acp`; the release request gives up after thirty seconds instead of leaving later checks silent, and the update check and its links use `wuIDE`.

## 1.4.4 - 2026-11-23

- Repository map: vendored and generated directories such as `node_modules`, `vendor` and `dist` are excluded, so a Rust monorepo no longer comes back as third-party JavaScript.
- Repository map: files are bucketed by their first two path components and the byte budget is shared between the buckets, so one directory cannot crowd out the rest and selection no longer depends on the order the caller supplies.
- Repository map: up to 3000 files are indexed instead of 900, with a 64 MiB parse budget, and the header states how many candidate files were skipped.
- Repository map: a per-file cache keyed by size and modification time makes reopening the panel almost instant, about a second on an unchanged repository instead of twenty.
- Repository map: the list of omitted files names at most 20 paths instead of up to 100.
- Tests: a cross-platform Rust stand-in replaces the POSIX `fake-claude.sh`, so the chat and Claude harness tests run on Windows; nineteen tests that only ran on Unix now run everywhere.
- Edit predictions: a twenty-second total timeout, a warning that names only the endpoint host, and a one-to-thirty-second retry backoff, instead of failing silently.
- A chat opened for a selection inherits the agent and model but no longer the approval mode.
- Agent data now lives in one directory: `--user-data-dir` moves the Pi home, the repository map snapshots and the diagnostics snapshots together.
- Fix the built-in Pi agent being reported as missing when Wu runs from a copied executable: the search includes the session's project, remembers the first successful resolution, and names the folders it checked.

## 1.4.3 - 2026-11-21

- Fix the Windows test suite: 17 tests that drove a POSIX shell harness are skipped there instead of failing, the loopback proxy test no longer depends on the registry, and the code lens test derives its expected set from the viewport.
- Repository map: symbols get readable kinds, function parameters and nested declarators are gone, local variables are hidden by default, and each file carries its include fan-in and fan-out.
- Repository map paging: `limit`, `offset` and `files_only` behave as documented, each file block keeps at most 40 symbols, and a whole page stays inside its character budget including the list of omitted files.
- Add a `diagnostics` MCP tool, so the built-in Pi agent reads the project's diagnostics itself instead of being handed pasted text.
- Add **Add Selection to Chat** as a compact reference: the composer keeps a `path Lx-Ly` chip and the agent receives a pointer instead of up to 8000 characters of code.
- Add **Add Diagnostic to Chat** and **Add All Diagnostics to Chat**: diagnostics become chips too, and a batch larger than 20 points the agent at the `diagnostics` tool.
- Fix the built-in Pi agent being reported as missing in a local build: the runtime is still resolved from the bundled app first, with the checkout as a last resort, and a missing file is named in the error.
- Add a first-turn repository map digest, about 700 tokens for a 99-file project and seventeen times smaller than the full map.
- Add the **project digest** chip to a new chat: it carries that digest into the first message, stays visible in the transcript, is on by default and can be switched off in settings.

## 1.4.2 - 2026-11-20

- Repository map: symbols now carry a readable kind (function, method, class, struct and so on) instead of a raw syntax node name, function parameters and nested declarators are no longer listed, each file's top-level symbols come first, and duplicates are dropped.
- Add **Copy Symbols as JSON** to the repo map: `ctrl-alt-m` opens the panel and `ctrl-alt-shift-m` copies its symbols as JSON (`ctrl-cmd-m` and `ctrl-cmd-shift-m` on macOS).
- Give the built-in Pi its project conventions in the system prompt on every start, and let it query the repository map on demand through the `repo_map` and `symbols` MCP tools. Snapshots are written to the IDE data directory, never into the project.
- Repository map: skip files that have no syntax tree, so they no longer take a symbol budget slot or log UTF-8 parse errors.

## 1.4.1 - 2026-11-20

- Fix Wu crashing right after launch: the repo map panel read the workspace while the workspace was still being updated, which 1.4.0 hit on every start.
- Fix Wu crashing right after launch when the Remote Explorer panel's settings observer updated the panel from inside its own update.
- Fix the Repo Map button in the activity bar doing nothing: the panel's actions were never registered in production builds.
- Ask Agent About Selection now starts the new chat with the agent and model of the chat you are looking at, or the most recent one, instead of always picking Claude with Fable 5.
- Add **Add Selection to Chat** to the editor context menu: it appends the selected code to the chat you are looking at, and tells you to open a chat first instead of starting one for you.
- Edit predictions (ghost text) remain experimental and off by default: enable them only for endpoints you trust, and note that the API key is stored in plain text in your settings file.

## Unreleased

- Keep the isolated Pi home private: its directories are now `0700` and the credentials and transcripts inside it `0600`, so other accounts on the same machine can no longer read them.
- Ship `LICENSE-GPL`, `LICENSE-APACHE` and `NOTICE` with every bundle, and show the license in the Windows installer, so a distributed build carries the terms it is licensed under.
- Bundle the licenses of the redistributed agent runtimes (Node.js, `dsh-acp`, `pi-acp`, `pi-coding-agent`) next to them.
- Record which version of each bundled runtime is checked in, and how to verify it, in `agents/VERSIONS.md`.
- Give the release workflow read-only default permissions and hand write access only to the job that publishes.
- Replace the upstream ReadMe and install guide with documentation for this fork: correct download names, unsigned-build instructions, update behaviour, the list of outbound requests, and licensing.
- Move the profile directory to a unique name, `WuIDE`, so it cannot collide with another application called IDE. An existing `IDE`, `ide`, `Wu` or `wu` profile is copied over on first launch, so settings, sessions and agent data survive the rename.
- Add `SYNC.md` and `script/sync-upstream` to record where this fork diverged from upstream and what merging the next upstream release will touch.
- Fix the Git panel going stale after running git commands outside Wu. (from upstream)
- Fix new files ignoring the `line_ending` setting. (from upstream)
- Fix a console window opening on Windows when an agent starts. (from upstream)
- Fix OpenCode questions not showing a picker. (from upstream)
- Fix Wu crashing on startup on ARM Linux. (from upstream)
- List models in the model picker in the same order as the underlying agent. (from upstream)

## 1.4.0 - 2026-11-19

- Add a Remote Explorer sidebar panel that lists configured SSH/WSL hosts and hosts discovered in `~/.ssh/config`, marks active connections, expands registered projects, and offers connect, open, refresh and new-remote actions. Toggle it with `ctrl-alt-r` (`ctrl-cmd-r` on macOS).
- Add a repo map panel with a zero-cost directory tree and an on-demand symbol map: definitions are extracted with tree-sitter and files ranked by cross-file references (PageRank), so the most-referenced modules surface first. Toggle it from the command palette or the activity bar.
- Add `/conventions` and `/repo-map` chat commands: they expand project AGENTS.md files and the ranked repo map into the composer without sending them, so you can review exactly what the agent receives. Both support inline expansion on their own line in a longer message.
- Start a Pi chat directly inside a git worktree from the worktree picker.
- Add inline edit predictions (ghost text) from any OpenAI-compatible endpoint, including DeepSeek and local Ollama. Disabled by default; enable via `edit_predictions` settings with an endpoint and optional API key. Tab accepts, cursor movement dismisses.
- Ask the configured agent to explain the selected code or fix the diagnostics at the cursor from the editor context menu.
- Restore extension-provided slash commands and context servers in the WASM extension API.
- Make the inlay hints toggle persist across restarts.
- Download the dictation ONNX Runtime from PyPI wheels: the upstream GitHub release is gone, so speech model installation no longer 404s.
- Raise cargo build parallelism to match 16-core machines and remove 46 unused dependency declarations.
- Keep the i18n completeness test working on Windows for platform-gated actions.

## 1.3.2 - 2026-11-11

- Turn automatic updates back on, checking this project's own GitHub releases rather than the upstream project's.
- Announce a new version and wait for **Download and Install** before replacing the app; **Skip This Version** stops a release you do not want from asking again until a higher one ships.
- Fix installing an update on macOS by reading the mounted disk image's real volume, and on Linux by looking for the bundled `ide.app`.
- Stop **Check for Updates** from reporting an error when a published release has no download that matches this build.
- Show the CLI as `ide <path>` in the setting that describes how directories open from the command line.

## 1.3.1 - 2026-11-11

- Show the application icon on the welcome and onboarding screens, and use the same "不打扰你的编辑器" tagline on both of them.
- Label the base keymap IDE while keeping the stored value `Zed`, so existing settings keep working.
- Rename the built-in UI themes to `IDE Dark` / `IDE Light` / `IDE Dark Translucent` and map the old names when settings load.
- Draw the base-keymap mark and the agent chat thinking animation with the character silhouette, and keep the spinner's tint bands.
- Give the Pi agent its own pixel mark instead of the OpenCode icon.
- Report IDE to debug adapters, in the update helper window title and in the Windows COM surrogate name.

## 1.3.0 - 2026-11-11

- Add the DeepSeek and Pi agents, each with a visibility toggle in settings.
- Pick a model and reasoning effort for Pi from the model picker, re-reading Pi's catalogue when it changes on disk.
- Rename the app to IDE in window titles, the app menu, dialogs, CLI help and the macOS bundle name.
- Ship the Windows build as `IDE.exe` and keep the profile in an `IDE` directory. Existing installs start with an empty profile: settings, chats and logs from the `Wu` directory are not migrated.
- Package the renamed executable in the macOS and Linux bundles, and let the `wu` CLI find installs that still ship `Wu.exe`.
- Use the IDE name in the Windows installer, Start menu entry, context menus and the Linux app-launcher entry.
- Fix the `wu` CLI failing to find the installed app on Windows.
- Bundle the DeepSeek and Pi agents in Linux builds.
- Let Pi use a Node runtime from `PATH` when the bundle doesn't ship one.
- Report that updates are unavailable when checking for updates, instead of doing nothing.
- Fill in the missing Simplified Chinese translations for the DeepSeek and Pi agents.

## 1.2.1 - 2026-11-09

- Fix links and text selection in tables rendered in agent chat.
- Allow multiple tabs inside the agent chat side panel.
- Reduce quoted text bar's width.
- Close images in agent chat on background click.
- Make agent thinking indicator animation look more like Wu.

## 1.2.0 - 2026-11-08

- Add a side panel for agent chats.
- Fix voice dictation UI in agent chat.
- Fix window resize not persisting on Windows.
- Inter at 350 weight is now the new interface font.
- Bring in improvements and bug fixes from Zed v1.22.
- Fix background command indicator wrapping onto the next line.
- Fix thought process getting nested under two headers when there's nothing else in the group.

## 1.1.19 - 2026-11-07

- Fix jank in git panel's history.
- Improve agent chat input UI.
- Improve agent questions UX.
- Smaller download and install size on macOS.

| Binary | 1.1.18 (MB) | 1.1.19 (MB) | Change |
|---|---|---|---|
| Wu-aarch64.dmg | 86.8 | 72.2 | −17% |
| Wu-x86_64.dmg | 95.4 | 75.8 | −21% |

## 1.1.18 - 2026-11-06

- Smaller download and install size on Linux and Windows.

| Binary | 1.1.17 (MB) | 1.1.18 (MB) | Change |
|---|---|---|---|
| wu-linux-aarch64.tar.gz | 86.3 | 54.7 | −37% |
| wu-linux-x86_64.tar.gz | 89.4 | 57.8 | −35% |
| Wu-x86_64.exe | 53.6 | 39.6 | −26% |

## 1.1.17 - 2026-11-06

- Clearer message in Agent Chat when an agent isn't installed.
- Fixed the Windows installer putting Wu into Zed's folder, and Zed into Wu's, when both are installed.
- Fixed the Explorer "Open with Wu" menu showing Zed's title when Zed is installed.
- New `project_panel.open_mode` setting to open files in the Project panel with a double click. Possible values: `single_click` (default) and `double_click`.
- Write commit messages with agents from the Git panel.
- Commits in the Git panel's History now expand to list their changed files. Clicking a file follows `git_panel.entry_primary_click_action`.
- New `git.graph_date_format` setting to show Git Graph dates as `2026-11-04 16:14:00`. Possible values: `default` and `iso`.

## 1.1.16 - 2026-11-02

- Fix debug session picker overlapping the panel's tabs.
- Switching projects from the top-left menu keeps the previous projects open in the background.
- Easy switching between multiple open projects in a window.
- Open projects in the project menu show a dot while an agent is working or needs you.
- Clicking an agent notification switches to that chat's project.

## 1.1.15 - 2026-10-31

- Fixed Wu started from Raycast still getting Raycast's env in its shell.

## 1.1.14 - 2026-10-31

- Fixed the agent chat box jumping between one and two lines.
- Lines starting with `>` in agent chat messages now render as quotes on send.
- Forked chats are now numbered for distinction.
- Fixed projects opened from Raycast inheriting the launcher's env in their terminals.
- Fixed GPT-6.1-Sol missing from the Codex model list.

## 1.1.13 - 2026-10-28

- Subagents running in the background can now be expanded and observed via the background task indicator.
- On WSL2, Wu can now use the GPU when started with `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1`.
- In agent chat, SVG, BMP and TIFF files are now added as file references instead of being turned into PNG attachments.
- Added syntax highlighting for XML and SVG files.

## 1.1.12 - 2026-10-26

- Add support for Simplified Chinese (Preview).
- Cancelled messages in older Codex chats no longer offer Edit.
- Fixed agent chats with generated images jumping while you scroll.

## 1.1.11 - 2026-10-26

- You can now save important messages in agent chats for later.
- Wu Dark has a new solid look with softer colors. Previous Wu Dark is now `Wu Dark Translucent`.
- Typing `@` in an agent chat now shows more relevant file suggestions.
- Images in agent chats can now be downloaded.
- Fixed Perso-Arabic script and other text with raised or lowered letters looking scrambled on macOS.
- Cancelled messages can be edited and sent again, as long as the agent didn't make any changes.
- Line breaks in agent replies now show as written instead of running lines together.

## 1.1.10 - 2026-10-24

- Agent CLIs are now kept up to date by Wu.
- Agent chats are now restored on restart.
- Typing `@` in an agent chat now lists project's files right away.
- Typing `$` in a Claude Code chat now lists skills instead of only listing them on `/`.
- Files, commands and skills picked from the chat box menus show up as badges instead of plain text.
- Removed running shell commands by starting an agent chat message with `!`.
- Removed the checkout and branch picker from under the agent chat input.

## 1.1.9 - 2026-10-24

- Start a message with `!` in an agent chat to run it as a shell command.
- Images can now be dragged onto Wu's dock icon and dropped into one of its windows on macOS.
- Changing the permission mode now applies to the turn immediately instead of on the next message.
- Fixed long permission prompts pushing the Allow and Deny buttons off screen.
- Subagents and files referred in agent chats now open in a side pane instead of a new tab.
- The cursor in the agent chat box is thinner.

## 1.1.8 - 2026-10-20

- Fixed "Dock Right" not moving the agent panel.

## 1.1.7 - 2026-10-15

- Fixed Codex bundled with the ChatGPT desktop app not found.
- Chats can now be renamed from their tabs.
- Tables in agent replies can be copied as markdown.
- Fixed Claude Code chats that seemed stuck.
- OpenCode chats now pick up the name OpenCode gives them.

## 1.1.6 - 2026-10-11

- Added "Pull From" to the Git sync menu, so you can pull any branch from any remote.
- Fixed the Git sync button spinning forever after closing the remote picker.
- Update the source address for auto-updates.

## 1.1.5 - 2026-10-09

- Fixed Codex computer use being blocked.
- Claude Code chats can now use your Chrome browser through the Claude in Chrome extension.
- Opening an empty folder in the file tree now shows an "Empty" line under it.
- Claude Code skills in the chat's / menu now have their own icon, so they're easy to tell apart from commands.
- Added an Enable Agent Chat setting. Turn it off to hide the agent panel and everything agent-releated.
- Added more configuration options for agent chats to Settings.
- Wu now asks before quitting or restarting while an agent is working.
- Added "Continue Saved Chat" to the new chat menu, which picks up a chat you started in a CLI.
- Added settings to hide Claude Code, Codex or OpenCode from the new chat menu.
- Desktop notifications for agent chats are now off by default. On macOS, turning them on opens system preferences to allow Wu's notifications.
- Removed Claude Code's settings and info commands like /model, /usage and /context from the chat's / menu.


## 1.1.4 - 2026-10-08

- Made the time on agent chats in the sidebar easier to read.
- Fixed error and warning popups in the editor being see-through and hard to read in Wu Dark.
- Cleaned up the context menu options in the agent chat sidebar.
- Forked chats now continue from a real copy of the agent's own session.
- User messages in agent chats can now be selected and copied in part.
- Fixed open files sometimes not coming back when reopening a project.

## 1.1.3 - 2026-10-06

- Support for Intel Macs.
- Fixed subagent tabs not showing the working indicator while a background subagent was still running.
- Hovering the running command or agent indicator under the chat box now shows what's running in a tooltip.
- The title bar and status bar in Wu Dark now have the same tint as the panels.
- Fixed the working indicator in agent chats not coming back when the agent picks up again after a background process finishes.
- Agent Chat now supports OpenCode.
- The line number gutter is more compact.
- The activity bar can sit on the right side of the window. Set it with `activity_bar.position` or in Settings.
- A little more space between the activity bar icons.
- Activity bar icons now follow the UI font size by default.
- Fixed the side panel jumping wider when you start resizing it.
- Users can now choose permission modes from the model picker.
- Improve Wu Light's interface colors.

## 1.1.2 - 2026-10-05

- Fixed images dropped onto an agent chat opening in a preview instead of being attached.
- When an agent is waiting on a background command or subagent, the bar under the chat box shows what's running and for how long.
- Search inside an open agent chat with the Find bar (Cmd-F, or Ctrl-F on Windows and Linux).
- Fixed macOS asking Wu for access to Desktop, Documents and Photos when agent chat looked up the available models.
- Fixed the tab and the split and zoom buttons flickering after clicking into a chat message and scrolling away from it.
- Fixed empty thought blocks in agent chats.
- Thoughts in agent chats stay collapsed until you open them.
- Claude models now use their largest context window by default, and Wu picks up the available sizes from Claude Code.
- Agent notifications now use the system's native notifications.
- In the model picker, effort is set with a slider and context window show every choice as a button.
- Fixed the dark bar that showed at the top of the file tree when scrolling in Wu themes.
- The sidebar, activity bar and tab bar in Wu Light now have solid colors.
- Fixed the context window and plan usage popups in agent chats staying open when clicking elsewhere.
- Removed the options menu from the top of the agent chat sidebar.
- Agent chats in the sidebar no longer repeat the project name on every row, so the list is shorter and easier to scan.
- The agent chat sidebar now uses the same background as the other panels in every theme.

## 1.1.1 - 2026-10-05

- `/compact` and other slash commands now work mid-conversation in Claude chats.
- The chat shows "Compacting conversation…" while the agent compacts and the context ring updates right after.
- Long agent chats scroll smoothly and typing in the chat box feels instant, since only the messages on screen are drawn.
- Fixed a black bar at the top of the file tree when scrolling with the Wu themes.
- Fixed text in the chat box being hard to read after switching between Wu Dark and Wu Light.

## 1.1.0 - 2026-10-04

- The activity bar's icon size is now configurable in settings and via the `activity_bar.icon_size` property.
- New Wu Light and Wu Dark themes.
- Symbols is the new default icon theme.
- Geist is the new interface font, and JetBrains Mono is the new code font.
- Redesigned tabs.
- New Agent Chat panel in the activity bar. Chat with Claude Code or Codex using the CLIs you already have installed and signed in; no API keys needed.
- Agent chats support slash commands, `@` file mentions and `$` skills, with a menu that completes them as you type.
- Attach images to a message by pasting, dropping or using the paperclip button.
- The chat sidebar has pins, custom sections, an archive, search and grouping by project. Chats can be forked, and side chats open next to the main one.
- Wu can play a sound and show a notification when the agent finishes, needs your input or hits an error.
- Voice dictation in the chat box, using a local speech model that downloads on first use.
- New Agent Chat page in settings.
- Idle windows on macOS stop redrawing until something changes, which saves power.
- Fixed some list scrolling glitches and images that were sized wrongly in some layouts.

## 1.0.10 - 2026-09-26

- Project search uses much less memory. Files in the results are only parsed for syntax highlighting once they're shown on screen.
- Breadcrumbs and sticky headers fill in as soon as a newly opened file finishes parsing, instead of waiting for the cursor to move.
- Selected files in lists like the project panel stand out more in the Catppuccin themes.
- Fixed a crash when uninstalling an extension whose theme has the same name as a built-in theme.
- Opening a result from the search panel highlights the matches in that file the same way in-file search does.
- Fixed cursor and scroll positions sometimes not being saved for files that were just opened.

## 1.0.9 - 2026-09-20

- Files copied in the project panel can now be pasted into another Wu window or into other apps.
- Closing the last tab keeps the window open by default.
- The project panel context menu has a Get Info item (Properties on Windows and Linux) that shows a file or folder's path, size, created and modified dates, and permissions.
- Removed the bundled Ayu and Gruvbox themes.

## 1.0.8 - 2026-09-12

- Material Icon Theme is now the default icon theme, with light and dark variants that follow the theme mode.
- Synced with Zed 1.19.2: multi-select in the Git panel, automatic language detection for untitled buffers, project search on type, recency-sorted command palette, the `reveal_if_open` setting, and many fixes.
- Removed "Delete Permanently" from the project panel context menu. Delete always moves to the Trash.
- Right-clicking the empty space below the file tree now opens the context menu.
- Tabs for files that no longer exist show "File not found. It was deleted or moved." without offering to recreate the file.
- Release builds compile every crate as a single codegen unit again for better runtime performance.

## 1.0.7 - 2026-09-11

- Files that were deleted while Wu was closed reopen as strikethrough tabs with a "file not found" message instead of a blank editor.
- Tabs show the file's icon before its name.
- The project panel's Delete action moves files to the Trash, with a separate Delete Permanently option.
- Activity bar icons match VS Code's, with more vertical spacing, and all left-dock panels share one width.
- Search moved into its own panel, and activity bar items can be reordered.
- The file tree scrolls a little past its last entry.
