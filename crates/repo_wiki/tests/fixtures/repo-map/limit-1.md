## Repository map

showing 1–1 of 51 files with definitions.
Order: referencing fan-in (desc), then IDF-weighted reference PageRank (desc), then path. `in N` counts distinct files that reference a definition this file owns, ignoring names shared by more than 3 files; `out M` counts the files this one references under the same rule; `deps N` counts the files whose `mod`, `use`, `#include` or relative `import` resolves to this one, so it is a certain dependency rather than a shared name.
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

Omitted files (50, paths only):
crates/project/src/project.rs
crates/language/src/language.rs
crates/editor/src/editor.rs
crates/workspace/src/workspace.rs
crates/text/src/text.rs
crates/ui/src/ui.rs
crates/settings/src/settings.rs
crates/lsp/src/lsp.rs
crates/path/src/path.rs
crates/util/src/util.rs
… +40 more omitted (call files_only or raise offset to list them)

Next page: offset=1, limit=1.
