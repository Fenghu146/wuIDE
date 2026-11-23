## Repository map

showing 1–40 of 51 files with definitions.
Order: referencing fan-in (desc), then IDF-weighted reference PageRank (desc), then path. `in N` counts distinct files that reference a definition this file owns, ignoring names shared by more than 20 files; `out M` counts the files this one references under the same rule; `deps N` counts the files whose `mod`, `use`, `#include` or relative `import` resolves to this one, so it is a certain dependency rather than a shared name.
Indexed 52 / 60 candidate files (5 unsupported language, 2 over 1024 KiB, 1 unreadable).
Excluded before scanning: 3 vendored or generated paths.

`crates/gpui/src/gpui.rs` (in 128 / out 31 / deps 7, 1820 lines)
- struct App (line 40)
- `public:`
  - function run(self) (line 88)
  - field cx: &mut Context<App> (line 96)
  - field background: Arc<Background> (line 97)
  - field foreground: Arc<Foreground> (line 98)
- `private:`
  - function quit(self) (line 120)

`crates/project/src/project.rs` (in 97 / out 24 / deps 1, 1420 lines)
- struct Project (line 70)
- function worktree_for_entry(self, entry_id: u64) -> Option<Entity<Worktree>> (line 320)

`crates/language/src/language.rs` (in 88 / out 19 / deps 2, 980 lines)
- struct Language (line 120)

`crates/editor/src/editor.rs` (in 74 / out 40 / deps 3, 2100 lines)
- struct Editor (line 60)
- function buffer(self) -> &Entity<Buffer> (line 700)
- function scroll (line 900)

`crates/workspace/src/workspace.rs` (in 61 / out 18 / deps 0, 800 lines)
- struct Workspace (line 30)

`crates/text/src/text.rs` (in 55 / out 12 / deps 1, 640 lines)
- function op_0(self) (line 1)
- function op_1(self) (line 4)
- function op_2(self) (line 7)
- function op_3(self) (line 10)
- function op_4(self) (line 13)
- function op_5(self) (line 16)
- function op_6(self) (line 19)
- function op_7(self) (line 22)
- function op_8(self) (line 25)
- function op_9(self) (line 28)
- function op_10(self) (line 31)
- function op_11(self) (line 34)
- function op_12(self) (line 37)
- function op_13(self) (line 40)
- function op_14(self) (line 43)
- function op_15(self) (line 46)
- function op_16(self) (line 49)
- function op_17(self) (line 52)
- function op_18(self) (line 55)
- function op_19(self) (line 58)
- function op_20(self) (line 61)
- function op_21(self) (line 64)
- function op_22(self) (line 67)
- function op_23(self) (line 70)
- function op_24(self) (line 73)
- function op_25(self) (line 76)
- function op_26(self) (line 79)
- function op_27(self) (line 82)
- function op_28(self) (line 85)
- function op_29(self) (line 88)
- function op_30(self) (line 91)
- function op_31(self) (line 94)
- function op_32(self) (line 97)
- function op_33(self) (line 100)
- function op_34(self) (line 103)
- function op_35(self) (line 106)
- function op_36(self) (line 109)
- function op_37(self) (line 112)
- function op_38(self) (line 115)
- function op_39(self) (line 118)
… +2 more symbols

`crates/ui/src/ui.rs` (in 48 / out 6 / deps 2, 410 lines)
- struct Label (line 22)

`crates/settings/src/settings.rs` (in 41 / out 9 / deps 3, 330 lines)
- struct Settings (line 12)

`crates/lsp/src/lsp.rs` (in 37 / out 11 / deps 0, 520 lines)
- struct LanguageServer (line 40)

`crates/path/src/path.rs` (in 33 / out 4 / deps 1, 260 lines)
- struct RelPath (line 18)

`crates/util/src/util.rs` (in 30 / out 2 / deps 2, 180 lines)
- function defer (line 20)
- variable x3 (lines 30–32)

`crates/theme/src/theme.rs` (in 28 / out 0 / deps 3, 120 lines)
- struct Theme (line 10)

`crates/input/src/input.rs` (in 27 / out 1 / deps 0, 127 lines)
- function Input (line 11)

`crates/menu/src/menu.rs` (in 26 / out 2 / deps 1, 134 lines)
- function Menu (line 12)

`crates/modal/src/modal.rs` (in 25 / out 3 / deps 2, 141 lines)
- struct Modal (line 13)

`crates/picker/src/picker.rs` (in 24 / out 4 / deps 3, 148 lines)
- function Picker (line 14)

`crates/terminal/src/terminal.rs` (in 23 / out 0 / deps 0, 155 lines)
- function Terminal (line 15)

`crates/tasks/src/tasks.rs` (in 22 / out 1 / deps 1, 162 lines)
- struct Tasks (line 16)

`crates/search/src/search.rs` (in 21 / out 2 / deps 2, 169 lines)
- function Search (line 17)

`crates/outline/src/outline.rs` (in 20 / out 3 / deps 3, 176 lines)
- function Outline (line 18)

`crates/diagnostics/src/diagnostics.rs` (in 19 / out 4 / deps 0, 183 lines)
- struct Diagnostics (line 19)

`crates/git/src/git.rs` (in 18 / out 0 / deps 1, 190 lines)
- function Git (line 20)

`crates/fs/src/fs.rs` (in 17 / out 1 / deps 2, 197 lines)
- function Fs (line 21)

`crates/http/src/http.rs` (in 16 / out 2 / deps 3, 204 lines)
- struct Http (line 22)

`crates/rpc/src/rpc.rs` (in 15 / out 3 / deps 0, 211 lines)
- function Rpc (line 23)

`crates/proto/src/proto.rs` (in 14 / out 4 / deps 1, 218 lines)
- function Proto (line 24)

`crates/json/src/json.rs` (in 13 / out 0 / deps 2, 225 lines)
- struct Json (line 25)

`crates/toml/src/toml.rs` (in 12 / out 1 / deps 3, 232 lines)
- function Toml (line 26)

`crates/yaml/src/yaml.rs` (in 11 / out 2 / deps 0, 239 lines)
- function Yaml (line 27)

`crates/markdown/src/markdown.rs` (in 10 / out 3 / deps 1, 246 lines)
- struct Markdown (line 28)

`crates/syntax/src/syntax.rs` (in 9 / out 4 / deps 2, 253 lines)
- function Syntax (line 29)

`crates/highlight/src/highlight.rs` (in 8 / out 0 / deps 3, 260 lines)
- function Highlight (line 30)

`crates/rope/src/rope.rs` (in 7 / out 1 / deps 0, 267 lines)
- struct Rope (line 31)

`crates/clock/src/clock.rs` (in 6 / out 2 / deps 1, 274 lines)
- function Clock (line 32)

`crates/time/src/time.rs` (in 5 / out 3 / deps 2, 281 lines)
- function Time (line 33)

`crates/log/src/log.rs` (in 4 / out 4 / deps 3, 288 lines)
- struct Log (line 34)

`crates/metrics/src/metrics.rs` (in 3 / out 0 / deps 0, 295 lines)
- function Metrics (line 35)

`crates/cache/src/cache.rs` (in 2 / out 1 / deps 1, 302 lines)
- function Cache (line 36)

`crates/store/src/store.rs` (in 1 / out 2 / deps 2, 309 lines)
- struct Store (line 37)

`crates/config/src/config.rs` (in 0 / out 3 / deps 3, 316 lines)
- function Config (line 38)

Omitted files (11, paths only):
crates/keymap/src/keymap.rs
crates/command/src/command.rs
crates/panel/src/panel.rs
crates/dock/src/dock.rs
crates/toolbar/src/toolbar.rs
crates/status/src/status.rs
crates/notify/src/notify.rs
crates/dialog/src/dialog.rs
crates/welcome/src/welcome.rs
crates/recent/src/recent.rs
crates/chat/src/chat.rs

Next page: offset=40, limit=40.
