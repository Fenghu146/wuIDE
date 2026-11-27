## Repository map

showing 1–10 of 51 files with definitions.
Order: referencing fan-in (desc), then IDF-weighted reference PageRank (desc), then path. `in N` counts distinct files that reference a definition this file owns, ignoring names shared by more than 3 files; `out M` counts the files this one references under the same rule; `deps N` counts the files whose `mod`, `use`, `#include` or relative `import` resolves to this one, so it is a certain dependency rather than a shared name.
Parsed 52 / 60 candidate files (5 unsupported language, 2 over 1024 KiB, 1 unreadable); 51 with definitions.
Excluded before scanning: 3 vendored or generated paths.

`crates/gpui/src/gpui.rs` (in 128 / out 31 / deps 7, 1820 lines)

`crates/project/src/project.rs` (in 97 / out 24 / deps 1, 1420 lines)

`crates/language/src/language.rs` (in 88 / out 19 / deps 2, 980 lines)

`crates/editor/src/editor.rs` (in 74 / out 40 / deps 3, 2100 lines)

`crates/workspace/src/workspace.rs` (in 61 / out 18 / deps 0, 800 lines)

`crates/text/src/text.rs` (in 55 / out 12 / deps 1, 640 lines)

`crates/ui/src/ui.rs` (in 48 / out 6 / deps 2, 410 lines)

`crates/settings/src/settings.rs` (in 41 / out 9 / deps 3, 330 lines)

`crates/lsp/src/lsp.rs` (in 37 / out 11 / deps 0, 520 lines)

`crates/path/src/path.rs` (in 33 / out 4 / deps 1, 260 lines)

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
