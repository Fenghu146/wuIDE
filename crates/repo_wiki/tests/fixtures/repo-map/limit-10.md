## Repository map

showing 1–10 of 51 files with definitions.
Order: referencing fan-in (desc), then IDF-weighted reference PageRank (desc), then path. `in N` counts distinct files that reference a definition this file owns, ignoring names shared by more than 20 files; `out M` counts the files this one references under the same rule; `deps N` counts the files whose `mod`, `use`, `#include` or relative `import` resolves to this one, so it is a certain dependency rather than a shared name.
Parsed 52 / 60 candidate files (5 unsupported language, 2 over 1024 KiB, 1 unreadable); 51 with definitions.
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

Omitted files (41, paths only):
crates/util/src/util.rs
crates/theme/src/theme.rs
crates/input/src/input.rs
crates/menu/src/menu.rs
crates/modal/src/modal.rs
crates/picker/src/picker.rs
crates/terminal/src/terminal.rs
crates/tasks/src/tasks.rs
crates/search/src/search.rs
crates/outline/src/outline.rs
crates/diagnostics/src/diagnostics.rs
crates/git/src/git.rs
crates/fs/src/fs.rs
crates/http/src/http.rs
crates/rpc/src/rpc.rs
crates/proto/src/proto.rs
crates/json/src/json.rs
crates/toml/src/toml.rs
crates/yaml/src/yaml.rs
crates/markdown/src/markdown.rs
… +21 more omitted (call files_only or raise offset to list them)

Next page: offset=10, limit=10.
