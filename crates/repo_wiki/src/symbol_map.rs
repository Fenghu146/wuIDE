//! Symbol-level repository map.
//!
//! Every file is parsed once with tree-sitter using the grammar's outline
//! query (the same queries the outline panel uses, so no per-language work is
//! needed here). Definitions become the file's symbol list; identifiers become
//! edges in a cross-file reference graph, which is ranked with PageRank so the
//! files that the rest of the repository talks about float to the top.
//!
//! Not every mention is evidence of a dependency. `init`, `new`, `run` and
//! `tests` are written by hundreds of files and implemented by dozens, so an
//! edge through one of them records shared vocabulary rather than one file
//! using another's definition. Edges are therefore weighted by how distinctive
//! the name is (see [`shared_name_file_limit`] and [`mention_weight`]), and the
//! `in`/`out` counts shown to agents report the same filtered edges the rank is
//! built from.
//!
//! The whole thing is a bounded, cancellable background job: it never runs
//! unless the repo map panel is opened or `/repo-map` is used, and it drops
//! its task as soon as the caller goes away.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language::{Language, LanguageRegistry};
use path::rel_path::RelPath;
use project::Project;
use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator as _;

use crate::symbol_cache::{self, CacheEntry, FileStamp};

/// Upper bound on how many files one build parses. A file costs roughly ten
/// milliseconds to read, parse and walk for identifiers, so this cap is worth
/// about thirty seconds of background work; the map is built once and cached.
///
/// This is one of two gates on a build: it stops a monorepo with hundreds of
/// thousands of files from making the map enormous, while [`MAX_TOTAL_BYTES`]
/// bounds the work in bytes. Whichever runs out first wins, and the coverage
/// line says which one did. 3000 is above the number of indexable files in this
/// repository (~2,500), so the map of wuIDE itself is complete; only a larger
/// repository sees this cap.
pub const MAX_FILES: usize = 3000;
/// Files larger than this are skipped: they are almost always generated or
/// vendored data, and parsing them dominates the runtime.
///
/// 1 MiB. The previous 256 KiB cap excluded 18 of this repository's own core
/// sources — `crates/workspace/src/workspace.rs` (677 KiB),
/// `crates/project/src/lsp_store.rs` (637 KiB), `crates/editor/src/editor.rs`
/// (454 KiB), `crates/gpui/src/window.rs` (333 KiB) and the like — so the map
/// could never rank them and an agent asking "what depends on the editor" got
/// `display_map.rs` instead of `editor.rs`. A big hand-written module is exactly
/// the file a map should name; a big generated one is already caught by
/// [`VENDORED_DIRS`] or by its language never being indexed. The cap stays as a
/// guard against a genuinely enormous single file: nothing above 1 MiB is
/// parsed, and those files are counted as `too_large` in the coverage line.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Cap on identifiers collected per file, which bounds the reference graph.
const MAX_IDENTIFIERS_PER_FILE: usize = 4_000;
/// Cap on symbols kept per file.
const MAX_SYMBOLS_PER_FILE: usize = 200;
/// Cap on the total bytes parsed in one build, so the work stays bounded even
/// when a repository has many medium-sized files. It is the byte-side gate
/// paired with [`MAX_FILES`]: a repository can pass one and hit the other, and
/// the coverage line names the one that ran out.
///
/// 24 MiB was too tight once the file limit rose: this repository's indexable
/// set is about 32 MiB, so the byte gate alone dropped 357 files, 213 of them
/// Rust. 64 MiB holds the whole set, leaving the file count as the only gate
/// for repositories up to a few thousand files.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Directory names whose contents are vendored or generated rather than the
/// project's own code.
///
/// They are skipped before any budget applies, because a tracked `node_modules`
/// (this repository commits one) is invisible to `.gitignore` and its bundled
/// JavaScript would otherwise fill the whole map. Both the list and the hidden
/// directory rule are mirrored by `agents/mcp/wu-repo-map/server.js`, which uses
/// them only to explain to an agent why a path is missing from the snapshot.
const VENDORED_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "third_party",
    "third-party",
    "dist",
    "site-packages",
    "target",
    ".venv",
    "venv",
    "bower_components",
    ".yarn",
    ".pnpm-store",
    "out",
    "Pods",
    "__pycache__",
    ".tox",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".gradle",
];
const PAGERANK_ITERATIONS: usize = 24;
const PAGERANK_DAMPING: f32 = 0.85;
/// A name that more files than this mention creates no reference edge at all.
///
/// The reference graph answers "which file owns the definitions the rest of the
/// repository uses". A name written by many files cannot answer that: the
/// mention says the two files share vocabulary, not that the mentee uses the
/// mentor's definition.
///
/// The statistic is the number of files that *mention* the name, not the number
/// that define it. Definers alone do not separate the two kinds of noise:
/// `init` is defined in 106 files and mentioned by hundreds, while `Option`,
/// `Box` and `File` are defined by exactly one file — the `IconName` enum
/// carries variants with those names — and mentioned by well over a thousand
/// files, most of which mean the standard library type. Counting definers
/// therefore kept `crates/icons/src/icons.rs` first in the map, and every file
/// that wrote `Option<T>` fed it. Mentioning files catch both.
///
/// Measured on this repository: 27,469 of the 32,881 distinct names are defined
/// in exactly one file, and the names past this line are the ones with no
/// ownership signal — `new` 583 definitions, `tests` 410, `render` 297, `name`
/// 264, `from` 137, `fmt` 113, `init` 106, `default` 105 — plus the shadowed
/// standard names above.
///
/// The cap scales with the repository. Twenty is a little over one percent of
/// this repository's 1,565 indexed files, and it is where a name stops pointing
/// at an owner: at or below it a mention is one of a handful that tie two files
/// together, and above it the name is part of how the whole codebase talks. A
/// fixed 20 would be 18% of a 113-file repository, where almost no name reaches
/// it and every mention becomes an edge; one fiftieth of the indexed files keeps
/// the same fraction at any size. The floor keeps small repositories from
/// treating two or three mentions as ownership, and the ceiling stops common
/// names in very large repositories from turning into edges.
pub fn shared_name_file_limit(file_count: usize) -> usize {
    (file_count / 50).clamp(3, 50)
}

/// Definition kinds that can own an inbound reference edge.
///
/// These are the names another file can call or name as an API: functions,
/// methods, types, traits, constants, macros and modules. What is left out —
/// `field`, `variant`, `variable` and the generic `symbol` of a data file — is a
/// name *inside* something: a struct field, an enum variant, a local, a JSON key.
/// Those spellings collide with unrelated code all over a repository — the
/// `IconName` enum "defines" `Option`, `Box` and `File`; `EditorSettingsContent`
/// "defines" `Menu`, `Full` and `Open` — and an edge through one of them says the
/// two files share a word, not that one depends on the other.
///
/// Measured on this repository: `crates/icons/src/icons.rs` owns 200 names, 198
/// of them variants, and with those counted it is the map's first file (282
/// referencing files); without them it leaves the top fifteen entirely. The
/// seven `settings_content` files that own the settings vocabulary fall out the
/// same way: `editor.rs` keeps 58 referencing files instead of 218, and never
/// reaches the first page. Tests are unaffected — `tests.rs` and `util.rs` own
/// functions, so their counts and positions do not move.
pub const OWNING_KINDS: &[&str] = &[
    "function",
    "method",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "constant",
    "macro",
    "module",
];

/// Whether a definition of this kind can own an inbound reference edge.
fn owns_references(kind: &str) -> bool {
    OWNING_KINDS.contains(&kind)
}

/// The group of languages whose identifiers can name one another's definitions.
///
/// A mention only counts when the mentioning file and the defining file speak the
/// same language: `Option`, `Menu` and `theme` are spelled identically in Rust,
/// TypeScript, JSON and Markdown, so a cross-language match is a coincidence of
/// spelling. On this repository that removes 1,795 of 45,823 edges (3.9%), but
/// they are the ones that matter: 91–95% of the inbound edges of
/// `website/i18n/en.ts`, `zh-CN.ts` and `website/components/WuWindow.tsx` were
/// Rust files, which is what put three website files in the first page.
///
/// Related grammars share a group because their identifiers really do refer to
/// each other: a `.c` file includes `.h` files that the `C++` grammar claims, and
/// a `.tsx` component imports `.ts` modules. A language this list does not name
/// is its own group, so adding a grammar cannot silently borrow another's names.
fn language_family(name: &str) -> &str {
    match name {
        "Rust" => "rust",
        "C" | "C++" => "c",
        "TypeScript" | "TSX" | "JavaScript" => "javascript",
        "Python" => "python",
        "Go" => "go",
        "JSON" | "JSONC" => "json",
        "Markdown" | "Markdown Inline" => "markdown",
        "YAML" => "yaml",
        "Shell Script" => "shell",
        other => other,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub name: Arc<str>,
    /// The normalized category of the definition, e.g. `function`.
    pub kind: Arc<str>,
    /// Zero-based row of the definition's name.
    pub row: u32,
    /// Name of the enclosing definition (a type, function, namespace or
    /// module), when the symbol is nested inside one.
    pub owner: Option<Arc<str>>,
    /// Number of enclosing definition bodies, so the panel can indent.
    pub depth: u32,
    /// C++ access specifier of a type member.
    pub access: Option<Arc<str>>,
    /// Suffix shown after the name: parameters and return type for functions,
    /// the declared type for variables (`(int x) -> int`, `: ListNode*`).
    pub signature: Option<Arc<str>>,
    /// True when the definition lives inside a function body.
    pub local: bool,
}

#[derive(Clone, Debug)]
pub struct FileSymbols {
    pub rel: Arc<RelPath>,
    pub size: u64,
    pub rank: f32,
    /// Number of text lines in the file.
    pub lines: u32,
    /// Distinct files that reference a definition this file owns.
    ///
    /// Only edges that survived [`shared_name_file_limit`] count, so this is the same
    /// graph the rank is computed from: a file whose only inbound evidence was a
    /// name like `new` reports `0` here and does not rank as if it were called.
    pub in_degree: u32,
    /// Distinct files holding a definition this one references, under the same
    /// filter as [`FileSymbols::in_degree`].
    pub out_degree: u32,
    /// Distinct files whose `include`, `use` or `import` resolves to this file.
    ///
    /// Unlike [`FileSymbols::in_degree`] this is not inferred from names: every
    /// edge here was resolved to a real file path by
    /// [`resolve_dependencies`], so it is the "certain dependency" count — the
    /// file imports this one, rather than mentioning a name this one also uses.
    pub dep_in_degree: u32,
    pub symbols: Vec<SymbolEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct BuildStats {
    /// Files handed to the builder: everything the worktree lists except paths
    /// excluded as vendored or generated.
    pub considered: usize,
    /// Files actually parsed and kept.
    pub parsed: usize,
    /// Files skipped because they were too large.
    pub too_large: usize,
    /// Files skipped because no language (or no grammar) matched.
    pub unsupported: usize,
    /// Files that were selected but could not be read from disk.
    pub unreadable: usize,
    /// Files skipped because the remaining byte budget could not hold them.
    pub over_budget: usize,
    /// Files skipped because [`MAX_FILES`] was already reached.
    pub file_limit: usize,
    /// Files excluded as vendored or generated before any budget applied.
    pub vendored_skipped: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolMap {
    pub files: Vec<FileSymbols>,
    pub stats: BuildStats,
}

/// Files a first-turn digest lists.
pub const DEFAULT_DIGEST_FILES: usize = 20;
/// Symbols a digest lists per file.
pub const DEFAULT_DIGEST_SYMBOLS_PER_FILE: usize = 5;
/// Character budget for a first-turn digest.
///
/// At the usual four characters per token this is about 900 tokens, the top of
/// the 600–900 token range a digest may cost before an agent has asked for
/// anything. The full map is ten times that (40k characters ≈ 10k tokens).
pub const MAX_DIGEST_CHARS: usize = 3_600;

impl SymbolMap {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Files that carry at least one definition, best-ranked first.
    pub fn ranked(&self) -> impl Iterator<Item = &FileSymbols> {
        self.files.iter().filter(|file| !file.symbols.is_empty())
    }

    /// How the files are ordered, as stated in every rendering so an agent can
    /// tell why the first file is the first file.
    pub fn order_description() -> &'static str {
        "Order: referencing fan-in (desc), then IDF-weighted reference PageRank (desc), \
         then path. `in N` counts distinct files that reference a definition this file owns, \
         ignoring names shared by more than 20 files; `out M` counts the files this one \
         references under the same rule; `deps N` counts the files whose `mod`, `use`, \
         `#include` or relative `import` resolves to this one, so it is a certain \
         dependency rather than a shared name."
    }

    /// Renders the map as markdown for a chat prompt.
    ///
    /// `max_chars` bounds the result so a huge repository cannot blow up the
    /// composer; files are added whole until the budget runs out.
    pub fn to_markdown(&self, max_chars: usize) -> String {
        self.repo_map_markdown(&RepoMapOptions::default(), max_chars)
    }

    /// Renders the first-turn digest: the best-ranked files with a handful of
    /// their definitions each, plus one line saying where the full map lives.
    ///
    /// This is what a fresh conversation can afford to carry, so every entry is
    /// chosen rather than complete: `max_files` bounds the files,
    /// `symbols_per_file` the definitions inside one file (top-level ones
    /// before methods and fields, never locals), and `max_chars` the whole
    /// result. Nothing disappears silently: the files that do not fit are
    /// counted in a last line that points at `repo_map`.
    pub fn to_digest(&self, max_files: usize, symbols_per_file: usize, max_chars: usize) -> String {
        let files: Vec<&FileSymbols> = self.ranked().collect();
        let total = files.len();
        if total == 0 {
            return String::from(
                "这个项目还没有可用的仓库地图（请先在 WuIDE 里打开一次 Repository Map 面板）",
            );
        }

        let shown = max_files.min(total);
        let header = format!(
            "仓库地图摘要（按被引用程度排序，共 {total} 个文件）：这里列出前 {shown} 个，\
             每个最多 {symbols_per_file} 个符号；需要完整或更新内容时调用 `repo_map` 或 `symbols` 工具。"
        );

        // The omitted-files line is reserved before the body is rendered, so
        // adding it never pushes the digest past `max_chars`. The reserve is
        // measured against `total`, the widest count that line can carry.
        let reserve = omitted_note(total).chars().count();
        let budget = max_chars.saturating_sub(header.chars().count() + reserve);
        let (body, written) = render_ranked(&files, 0, shown, budget, |file| {
            digest_line(file, symbols_per_file)
        });

        let mut out = header;
        out.push('\n');
        out.push_str(&body);
        if written < total {
            out.push_str(&omitted_note(total - written));
        }
        out
    }

    /// Renders the paged, top-level-first repository map.
    ///
    /// `limit` and `offset` page over the files that define something; it is an
    /// upper bound, because the character budget can end the page earlier, and
    /// the header then says the budget did it. The budget covers the whole
    /// page — header, notes, body and the list of files left out — because a
    /// list of every omitted path can cost more than the body it describes.
    /// What still does not fit is summarized in one line.
    pub fn repo_map_markdown(&self, options: &RepoMapOptions, max_chars: usize) -> String {
        let files: Vec<&FileSymbols> = self.ranked().collect();
        let total = files.len();
        let start = options.offset.min(total);
        let end = start.saturating_add(options.limit).min(total);

        if total == 0 {
            return String::from(
                "## Repository map\n\nNo definitions were found in this repository.\n",
            );
        }

        // The body is measured against what the framing and the omitted list
        // leave behind, so shrinking the body never overshoots the budget.
        let mut body_budget = max_chars;
        let mut attempts = 0;
        let (head, body, written, reserve_len) = loop {
            let (body, written) = render_ranked(&files, start, end, body_budget, |file| {
                self.file_block(file, options)
            });
            let head = self.page_head(written, start, end, total, options.offset);
            let reserve_len = self
                .page_tail_reserve(written, start, end, total, options)
                .chars()
                .count();
            let used = head.chars().count() + body.chars().count() + reserve_len;
            // A page always keeps its first file, so a budget too small for the
            // framing alone cannot be satisfied and is left as best effort.
            if used <= max_chars || written <= start + 1 || attempts >= 8 {
                break (head, body, written, reserve_len);
            }
            body_budget = body_budget.saturating_sub((used - max_chars).max(1));
            attempts += 1;
        };

        let mut out = head;
        out.push_str(&body);
        // The whole tail is reserved before any path is printed, so the list can
        // only use what the header, the body and the trailing notes leave.
        let mut room = max_chars.saturating_sub(out.chars().count() + reserve_len);
        let skipped = total.saturating_sub(written);
        if skipped > 0 {
            out.push_str(&omitted_files_header(skipped));
            // Show whole paths while they fit, never more than
            // `omitted_path_limit`, and name the rest in one line.
            let mut shown = 0;
            for file in files[written..]
                .iter()
                .take(omitted_path_limit(options.limit))
            {
                let line = format!("{}\n", file.rel.display(path::PathStyle::Unix));
                let cost = line.chars().count();
                if cost > room {
                    break;
                }
                room -= cost;
                out.push_str(&line);
                shown += 1;
            }
            if shown < skipped {
                out.push_str(&more_omitted_line(skipped - shown));
            }
        }
        if written > start && written == end {
            out.push_str(&next_page_line(written, options.limit));
        }
        if options.include_locals {
            out.push_str("\nIncludes local variables.\n");
        }
        out
    }

    /// Everything before the body: the header, the ordering note and the count
    /// of files the scan could not index.
    ///
    /// `offset` is the caller's, unsatuated: an offset at or past `total` would
    /// otherwise print `showing 1515–1514 of 1514 files`, a range that runs
    /// backwards. That page is empty, so it says so and names the largest offset
    /// that is not.
    fn page_head(
        &self,
        written: usize,
        start: usize,
        end: usize,
        total: usize,
        offset: usize,
    ) -> String {
        let mut header = if start >= total {
            format!(
                "showing 0 of {total} files (offset {offset} is past the end; \
                 use an offset below {total})"
            )
        } else {
            format!(
                "showing {}–{written} of {total} files with definitions",
                start + 1
            )
        };
        // Only the budget needs an explanation: a page cut by `limit` is the
        // page the caller asked for, and the paging hint below covers it.
        if written < end {
            header.push_str(&format!(
                " (output budget reached; use offset={written} for the next page, or call \
                 symbols(path) for one file)"
            ));
        }
        let mut head = format!(
            "## Repository map\n\n{header}.\n{}\n",
            Self::order_description()
        );
        for note in self.coverage_notes(total) {
            head.push_str(&note);
            head.push('\n');
        }
        head
    }

    /// Everything after the body, with the widest "+N more omitted" line its
    /// `skipped` count can produce, so the body budget never eats that room.
    fn page_tail_reserve(
        &self,
        written: usize,
        start: usize,
        end: usize,
        total: usize,
        options: &RepoMapOptions,
    ) -> String {
        let mut tail = String::new();
        let skipped = total.saturating_sub(written);
        if skipped > 0 {
            tail.push_str(&omitted_files_header(skipped));
            tail.push_str(&more_omitted_line(skipped));
        }
        if written > start && written == end {
            tail.push_str(&next_page_line(written, options.limit));
        }
        if options.include_locals {
            tail.push_str("\nIncludes local variables.\n");
        }
        tail
    }

    /// Honest coverage for the whole build: how many candidates became map
    /// entries, why the rest did not, and what never became a candidate at all.
    ///
    /// A page that lists 40 files out of thousands would otherwise look
    /// complete. The parsed count and the number of files with definitions are
    /// different measures; naming both keeps the coverage line from looking
    /// like it contradicts the page's "files with definitions" total.
    fn coverage_notes(&self, total: usize) -> Vec<String> {
        let stats = &self.stats;
        let mut notes = Vec::new();
        if stats.considered > 0 {
            let mut reasons = Vec::new();
            if stats.unsupported > 0 {
                reasons.push(format!("{} unsupported language", stats.unsupported));
            }
            if stats.too_large > 0 {
                reasons.push(format!(
                    "{} over {} KiB",
                    stats.too_large,
                    MAX_FILE_BYTES / 1024
                ));
            }
            if stats.over_budget > 0 {
                reasons.push(format!("{} over the byte budget", stats.over_budget));
            }
            if stats.file_limit > 0 {
                reasons.push(format!(
                    "{} beyond the {MAX_FILES}-file limit",
                    stats.file_limit
                ));
            }
            if stats.unreadable > 0 {
                reasons.push(format!("{} unreadable", stats.unreadable));
            }
            let reasons = if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join(", "))
            };
            notes.push(format!(
                "Parsed {} / {} candidate files{reasons}; {} with definitions.",
                stats.parsed, stats.considered, total
            ));
        }
        let mut excluded = Vec::new();
        if stats.vendored_skipped > 0 {
            excluded.push(format!(
                "{} vendored or generated paths",
                stats.vendored_skipped
            ));
        }
        if !excluded.is_empty() {
            notes.push(format!(
                "Excluded before scanning: {}.",
                excluded.join(", ")
            ));
        }
        notes
    }

    fn file_block(&self, file: &FileSymbols, options: &RepoMapOptions) -> String {
        let mut block = format!(
            "\n`{}` (in {} / out {} / deps {}, {} lines)\n",
            file.rel.display(path::PathStyle::Unix),
            file.in_degree,
            file.out_degree,
            file.dep_in_degree,
            file.lines
        );
        if options.files_only {
            return block;
        }
        let visible: Vec<SymbolEntry> = file
            .symbols
            .iter()
            .filter(|symbol| options.include_locals || !symbol.local)
            .cloned()
            .collect();
        let kept = visible.len().min(MAX_SYMBOLS_PER_FILE_IN_MARKDOWN);
        // Locals were filtered above; the tree renderer must not filter again.
        for line in symbol_tree_lines(&visible[..kept], true) {
            block.push_str(&line);
            block.push('\n');
        }
        if kept < visible.len() {
            block.push_str(&format!("… +{} more symbols\n", visible.len() - kept));
        }
        block
    }

    /// The map as JSON for the clipboard export and the on-disk snapshot: one
    /// entry per file that has definitions, best-ranked first, with one-based
    /// line numbers and the same field names the snapshot uses.
    pub fn json_entries(&self) -> Vec<serde_json::Value> {
        self.ranked().map(file_json).collect()
    }
}

/// Renders whole file blocks in rank order until `max_chars` runs out.
///
/// Shared by the full map and the digest: both walk the ranked files and both
/// keep the first block even when it alone exceeds the budget, so a tiny budget
/// still names the best-ranked file instead of returning nothing. Returns the
/// body and the absolute index just past the last file it holds.
fn render_ranked(
    files: &[&FileSymbols],
    start: usize,
    end: usize,
    max_chars: usize,
    mut render: impl FnMut(&FileSymbols) -> String,
) -> (String, usize) {
    let mut body = String::new();
    let mut budget = max_chars;
    let mut written = start;
    for file in &files[start..end] {
        let block = render(file);
        let cost = block.chars().count();
        if cost > budget && written > start {
            break;
        }
        budget = budget.saturating_sub(cost);
        body.push_str(&block);
        written += 1;
    }
    (body, written)
}

/// The line that opens the list of files a page left out.
fn omitted_files_header(skipped: usize) -> String {
    format!("\nOmitted files ({skipped}, paths only):\n")
}

/// The line that closes a page when even the omitted paths did not all fit.
fn more_omitted_line(remaining: usize) -> String {
    format!("… +{remaining} more omitted (call files_only or raise offset to list them)\n")
}

/// The paging hint for a page that `limit`, not the budget, ended.
fn next_page_line(written: usize, limit: usize) -> String {
    format!("\nNext page: offset={written}, limit={limit}.\n")
}

/// The line a digest ends with when something was left out.
fn omitted_note(omitted: usize) -> String {
    format!("…另有 {omitted} 个文件，调用 `repo_map` 查看\n")
}

/// One digest line: the file, then its chosen definitions.
fn digest_line(file: &FileSymbols, symbols_per_file: usize) -> String {
    let mut line = format!("`{}`", file.rel.display(path::PathStyle::Unix));
    let symbols: Vec<String> = digest_symbols(&file.symbols, symbols_per_file)
        .into_iter()
        .map(|symbol| format!("{} {} ({})", symbol.kind, symbol.name, symbol.row + 1))
        .collect();
    if !symbols.is_empty() {
        line.push_str(" — ");
        line.push_str(&symbols.join("、"));
    }
    line.push('\n');
    line
}

/// The definitions a digest shows for one file.
///
/// Locals bury the structure of a real C and C++ file, so they never appear
/// here. Top-level definitions come first, methods and fields after them, and
/// each group keeps the file's own line order.
fn digest_symbols(symbols: &[SymbolEntry], limit: usize) -> Vec<&SymbolEntry> {
    let mut chosen: Vec<&SymbolEntry> = symbols.iter().filter(|symbol| !symbol.local).collect();
    chosen.sort_by_key(|symbol| digest_tier(&symbol.kind));
    chosen.truncate(limit);
    chosen
}

/// Whether a definition describes the file rather than a detail inside one
/// type. Kinds are already normalized by [`classify_kind`].
fn digest_tier(kind: &str) -> u8 {
    match kind {
        "function" | "class" | "struct" | "enum" | "macro" | "constant" | "trait" | "interface"
        | "module" | "type" => 0,
        _ => 1,
    }
}

/// What a rendering of the map includes, mirrored by the bundled MCP server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoMapOptions {
    pub limit: usize,
    pub offset: usize,
    pub files_only: bool,
    pub include_locals: bool,
}

impl Default for RepoMapOptions {
    fn default() -> Self {
        Self {
            limit: 40,
            offset: 0,
            files_only: false,
            include_locals: false,
        }
    }
}

/// One file as the snapshot and the clipboard export describe it.
pub fn file_json(file: &FileSymbols) -> serde_json::Value {
    serde_json::json!({
        "path": file.rel.display(path::PathStyle::Unix),
        "rank": (file.rank * 10_000.0).round() / 10_000.0,
        "size": file.size,
        "lines": file.lines,
        "in": file.in_degree,
        "out": file.out_degree,
        "deps": file.dep_in_degree,
        "symbols": file.symbols.iter().map(symbol_json).collect::<Vec<_>>(),
    })
}

pub fn symbol_json(symbol: &SymbolEntry) -> serde_json::Value {
    serde_json::json!({
        "name": symbol.name,
        "line": symbol.row + 1,
        "kind": symbol.kind,
        "owner": symbol.owner,
        "depth": symbol.depth,
        "access": symbol.access,
        "signature": symbol.signature,
        "local": symbol.local,
    })
}

/// One rendered line of a file's symbol tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolLine {
    /// A C++ access label such as `public:`.
    Access {
        depth: u32,
        access: Arc<str>,
    },
    Symbol(SymbolEntry),
    /// A run of variables summarized as `variable x5 (lines 7–19)`.
    Folded {
        depth: u32,
        kind: Arc<str>,
        count: usize,
        first_row: u32,
        last_row: u32,
    },
}

/// Whether a symbol is the kind that folds into a "variable xN" summary.
///
/// Only variables fold: a struct's fields are its layout, and collapsing them
/// would hide the names a reader needs.
fn folds_when_repeated(kind: &str) -> bool {
    kind == "variable"
}

/// Groups a file's symbols into the lines the panel and the agent both read:
/// locals are dropped unless `include_locals` is set, members of a type sit
/// under their C++ access label, and a run of three or more consecutive
/// variables in one scope becomes a single summary line, so a struct with
/// twenty fields does not bury the methods around it.
pub fn symbol_lines(symbols: &[SymbolEntry], include_locals: bool) -> Vec<SymbolLine> {
    let visible: Vec<&SymbolEntry> = symbols
        .iter()
        .filter(|symbol| include_locals || !symbol.local)
        .collect();
    let mut lines = Vec::with_capacity(visible.len());
    let mut last_access: Option<(Option<Arc<str>>, Option<Arc<str>>, u32)> = None;
    let mut index = 0;
    while index < visible.len() {
        let symbol = visible[index];
        let access_key = (symbol.owner.clone(), symbol.access.clone(), symbol.depth);
        if symbol.access.is_some() && last_access.as_ref() != Some(&access_key) {
            if let Some(access) = &symbol.access {
                lines.push(SymbolLine::Access {
                    depth: symbol.depth.saturating_sub(1),
                    access: access.clone(),
                });
            }
        }
        last_access = Some(access_key);

        let mut run_end = index + 1;
        if folds_when_repeated(&symbol.kind) {
            while run_end < visible.len() {
                let next = visible[run_end];
                if next.kind != symbol.kind
                    || next.owner != symbol.owner
                    || next.access != symbol.access
                    || next.local != symbol.local
                {
                    break;
                }
                run_end += 1;
            }
        }
        if run_end - index >= 3 {
            lines.push(SymbolLine::Folded {
                depth: symbol.depth,
                kind: symbol.kind.clone(),
                count: run_end - index,
                first_row: symbol.row,
                last_row: visible[run_end - 1].row,
            });
        } else {
            for symbol in &visible[index..run_end] {
                lines.push(SymbolLine::Symbol((*symbol).clone()));
            }
        }
        index = run_end;
    }
    lines
}

/// Renders [`symbol_lines`] as markdown bullets.
pub fn symbol_tree_lines(symbols: &[SymbolEntry], include_locals: bool) -> Vec<String> {
    symbol_lines(symbols, include_locals)
        .into_iter()
        .map(|line| match line {
            SymbolLine::Access { depth, access } => {
                format!("{}- `{access}:`", "  ".repeat(depth as usize))
            }
            SymbolLine::Symbol(symbol) => {
                format!("{}- {symbol}", "  ".repeat(symbol.depth as usize))
            }
            SymbolLine::Folded {
                depth,
                kind,
                count,
                first_row,
                last_row,
            } => format!(
                "{}- {kind} x{count} (lines {}–{})",
                "  ".repeat(depth as usize),
                first_row + 1,
                last_row + 1
            ),
        })
        .collect()
}

impl std::fmt::Display for SymbolEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.kind, self.name)?;
        if let Some(signature) = &self.signature {
            write!(formatter, "{signature}")?;
        }
        write!(formatter, " (line {})", self.row + 1)
    }
}

/// One file to consider, gathered from a worktree snapshot on the foreground
/// thread so the background job needs no project access.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub rel: Arc<RelPath>,
    pub abs_path: PathBuf,
    pub size: u64,
    /// The worktree snapshot's modification time in nanoseconds since the Unix
    /// epoch, or `None` when the snapshot has none.
    ///
    /// The cache fingerprints files with this rather than a fresh `stat`: the
    /// snapshot is maintained by the worktree watcher, so it is exactly as
    /// fresh as the file list the panel draws from, and reading it costs no
    /// syscall. A stat of this repository's two thousand sources measured
    /// about 0.3 s on Windows, which would be most of a cache hit's budget.
    pub modified_ns: Option<i64>,
}

/// The snapshot's mtime for one file, in the units [`SourceFile`] carries.
pub fn modified_nanos(mtime: Option<fs::MTime>) -> Option<i64> {
    let (seconds, nanos) = mtime?.to_seconds_and_nanos_for_persistence()?;
    (seconds as i64)
        .checked_mul(1_000_000_000)
        .and_then(|nanos_since_epoch| nanos_since_epoch.checked_add(nanos as i64))
}

/// Applies the vendored, size, count and byte budgets to a candidate list.
///
/// Candidates are bucketed by their first two path components (`crates/gpui`
/// and `crates/editor` are different buckets), so a monorepo whose code all
/// lives under `crates/` still gets every crate represented.
///
/// The byte budget is split evenly between the buckets instead of being spent
/// in the order the caller happened to hand files over: the panel walks the
/// heaviest directory first, another caller may walk in path order, and the map
/// must not depend on that. Each round offers every bucket with work left an
/// equal share of what remains, so a bucket that finishes early or cannot fill
/// its share returns it to the others. Whatever the shares cannot place is then
/// offered to every remaining file in path order, skipping the ones that no
/// longer fit, so a small file behind a too-large one still gets in.
///
/// Every candidate ends up in exactly one category, which is what makes the
/// coverage line honest.
fn select(
    mut sources: Vec<SourceFile>,
    is_source: impl Fn(&SourceFile) -> bool,
) -> (Vec<SourceFile>, BuildStats) {
    // The caller's order is an implementation detail of its tree walk. Sorting
    // here makes the same worktree produce the same map for every caller.
    sources.sort_by(|left, right| {
        left.rel
            .cmp(&right.rel)
            .then_with(|| left.abs_path.cmp(&right.abs_path))
    });

    let mut stats = BuildStats::default();
    let mut buckets: Vec<(String, Vec<SourceFile>)> = Vec::new();
    let mut bucket_index: HashMap<String, usize> = HashMap::new();
    for source in sources {
        if is_vendored_path(&source.rel) {
            stats.vendored_skipped += 1;
            continue;
        }
        stats.considered += 1;
        if !is_source(&source) {
            stats.unsupported += 1;
            continue;
        }
        if source.size > MAX_FILE_BYTES {
            stats.too_large += 1;
            continue;
        }
        let key = bucket_key(&source.rel);
        let bucket = match bucket_index.get(&key) {
            Some(index) => *index,
            None => {
                buckets.push((key.clone(), Vec::new()));
                let index = buckets.len() - 1;
                bucket_index.insert(key, index);
                index
            }
        };
        buckets[bucket].1.push(source);
    }

    let mut chosen = Vec::with_capacity(buckets.iter().map(|(_, files)| files.len()).sum());
    let mut taken = vec![0usize; buckets.len()];
    let mut picked = vec![0usize; buckets.len()];
    let mut total_bytes = 0u64;
    loop {
        let pending: Vec<usize> = (0..buckets.len())
            .filter(|index| taken[*index] < buckets[*index].1.len())
            .collect();
        if pending.is_empty() || chosen.len() >= MAX_FILES {
            break;
        }
        let remaining = MAX_TOTAL_BYTES.saturating_sub(total_bytes);
        if remaining == 0 {
            break;
        }
        let share = remaining / pending.len() as u64;
        let mut progressed = false;
        for index in pending {
            let mut budget = share;
            let (_, bucket) = &buckets[index];
            while taken[index] < bucket.len() && chosen.len() < MAX_FILES {
                let source = &bucket[taken[index]];
                if source.size > budget {
                    break;
                }
                budget -= source.size;
                total_bytes += source.size;
                chosen.push(source.clone());
                taken[index] += 1;
                picked[index] += 1;
                progressed = true;
            }
        }
        if progressed {
            continue;
        }
        break;
    }

    // Spend what the shares could not. The budget only shrinks from here, so a
    // file that does not fit now can never be taken and is skipped for good;
    // this is what lets a small file behind a too-large one still get in.
    loop {
        let mut added = false;
        for index in 0..buckets.len() {
            let bucket = &buckets[index].1;
            while taken[index] < bucket.len() && chosen.len() < MAX_FILES {
                let source = &bucket[taken[index]];
                if source.size > MAX_TOTAL_BYTES.saturating_sub(total_bytes) {
                    taken[index] += 1;
                    continue;
                }
                total_bytes += source.size;
                chosen.push(source.clone());
                taken[index] += 1;
                picked[index] += 1;
                added = true;
            }
        }
        if !added || chosen.len() >= MAX_FILES {
            break;
        }
    }

    let file_limit_reached = chosen.len() >= MAX_FILES;
    for (index, (_, bucket)) in buckets.iter().enumerate() {
        let skipped = bucket.len() - picked[index];
        if file_limit_reached {
            stats.file_limit += skipped;
        } else {
            stats.over_budget += skipped;
        }
    }

    (chosen, stats)
}

/// The bucket a file belongs to: its first two path components, or the first
/// one when the file sits directly in a top-level directory. Files at the
/// repository root share one bucket.
fn bucket_key(rel: &RelPath) -> String {
    let mut components = rel.components();
    let Some(first) = components.next() else {
        return String::new();
    };
    let Some(second) = components.next() else {
        return String::new();
    };
    if components.next().is_some() {
        format!("{first}/{second}")
    } else {
        first.to_string()
    }
}

/// Whether any directory on the path is vendored, generated, or hidden.
///
/// The file's own name is left out, so a hidden file such as `.gitignore` is
/// still judged by its language rather than by its name.
fn is_vendored_path(rel: &RelPath) -> bool {
    let count = rel.components().count();
    rel.components()
        .take(count.saturating_sub(1))
        .any(|component| component.starts_with('.') || VENDORED_DIRS.contains(&component))
}

/// What one build produced, and how much of it was already known.
pub struct BuildOutcome {
    pub map: Arc<SymbolMap>,
    /// When this map was built, which is earlier than now when the in-memory
    /// cache answered. Callers show it so a reused map says how old it is
    /// instead of claiming to be fresh.
    pub built_at: DateTime<Utc>,
    /// Files parsed in this build.
    pub parsed: usize,
    /// Files whose cached parse was reused.
    pub reused: usize,
    /// Whether the whole map came from memory without reading any file.
    pub from_memory: bool,
}

/// Parses `sources` and ranks them, reading and parsing every file.
///
/// This is the uncached path; [`build_cached`] is what the panel and the chat
/// call. It exists so a test can prove the cache changes nothing about the map.
///
/// Must run off the foreground thread.
pub async fn build(
    sources: Vec<SourceFile>,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
) -> SymbolMap {
    let (kept, stats) = select(sources, |source| {
        languages.language_for_file_path(&source.abs_path).is_some()
    });
    let stamps = stamp_index(&kept);
    let parsed = parse_sources(kept, stats, fs, languages, None, &stamps).await;
    finish(parsed.files, parsed.stats)
}

/// Parses `sources` and ranks them, reusing what an earlier build of the same
/// map already learned.
///
/// `roots` names the map: the worktree roots whose files it covers. A map over
/// several worktrees is not the map of any one of them, so the caller has to
/// say which it wants. The in-memory map is reused only when every source still
/// has the stamp it had then, and the per-file parses in `cache_dir` are reused
/// file by file, so an edit costs one parse instead of a whole rebuild.
///
/// Must run off the foreground thread.
pub async fn build_cached(
    sources: Vec<SourceFile>,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
    roots: &[PathBuf],
    cache_dir: &Path,
) -> BuildOutcome {
    let (kept, stats) = select(sources, |source| {
        languages.language_for_file_path(&source.abs_path).is_some()
    });
    let stamps = stamp_index(&kept);
    let key = symbol_cache::key(roots);

    // Comparing every source's stamp against the filesystem is what makes a
    // stale map impossible to hand out: an edit, a deletion, an addition and a
    // branch switch all change some file's stat, and cost one stat each.
    if let Some((map, built_at)) = symbol_cache::reuse(&key, &stamps.stamps) {
        let reused = map.files.len();
        log::info!("repo map: {reused} files reused from memory, built at {built_at}");
        return BuildOutcome {
            map,
            built_at,
            parsed: 0,
            reused,
            from_memory: true,
        };
    }

    let cached = symbol_cache::load::<CachedParse>(cache_dir, &key);
    let cached_files = cached.as_ref().map_or(0, Vec::len);
    // Keyed by absolute path: two worktrees of one project can hold the same
    // relative path, and their parses must not be confused for one another.
    let by_path: HashMap<String, CacheEntry<CachedParse>> = cached
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.payload.path.clone(), entry))
        .collect();

    let parsed = parse_sources(kept, stats, fs, languages, Some(&by_path), &stamps).await;
    // Rewriting the document is only worth its bytes when a file changed: an
    // unchanged rebuild keeps the one already on disk, untouched.
    if parsed.parsed > 0 || cached_files != parsed.files.len() {
        let entries: Vec<CacheEntry<CachedParseRef<'_>>> = parsed
            .files
            .iter()
            .filter_map(|file| {
                // Only a file the snapshot can vouch for is worth storing; one
                // without a time could never be validated again.
                let (file_size, modified_ns) = stamps.by_path.get(&file.path).copied()?;
                Some(CacheEntry {
                    file_size,
                    modified_ns,
                    payload: CachedParseRef::from(file),
                })
            })
            .collect();
        let project = roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        symbol_cache::store(cache_dir, &key, &project, &entries);
    }

    let built_at = Utc::now();
    log::info!(
        "repo map: {} files reused, {} parsed, built at {built_at}",
        parsed.reused,
        parsed.parsed
    );
    let map = Arc::new(finish(parsed.files, parsed.stats));
    symbol_cache::remember(&key, map.clone(), built_at, stamps.stamps);
    BuildOutcome {
        map,
        built_at,
        parsed: parsed.parsed,
        reused: parsed.reused,
        from_memory: false,
    }
}

/// Drops the remembered map covering `roots`, so the next build re-reads the
/// filesystem. A manual refresh asks for a fresh look at the files, not for
/// every one of them to be parsed again, so the disk cache stays.
pub fn forget_cached_map(roots: &[PathBuf]) {
    symbol_cache::forget(&symbol_cache::key(roots));
}

/// What one parse pass produced: the files the map holds, in source order.
struct ParsedSources {
    files: Vec<ParsedFile>,
    stats: BuildStats,
    /// Files parsed now.
    parsed: usize,
    /// Files whose parse came from the cache.
    reused: usize,
}

/// Every source's stat, taken once per build.
struct StampIndex {
    /// Ordered by path, so two indexes of one file set compare equal.
    stamps: Vec<FileStamp>,
    by_path: HashMap<String, (u64, i64)>,
}

fn stamp_index(sources: &[SourceFile]) -> StampIndex {
    let mut stamps: Vec<FileStamp> = sources
        .iter()
        .filter_map(|source| {
            // A file the snapshot has no time for cannot be validated from the
            // cache, so it is left out of the index: it is parsed every build
            // and never stored.
            let modified_ns = source.modified_ns?;
            Some(FileStamp {
                path: absolute_key(&source.abs_path),
                file_size: source.size,
                modified_ns,
            })
        })
        .collect();
    stamps.sort_by(|left, right| left.path.cmp(&right.path));
    let by_path = stamps
        .iter()
        .map(|stamp| (stamp.path.clone(), (stamp.file_size, stamp.modified_ns)))
        .collect();
    StampIndex { stamps, by_path }
}

/// A file's identity for the cache: its absolute path, in one spelling for
/// every platform, so a document written on Windows is still found there.
fn absolute_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Reads and parses `kept`, taking each file's parse from `cached` when its
/// stat still matches. Must run off the foreground thread.
async fn parse_sources(
    kept: Vec<SourceFile>,
    mut stats: BuildStats,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
    cached: Option<&HashMap<String, CacheEntry<CachedParse>>>,
    stamps: &StampIndex,
) -> ParsedSources {
    let mut parsed: Vec<ParsedFile> = Vec::with_capacity(kept.len());
    let mut reused = 0usize;
    let mut fresh = 0usize;
    for source in kept {
        // Yielding between files lets the task be dropped promptly when the
        // panel closes or a newer build starts.
        futures::future::ready(()).await;
        let path = absolute_key(&source.abs_path);
        let (file_size, modified_ns) = stamps.by_path.get(&path).copied().unwrap_or_default();
        if let Some(entry) = cached.and_then(|cached| cached.get(&path))
            && entry.file_size == file_size
            && entry.modified_ns == modified_ns
        {
            parsed.push(entry.payload.to_parsed(source.rel.clone(), source.size));
            reused += 1;
            continue;
        }
        // Unreadable files are common (vendored blobs, binaries, non-UTF-8
        // sources such as coq's SharedStack) and say nothing about the
        // project, so they stay at debug level.
        let text = match fs.load(&source.abs_path).await {
            Ok(text) => text,
            Err(error) => {
                log::debug!(
                    "repo map: skipping {}: {error:#}",
                    source.rel.display(path::PathStyle::Unix)
                );
                stats.unreadable += 1;
                continue;
            }
        };
        let language = match languages
            .load_language_for_file_path(&source.abs_path)
            .await
        {
            Ok(language) => language,
            Err(error) => {
                log::debug!(
                    "repo map: skipping {}: {error:#}",
                    source.rel.display(path::PathStyle::Unix)
                );
                stats.unsupported += 1;
                continue;
            }
        };
        // Without a grammar and an outline query there is nothing to collect.
        if !has_outline_query(&language) {
            stats.unsupported += 1;
            continue;
        }
        let Some(file) = parse_file(&source, &text, &language) else {
            stats.unsupported += 1;
            continue;
        };
        parsed.push(file);
        fresh += 1;
    }

    ParsedSources {
        files: parsed,
        stats,
        parsed: fresh,
        reused,
    }
}

/// Ranks the parsed files and turns them into the map's public shape.
fn finish(mut parsed: Vec<ParsedFile>, mut stats: BuildStats) -> SymbolMap {
    stats.parsed = parsed.len();
    resolve_dependencies(&mut parsed);
    rank(&mut parsed);
    parsed.sort_by(|a, b| {
        b.in_degree
            .cmp(&a.in_degree)
            .then_with(|| b.rank.total_cmp(&a.rank))
            .then_with(|| a.rel.as_std_path().cmp(b.rel.as_std_path()))
    });

    SymbolMap {
        files: parsed
            .into_iter()
            .map(|file| FileSymbols {
                rel: file.rel,
                size: file.size,
                rank: file.rank,
                lines: file.lines,
                in_degree: file.in_degree,
                out_degree: file.out_degree,
                dep_in_degree: file.dep_in_degree,
                symbols: file.symbols,
            })
            .collect(),
        stats,
    }
}

#[derive(Clone, Debug)]
struct ParsedFile {
    rel: Arc<RelPath>,
    /// Absolute path, in the same spelling the cache uses as its key.
    path: String,
    size: u64,
    lines: u32,
    rank: f32,
    in_degree: u32,
    out_degree: u32,
    symbols: Vec<SymbolEntry>,
    /// Identifiers mentioned anywhere in the file, used to build the
    /// reference graph.
    mentions: HashSet<Arc<str>>,
    /// Language group whose identifiers can name this file's definitions, from
    /// [`language_family`].
    family: Arc<str>,
    /// Import specs the file declares, tagged with their kind (`mod:`, `use:`,
    /// `include:`, `import:`). Resolved to real paths by
    /// [`resolve_dependencies`].
    includes: Vec<String>,
    /// Distinct files whose import spec resolves to this one.
    dep_in_degree: u32,
}

/// One file's parse as the cache document holds it.
///
/// The path is a string because the document is read back without the project:
/// it is what tells two worktrees' copies of `src/lib.rs` apart. The fields
/// the ranking computes (`rank`, the degrees) are not stored, because they
/// depend on every other file in the map and are recomputed on each build.
#[derive(Deserialize)]
struct CachedParse {
    path: String,
    lines: u32,
    symbols: Vec<SymbolEntry>,
    #[serde(default)]
    mentions: Vec<Arc<str>>,
    /// The group whose identifiers can name this file's definitions, from
    /// [`language_family`]. Stored rather than derived so a cache document is
    /// self-describing; changing the grouping means bumping the cache version.
    #[serde(default)]
    family: String,
    #[serde(default)]
    includes: Vec<String>,
}

impl CachedParse {
    /// The parsed file, with the identity and size the current build knows.
    ///
    /// The size comes from the source list rather than the document so a
    /// reused file reports exactly what an uncached build would report, even
    /// when the worktree snapshot is a moment behind the disk.
    fn to_parsed(&self, rel: Arc<RelPath>, size: u64) -> ParsedFile {
        ParsedFile {
            rel,
            path: self.path.clone(),
            size,
            lines: self.lines,
            rank: 0.,
            in_degree: 0,
            out_degree: 0,
            symbols: self.symbols.clone(),
            mentions: self.mentions.iter().cloned().collect(),
            family: self.family.as_str().into(),
            includes: self.includes.clone(),
            dep_in_degree: 0,
        }
    }
}

/// A [`ParsedFile`] as it is written to the cache document.
///
/// Borrowed rather than copied so a build writes what it just parsed without
/// duplicating the symbols first. Mentions are sorted, which keeps the document
/// byte-for-byte stable across builds of the same content.
#[derive(Serialize)]
struct CachedParseRef<'a> {
    path: &'a str,
    lines: u32,
    symbols: &'a [SymbolEntry],
    mentions: Vec<&'a str>,
    family: &'a str,
    includes: &'a [String],
}

impl<'a> From<&'a ParsedFile> for CachedParseRef<'a> {
    fn from(file: &'a ParsedFile) -> Self {
        let mut mentions: Vec<&str> = file.mentions.iter().map(|mention| &**mention).collect();
        mentions.sort_unstable();
        Self {
            path: &file.path,
            lines: file.lines,
            symbols: &file.symbols,
            mentions,
            family: &file.family,
            includes: &file.includes,
        }
    }
}

/// Whether the language has both a grammar and the outline query the symbol
/// list is built from.
fn has_outline_query(language: &Language) -> bool {
    language
        .grammar()
        .is_some_and(|grammar| grammar.outline_config.is_some())
}

fn parse_file(source: &SourceFile, text: &str, language: &Arc<Language>) -> Option<ParsedFile> {
    let grammar = language.grammar()?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar.ts_language).ok()?;
    let tree = parser.parse(text, None)?;

    let symbols = collect_symbols(grammar.outline_config.as_ref(), tree.root_node(), text);
    let family: Arc<str> = language_family(language.name().as_ref()).into();
    let (mentions, includes) = collect_references(tree.root_node(), text, &family);
    Some(ParsedFile {
        rel: source.rel.clone(),
        path: absolute_key(&source.abs_path),
        size: source.size,
        lines: text.lines().count() as u32,
        rank: 0.,
        in_degree: 0,
        out_degree: 0,
        symbols,
        mentions,
        family,
        includes,
        dep_in_degree: 0,
    })
}


/// `use crate::a::{b, c as d};` becomes `crate::a::b` and `crate::a::c`.
///
/// One level of braces is enough: every use statement in this repository nests at
/// most that deep, and a path this cannot expand is simply not resolved.
fn expand_use_path(raw: &str) -> Vec<String> {
    let text = raw.trim().trim_end_matches(';').trim();
    let text = text.strip_prefix("use ").unwrap_or(text).trim();
    let Some(open) = text.find('{') else {
        return vec![strip_alias(text)];
    };
    let head = text[..open].trim().trim_end_matches("::").trim();
    let Some(close) = text.rfind('}') else {
        return vec![strip_alias(text)];
    };
    text[open + 1..close]
        .split(',')
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(|branch| {
            if branch == "self" {
                head.to_owned()
            } else {
                format!("{head}::{}", strip_alias(branch))
            }
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// Drops a trailing `as Name` from a use path.
fn strip_alias(path: &str) -> String {
    let path = path.trim();
    match path.find(" as ") {
        Some(at) => path[..at].trim().to_owned(),
        None => path.to_owned(),
    }
}

/// A definition name reported by the outline query, before the per-file cap.
struct Candidate {
    name: Arc<str>,
    kind: &'static str,
    row: u32,
    /// Depth of the matched node; shallow nodes win when the cap trims a file.
    tree_depth: usize,
    owner: Option<Arc<str>>,
    depth: u32,
    access: Option<Arc<str>>,
    signature: Option<Arc<str>>,
    local: bool,
}

/// Runs the grammar's outline query and keeps one readable entry per
/// definition name.
///
/// Outline queries match every `@name` they can reach, so a function body's
/// local declarations and nested declarators show up next to the definitions
/// the panel is meant to list. Each match is classified into a readable kind
/// and filtered before it becomes a [`SymbolEntry`].
fn collect_symbols(
    config: Option<&language::OutlineConfig>,
    root: tree_sitter::Node<'_>,
    text: &str,
) -> Vec<SymbolEntry> {
    let Some(config) = config else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&config.query, root, bytes);
    let mut candidates: Vec<Candidate> = Vec::new();
    // Prototypes and definitions of the same name, or a local declared twice,
    // are one symbol: keyed by the definition body that contains them. The
    // body's range rather than its start byte is the key, because a top-level
    // class and a member of it can both start at byte zero. The signature is
    // part of the key so overloads (`find(int)` and `find(int, int)`) stay
    // distinct while a prototype and its definition still collapse.
    let mut seen: HashSet<(usize, usize, String)> = HashSet::new();
    while let Some(mat) = matches.next() {
        let capture = |index: u32| {
            mat.captures
                .iter()
                .find(|capture| capture.index == index)
                .map(|capture| capture.node)
        };
        let (Some(name_node), Some(item_node)) = (
            capture(config.name_capture_ix),
            capture(config.item_capture_ix),
        ) else {
            continue;
        };
        // An `impl` block is a container, not a definition: its members are
        // listed on their own and its name only repeats the implemented type.
        if item_node.kind() == "impl_item" {
            continue;
        }
        // C and C++ outline queries capture the whole declarator for pointers
        // (`*cur`), so the readable name is the innermost declarator.
        let name_node = innermost_declarator(name_node);
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        let name = name.trim();
        // A declarator's text can span several tokens (`(*fp)(int)`), which is
        // not a name a reader can use.
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            continue;
        }
        if is_inside_parameter_list(name_node) {
            continue;
        }
        let kind = classify_kind(item_node);
        let access = member_access(item_node, bytes);
        let signature = match kind {
            "function" | "method" => function_signature(item_node, bytes),
            "variable" | "field" | "constant" => variable_signature(item_node, name_node, bytes),
            _ => None,
        };
        let body = enclosing_body(item_node);
        // Only a function with named parameters can be an overload. A macro
        // invocation such as `TEST(Group, 构造与初始化)` parses as a function
        // whose parameters are bare identifiers, and dozens of those must stay
        // one line rather than flooding the map.
        let distinguishes =
            !matches!(kind, "function" | "method") || parameters_are_named(item_node);
        let mut key = String::with_capacity(name.len() + 8);
        key.push_str(name);
        if distinguishes && let Some(signature) = signature.as_deref() {
            key.push('\u{1}');
            key.push_str(signature);
        }
        if !seen.insert((body.start_byte(), body.end_byte(), key)) {
            continue;
        }
        candidates.push(Candidate {
            name: name.into(),
            kind,
            row: name_node.start_position().row as u32,
            tree_depth: node_depth(item_node),
            owner: container_owner(item_node, bytes),
            depth: symbol_depth(item_node),
            access,
            signature,
            local: is_local(item_node),
        });
    }
    finish_symbols(candidates)
}

/// Applies the per-file cap, keeping the shallowest definitions, then orders
/// the survivors by line as the rest of the map expects.
fn finish_symbols(mut candidates: Vec<Candidate>) -> Vec<SymbolEntry> {
    candidates.sort_by_key(|candidate| (candidate.tree_depth, candidate.row));
    candidates.truncate(MAX_SYMBOLS_PER_FILE);
    candidates.sort_by_key(|candidate| candidate.row);
    candidates
        .into_iter()
        .map(|candidate| SymbolEntry {
            name: candidate.name,
            kind: candidate.kind.into(),
            row: candidate.row,
            owner: candidate.owner,
            depth: candidate.depth,
            access: candidate.access,
            signature: candidate.signature,
            local: candidate.local,
        })
        .collect()
}

/// How deeply a node is nested, used to prefer top-level definitions when a
/// file has more symbols than the cap allows.
fn node_depth(mut node: tree_sitter::Node<'_>) -> usize {
    let mut depth = 0;
    while let Some(parent) = node.parent() {
        depth += 1;
        node = parent;
    }
    depth
}

/// Follows `declarator` fields down to the identifier a reader recognizes.
fn innermost_declarator(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while node.kind().ends_with("declarator") {
        // Most declarators name their inner declarator with a field, but
        // `reference_declarator` (`&alias`) leaves it unnamed.
        let child = node.child_by_field_name("declarator").or_else(|| {
            let mut cursor = node.walk();
            let children: Vec<_> = node
                .named_children(&mut cursor)
                .filter(|child| {
                    child.kind().ends_with("identifier")
                        || child.kind() == "operator_name"
                        || child.kind() == "destructor_name"
                })
                .collect();
            children.into_iter().next()
        });
        let Some(child) = child else {
            break;
        };
        node = child;
    }
    node
}

/// Kinds that nest their children one level deeper in the rendered tree.
const CONTAINERS: &[&str] = &[
    "class_definition",
    "class_specifier",
    "enum_definition",
    "enum_specifier",
    "function_definition",
    "function_item",
    "impl_item",
    "interface_declaration",
    "method_definition",
    "mod_item",
    "module",
    "module_declaration",
    "namespace_definition",
    "object_type",
    "struct_definition",
    "struct_specifier",
    "trait_definition",
    "trait_item",
    "union_specifier",
];

/// Kinds whose bodies hold statements, making everything inside them local.
const FUNCTION_BODIES: &[&str] = &[
    "arrow_function",
    "closure_expression",
    "function_definition",
    "function_item",
    "generator_function_declaration",
    "method_definition",
];

/// The nesting level a symbol is rendered at.
///
/// A C++ access level is one extra step, so members sit one level below their
/// `public:`/`private:` label instead of next to it.
fn symbol_depth(mut node: tree_sitter::Node<'_>) -> u32 {
    let mut depth = 0;
    while let Some(parent) = node.parent() {
        if CONTAINERS.contains(&parent.kind()) || parent.kind() == "field_declaration_list" {
            depth += 1;
        }
        node = parent;
    }
    depth
}

fn is_local(mut node: tree_sitter::Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if FUNCTION_BODIES.contains(&parent.kind()) {
            return true;
        }
        node = parent;
    }
    false
}

/// The name of the closest enclosing definition that has one.
fn container_owner(mut node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<Arc<str>> {
    while let Some(parent) = node.parent() {
        if CONTAINERS.contains(&parent.kind())
            && let Some(name) = container_name(parent, bytes)
        {
            return Some(name);
        }
        node = parent;
    }
    None
}

fn container_name(node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<Arc<str>> {
    let name_node = match node.kind() {
        "function_definition" | "function_item" | "method_definition" => {
            innermost_declarator(node.child_by_field_name("declarator")?)
        }
        "impl_item" => node.child_by_field_name("type")?,
        "class_definition"
        | "class_specifier"
        | "enum_definition"
        | "enum_specifier"
        | "interface_declaration"
        | "mod_item"
        | "module"
        | "module_declaration"
        | "namespace_definition"
        | "object_type"
        | "struct_definition"
        | "struct_specifier"
        | "trait_definition"
        | "trait_item"
        | "union_specifier" => node.child_by_field_name("name")?,
        _ => return None,
    };
    let text = name_node.utf8_text(bytes).ok()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.into())
    }
}

/// The C++ access specifier governing a member, from the last `access_specifier`
/// that precedes it inside the enclosing class body.
fn member_access(mut node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<Arc<str>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "field_declaration_list" {
            let owner = parent.parent()?;
            let mut access = if owner.kind() == "class_specifier" {
                "private"
            } else {
                "public"
            };
            let mut cursor = parent.walk();
            for child in parent.named_children(&mut cursor) {
                if child.start_byte() >= node.start_byte() {
                    break;
                }
                if child.kind() == "access_specifier"
                    && let Ok(text) = child.utf8_text(bytes)
                {
                    access = text.trim().trim_end_matches(':').trim();
                }
            }
            return Some(access.into());
        }
        if matches!(
            parent.kind(),
            "class_specifier" | "struct_specifier" | "union_specifier"
        ) {
            break;
        }
        node = parent;
    }
    None
}

/// The parameter list and return type of a function, rendered after its name.
fn function_signature(item: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<Arc<str>> {
    // C and C++ hang the parameters off a function declarator.
    if let Some(declarator) = find_function_declarator(item)
        && let Some(parameters) = declarator.child_by_field_name("parameters")
    {
        let parameters = normalize_whitespace(parameters.utf8_text(bytes).ok()?);
        let decorations = declarator_decorations(declarator);
        return Some(match declared_type(item, bytes) {
            Some(return_type) => format!("{parameters} -> {return_type}{decorations}").into(),
            None => parameters.into(),
        });
    }
    // Other grammars put them on the item, and name the return type.
    let parameters = item.child_by_field_name("parameters")?;
    let parameters = normalize_whitespace(parameters.utf8_text(bytes).ok()?);
    let return_type = item
        .child_by_field_name("return_type")
        .or_else(|| item.child_by_field_name("type"))
        .and_then(|node| node.utf8_text(bytes).ok())
        .map(normalize_whitespace)
        .filter(|text| !text.is_empty());
    Some(match return_type {
        Some(return_type) => format!("{parameters} -> {return_type}").into(),
        None => parameters.into(),
    })
}

/// Whether a function's parameters carry names, which is what tells a real
/// overload from a macro call shaped like a function declaration.
fn parameters_are_named(item: tree_sitter::Node<'_>) -> bool {
    let Some(declarator) = find_function_declarator(item) else {
        // Grammars that put the list on the item (Rust, Python) name their
        // parameters there.
        return item.child_by_field_name("parameters").is_some();
    };
    let Some(parameters) = declarator.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = parameters.walk();
    parameters.named_children(&mut cursor).any(|parameter| {
        parameter.child_by_field_name("declarator").is_some()
            || parameter.kind() == "variadic_parameter_declaration"
    })
}

fn find_function_declarator(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if node.kind() == "function_declarator" {
        return Some(node);
    }
    let declared = node.child_by_field_name("declarator").or_else(|| {
        // `Group& operator=(const Group&)` wraps the function declarator in a
        // reference declarator that leaves it unnamed.
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind().ends_with("declarator"))
            .collect();
        children.into_iter().next()
    });
    declared.and_then(find_function_declarator)
}

/// The `*`/`&` a declarator adds to the type written before it. `TreeNode
/// *make()` reads `() -> TreeNode*`, just as `variable_signature` reads
/// `ListNode *cur` as `: ListNode*`.
fn declarator_decorations(node: tree_sitter::Node<'_>) -> String {
    let mut decorations = String::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "pointer_declarator" => decorations.push('*'),
            "reference_declarator" => decorations.push('&'),
            "array_declarator" | "init_declarator" | "parenthesized_declarator" => {}
            _ => break,
        }
        current = parent.parent();
    }
    decorations
}

/// The type a declaration writes before its declarator, walked out from the
/// declarator the outline query captured.
fn declared_type(mut node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<String> {
    for _ in 0..8 {
        if let Some(type_node) = node.child_by_field_name("type")
            && let Ok(text) = type_node.utf8_text(bytes)
        {
            let text = normalize_whitespace(text);
            if !text.is_empty() {
                return Some(text);
            }
        }
        if !matches!(
            node.kind(),
            "array_declarator"
                | "declaration"
                | "field_declaration"
                | "function_declarator"
                | "function_definition"
                | "init_declarator"
                | "pointer_declarator"
                | "reference_declarator"
        ) && !node.kind().ends_with("identifier")
        {
            return None;
        }
        node = node.parent()?;
    }
    None
}

/// The declared type of a variable, with the pointer and reference markers
/// that live on its declarator (`ListNode *cur` reads `: ListNode*`).
fn variable_signature(
    item: tree_sitter::Node<'_>,
    name_node: tree_sitter::Node<'_>,
    bytes: &[u8],
) -> Option<Arc<str>> {
    let declared = declared_type(item, bytes)?;
    let mut decorations = String::new();
    let mut current = name_node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "pointer_declarator" => decorations.push('*'),
            "reference_declarator" => decorations.push('&'),
            "array_declarator" | "init_declarator" | "parenthesized_declarator" => {}
            _ => break,
        }
        current = parent.parent();
    }
    Some(format!(": {declared}{decorations}").into())
}

/// Collapses runs of whitespace so a declaration that spans lines still reads
/// as one signature.
fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Node kinds that open a definition body. Duplicate names are only collapsed
/// within one of these, so two functions may each declare a local `i`.
const DEFINITION_BODIES: &[&str] = &[
    "class_definition",
    "class_specifier",
    "function_definition",
    "function_item",
    "impl_item",
    "interface_declaration",
    "mod_item",
    "namespace_definition",
    "struct_item",
    "struct_specifier",
    "trait_item",
];

fn enclosing_body(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while let Some(parent) = node.parent() {
        if DEFINITION_BODIES.contains(&parent.kind()) {
            return parent;
        }
        node = parent;
    }
    node
}

/// Parameter names are not definitions. Outline queries are anchored on
/// definitions, but the free-form `declaration` patterns of C and C++ can
/// still reach into a parameter list, so the names are filtered by ancestry.
fn is_inside_parameter_list(node: tree_sitter::Node<'_>) -> bool {
    const PARAMETER_KINDS: &[&str] = &[
        "argument_list",
        "formal_parameters",
        "optional_parameter_declaration",
        "parameter",
        "parameter_declaration",
        "parameter_list",
        "parameters",
        "preproc_params",
        "template_parameter_list",
        "type_parameters",
        "variadic_parameter",
        "variadic_parameter_declaration",
    ];
    let mut current = Some(node);
    while let Some(node) = current {
        if PARAMETER_KINDS.contains(&node.kind()) {
            return true;
        }
        current = node.parent();
    }
    false
}

/// The readable category of a definition.
///
/// `@item` often lands on a declarator or a plain `declaration`, whose kind
/// says nothing about what is being defined. Walking up to the nearest node
/// that carries meaning (`function_definition`, `class_specifier`,
/// `field_declaration`, ...) classifies those instead of exposing the
/// tree-sitter node name.
fn classify_kind(item: tree_sitter::Node<'_>) -> &'static str {
    let mut current = Some(item);
    while let Some(node) = current {
        if let Some(kind) = semantic_kind(node) {
            return refine_kind(node, kind);
        }
        current = node.parent();
    }
    "symbol"
}

fn semantic_kind(node: tree_sitter::Node<'_>) -> Option<&'static str> {
    let kind = match node.kind() {
        "arrow_function"
        | "constructor_declaration"
        | "destructor_declaration"
        | "function_declaration"
        | "function_definition"
        | "function_item"
        | "function_signature_item"
        | "method_declaration"
        | "method_definition" => "function",
        // C++ parses `vector<bool> visited(total, false);` as a function
        // declarator, so the declarator's context decides what it is.
        "function_declarator" => {
            if is_value_initialization(node) {
                "variable"
            } else {
                "function"
            }
        }
        // A declaration covers variables and function prototypes alike.
        "declaration" | "lexical_declaration" | "variable_declaration" => {
            if declares_function(node) {
                "function"
            } else {
                "variable"
            }
        }
        "class_declaration" | "class_definition" | "class_specifier" | "object_type" => "class",
        "struct_definition" | "struct_item" | "struct_specifier" | "union_definition"
        | "union_specifier" => "struct",
        "enum_definition" | "enum_item" | "enum_specifier" => "enum",
        "enum_variant" | "enumerator" => "variant",
        "trait_declaration" | "trait_definition" | "trait_item" => "trait",
        "interface_declaration" | "interface_definition" => "interface",
        "field_declaration" | "field_definition" => {
            if declares_function(node) {
                "function"
            } else {
                "field"
            }
        }
        "property_declaration" | "public_field_definition" => "field",
        "mod_item" | "module" | "module_declaration" | "namespace_definition" => "module",
        "const_item" | "constant_declaration" | "static_item" => "constant",
        "associated_type"
        | "concept_definition"
        | "type_alias_declaration"
        | "type_declaration"
        | "type_definition"
        | "type_item" => "type",
        "macro_definition" | "preproc_def" | "preproc_function_def" => "macro",
        _ => return None,
    };
    Some(kind)
}

/// Whether a declaration's declarator chain ends in a function, which tells a
/// prototype (`void sort(int);`) from a variable (`int i = 0;`).
fn declares_function(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    let declarators: Vec<_> = node
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    declarators.into_iter().any(declarator_is_function)
}

fn declarator_is_function(declarator: tree_sitter::Node<'_>) -> bool {
    if declarator.kind() == "function_declarator" {
        return !is_value_initialization(declarator);
    }
    let mut cursor = declarator.walk();
    let nested: Vec<_> = declarator
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    nested.into_iter().any(declarator_is_function)
}

/// Whether a function declarator is really a variable initialized with a
/// parenthesized value list (`vector<bool> visited(total, false);`).
///
/// C++ resolves the most vexing parse by name lookup, which a syntax tree does
/// not have. Inside a function body, a declarator whose parameters are bare
/// non-builtin identifiers is a value list in practice, not a nested prototype.
fn is_value_initialization(declarator: tree_sitter::Node<'_>) -> bool {
    if declarator.kind() != "function_declarator" || !is_local(declarator) {
        return false;
    }
    let Some(parameters) = declarator.child_by_field_name("parameters") else {
        return false;
    };
    if parameters.named_child_count() == 0 {
        return false;
    }
    let mut cursor = parameters.walk();
    parameters.named_children(&mut cursor).all(|parameter| {
        parameter.kind() == "parameter_declaration"
            && parameter.child_by_field_name("declarator").is_none()
            && parameter
                .child_by_field_name("type")
                .is_some_and(|kind| kind.kind() != "primitive_type")
    })
}

/// A function that lives in a type body is a method, not a free function.
fn refine_kind(node: tree_sitter::Node<'_>, kind: &'static str) -> &'static str {
    if kind == "function" && has_type_ancestor(node) {
        "method"
    } else {
        kind
    }
}

fn has_type_ancestor(node: tree_sitter::Node<'_>) -> bool {
    const TYPE_BODIES: &[&str] = &[
        "class_definition",
        "class_specifier",
        "impl_item",
        "interface_declaration",
        "object_type",
        "struct_specifier",
        "trait_item",
    ];
    let mut current = node.parent();
    while let Some(parent) = current {
        if TYPE_BODIES.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// Collects identifier-shaped tokens and the import specs a file declares, in
/// one walk of the tree.
///
/// Every tree-sitter grammar in this repository names identifiers with a
/// `*_identifier` kind, which keeps the mention side language-agnostic without a
/// per-language tags query. The specs are tagged with the rule that resolves
/// them: `mod:name`, `use:crate::a::b`, `include:path.h`, `import:./path`.
///
/// Only specs that name a file are kept. A Rust inline `mod foo { ... }` is a
/// declaration, not an include, so it is skipped — treating its text as a path is
/// what used to point every inline test module at the shallowest `tests.rs`.
/// `use serde::Deserialize` is kept as a spec and refused later by
/// [`resolve_dependencies`], which knows the crate's module tree.
fn collect_references(
    root: tree_sitter::Node<'_>,
    text: &str,
    family: &str,
) -> (HashSet<Arc<str>>, Vec<String>) {
    let bytes = text.as_bytes();
    let mut mentions: HashSet<Arc<str>> = HashSet::new();
    let mut includes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        // The mention cap bounds the reference graph; the walk itself continues
        // so an import declared late in a huge file is still seen.
        if mentions.len() < MAX_IDENTIFIERS_PER_FILE
            && kind.ends_with("identifier")
            && let Ok(name) = node.utf8_text(bytes)
            && !name.is_empty()
        {
            mentions.insert(name.into());
        }
        match (family, kind) {
            ("rust", "mod_item") => {
                if node.child_by_field_name("body").is_none()
                    && let Some(name) = node.child_by_field_name("name")
                    && let Ok(raw) = name.utf8_text(bytes)
                {
                    includes.push(format!("mod:{raw}"));
                }
            }
            ("rust", "use_declaration") => {
                if let Ok(raw) = node.utf8_text(bytes) {
                    for path in expand_use_path(raw) {
                        includes.push(format!("use:{path}"));
                    }
                }
            }
            ("c", "preproc_include") => {
                if let Some(path) = node.child_by_field_name("path")
                    && let Ok(raw) = path.utf8_text(bytes)
                    && let Some(spec) = raw.trim().strip_prefix('"')
                    && let Some(spec) = spec.strip_suffix('"')
                {
                    includes.push(format!("include:{spec}"));
                }
            }
            ("javascript", "import_statement" | "export_statement") => {
                let source = node
                    .child_by_field_name("source")
                    .or_else(|| node.child_by_field_name("path"));
                if let Some(source) = source
                    && let Ok(raw) = source.utf8_text(bytes)
                {
                    let spec = raw.trim().trim_matches(|c| c == '"' || c == '\'');
                    if spec.starts_with('.') {
                        includes.push(format!("import:{spec}"));
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    (mentions, includes)
}

/// Character budget for one `repo_map` page.
///
/// This is the single source of truth for the agent-facing map: the snapshot
/// records it as `max_chars` and the bundled MCP server reads it from there, so
/// WuIDE's rendering and the server's cannot drift apart. `limit` is an upper
/// bound on the files of one page; the budget can end the page earlier and the
/// header says so. It covers the whole page, the list of omitted paths
/// included.
pub const MAX_REPO_MAP_CHARS: usize = 48_000;

/// Definitions one file block shows before it summarizes the rest.
///
/// A file with two hundred definitions is not more useful than its first forty:
/// an agent that wants the rest calls `symbols` for that one file. Without a cap
/// a single such block can spend a third of [`MAX_REPO_MAP_CHARS`], which makes
/// `limit` unobservable.
pub const MAX_SYMBOLS_PER_FILE_IN_MARKDOWN: usize = 40;

/// Paths one page lists after its body before summarizing the rest.
///
/// The list exists so an agent knows what it did not see, but hundreds of
/// paths cost more than the map they describe, and a caller who asked for one
/// file does not want a full directory listing. Twenty names plus a count keep
/// the page honest and inside [`MAX_REPO_MAP_CHARS`]; `files_only` and `offset`
/// show the rest. Both numbers are mirrored by
/// `agents/mcp/wu-repo-map/server.js`.
pub const MAX_OMITTED_PATHS: usize = 20;

/// The same cap for a page that asked for very few files.
pub const MAX_OMITTED_PATHS_SMALL: usize = 10;

/// How many omitted paths one page may list.
pub fn omitted_path_limit(limit: usize) -> usize {
    if limit <= 5 {
        MAX_OMITTED_PATHS_SMALL
    } else {
        MAX_OMITTED_PATHS
    }
}

/// The weight one mention of `name` gives to each file that defines it.
///
/// `1 / df`, where `df` is the number of files defining the name, so a mention
/// always hands out a total of exactly one unit of weight no matter how many
/// files share the name: a name only one file owns points at it with the whole
/// unit and a name twenty files own splits it.
///
/// The alternative shape, `1 / (1 + ln df)`, decays far more gently (0.59 at
/// `df = 2`, still 0.25 at `df = 20`), which keeps the total weight of a mention
/// growing with `df`: files that write generic vocabulary would then keep more
/// outgoing mass than files that write distinctive names, and the spread of one
/// mention over twenty definers would outweigh a single unambiguous reference.
/// `1 / df` is also exact in binary for the small `df` values that matter, so
/// two builds of the same file set rank bit for bit identically.
fn mention_weight(definers: usize) -> f32 {
    1. / definers as f32
}

/// Resolves every file's import specs to the files they actually name, and sets
/// [`FileSymbols::dep_in_degree`] on the targets.
///
/// This is the only place a spec becomes an edge, and it refuses anything it
/// cannot prove: `use crate::a::b` resolves inside the crate's own module tree,
/// `mod x;` looks where rustc looks, `#include "x.h"` and `import "./x"` follow
/// the path they spell. A bare name — `use serde::Deserialize`, `import "react"`,
/// `#include <stdio.h>` — names something outside the map, so it creates nothing.
/// Guessing by file stem is what used to send every `use serde::…` in this
/// repository to `crates/util/src/serde.rs` and every inline `mod tests` to the
/// shallowest `tests.rs`.
fn resolve_dependencies(files: &mut [ParsedFile]) {
    let paths: Vec<String> = files
        .iter()
        .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
        .collect();
    let by_path: HashMap<&str, usize> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.as_str(), index))
        .collect();

    let mut incoming: Vec<u32> = vec![0; files.len()];
    for (index, file) in files.iter().enumerate() {
        let mut targets: HashSet<usize> = HashSet::new();
        for spec in &file.includes {
            if let Some(target) = resolve_spec(spec, &paths[index], &by_path)
                && target != index
            {
                targets.insert(target);
            }
        }
        for target in targets {
            incoming[target] += 1;
        }
    }
    for (index, file) in files.iter_mut().enumerate() {
        file.dep_in_degree = incoming[index];
    }
}

/// Extensions a JavaScript or TypeScript relative import may omit.
const IMPORT_EXTENSIONS: &[&str] = &["", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

/// Resolves one tagged spec (`mod:…`, `use:…`, `include:…`, `import:…`) against
/// the files the map holds.
fn resolve_spec(spec: &str, source: &str, by_path: &HashMap<&str, usize>) -> Option<usize> {
    let (kind, rest) = spec.split_once(':')?;
    match kind {
        "mod" => resolve_module(rest, source, by_path),
        "use" => resolve_use(rest, source, by_path),
        "include" => resolve_relative(rest, source, by_path, &[""]),
        "import" => resolve_relative(rest, source, by_path, IMPORT_EXTENSIONS),
        _ => None,
    }
}

/// `mod x;` resolves to the four places rustc looks for `x`: beside the module
/// file (`foo/x.rs`, `foo/x/mod.rs`) and, for the `foo.rs` style the compiler
/// still accepts, beside it (`x.rs`, `x/mod.rs`).
fn resolve_module(name: &str, source: &str, by_path: &HashMap<&str, usize>) -> Option<usize> {
    let directory = parent_of(source);
    let base = source.rsplit('/').next().unwrap_or_default();
    let child = if matches!(base, "mod.rs" | "lib.rs" | "main.rs") {
        directory.to_owned()
    } else {
        format!("{directory}/{}", base.trim_end_matches(".rs"))
    };
    for prefix in [child.as_str(), directory] {
        let prefix = if prefix.is_empty() { "" } else { prefix };
        for candidate in [
            format!("{prefix}/{name}.rs"),
            format!("{prefix}/{name}/mod.rs"),
        ] {
            let candidate = candidate.trim_start_matches('/');
            if let Some(&index) = by_path.get(candidate) {
                return Some(index);
            }
        }
    }
    None
}

/// `use crate::a::b` resolves inside this file's crate module tree.
///
/// The longest path that names a file wins, so `use crate::editor::Editor` lands
/// on `editor.rs` (the last segment is a type, not a module) while
/// `use crate::editor::display_map` lands on `display_map.rs`. Paths that start
/// with any other root — `std`, `serde`, another crate — name nothing here.
fn resolve_use(path: &str, source: &str, by_path: &HashMap<&str, usize>) -> Option<usize> {
    let root = crate_root(source)?;
    let mut segments: Vec<&str> = path.split("::").collect();
    let mut prefix: Vec<&str> = Vec::new();
    match segments.first().copied()? {
        "crate" => {
            segments.remove(0);
        }
        "self" => {
            segments.remove(0);
            prefix = module_of(source);
        }
        "super" => {
            let mut module = module_of(source);
            while segments.first() == Some(&"super") {
                segments.remove(0);
                module.pop();
            }
            prefix = module;
        }
        _ => return None,
    }
    if segments.is_empty() || segments.iter().any(|segment| *segment == "*" || segment.is_empty()) {
        return None;
    }
    let mut full: Vec<&str> = prefix;
    full.extend(segments);
    for cut in (1..=full.len()).rev() {
        let head = full[..cut].join("/");
        for candidate in [format!("{root}/{head}.rs"), format!("{root}/{head}/mod.rs")] {
            if let Some(&index) = by_path.get(candidate.as_str()) {
                return Some(index);
            }
        }
    }
    None
}

/// `#include "x.h"` and `import "./x"` resolve against the including file's
/// directory, with the extensions the language lets the author omit.
fn resolve_relative(
    spec: &str,
    source: &str,
    by_path: &HashMap<&str, usize>,
    extensions: &[&str],
) -> Option<usize> {
    let spec = spec.replace('\\', "/");
    let base = normalize_join(parent_of(source), &spec);
    if base.is_empty() {
        return None;
    }
    for extension in extensions {
        if let Some(&index) = by_path.get(format!("{base}{extension}").as_str()) {
            return Some(index);
        }
        for index_file in ["index.ts", "index.tsx", "index.js", "index.jsx"] {
            if !extension.is_empty() {
                continue;
            }
            if let Some(&index) = by_path.get(format!("{base}/{index_file}").as_str()) {
                return Some(index);
            }
        }
    }
    None
}

/// Joins `spec` onto `base`, applying `.` and `..` the way a path would.
fn normalize_join(base: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for segment in spec.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The crate root a file belongs to: the path up to and including its `src`.
fn crate_root(source: &str) -> Option<&str> {
    let parts: Vec<&str> = source.split('/').collect();
    let src = parts.iter().position(|part| *part == "src")?;
    if src + 1 >= parts.len() {
        return None;
    }
    Some(&source[..source.find("/src/")? + 4])
}

/// The module path of a file inside its crate, as `crate::…` would spell it.
fn module_of(source: &str) -> Vec<&str> {
    let Some(root) = crate_root(source) else {
        return Vec::new();
    };
    let rest = source[root.len()..].trim_start_matches('/');
    let mut parts: Vec<&str> = rest.split('/').collect();
    let base = parts.pop().unwrap_or_default();
    if !matches!(base, "mod.rs" | "lib.rs" | "main.rs") {
        parts.push(base.trim_end_matches(".rs"));
    }
    parts
}

/// Whether a file's definitions are data rather than names code can call.
/// JSON keys and Markdown headings reach the map through the outline query with
/// the generic `symbol` kind, and they are spelled like the code that consumes
/// them: `assets/settings/default.json` "defines" `dev`, `light` and
/// `dark`, and the Rust settings code mentions all three. Those edges are
/// coincidences of spelling, not dependencies, and they put the defaults file
/// and the locale bundles in the map's first page. A file whose every definition
/// is such a name takes no part in the reference graph, on either side; it stays
/// in the map with its definitions and zero counts.
fn is_data_file(file: &ParsedFile) -> bool {
    !file.symbols.is_empty() && file.symbols.iter().all(|symbol| &*symbol.kind == "symbol")
}

/// PageRank over the cross-file reference graph, and the `in`/`out` counts that
/// describe the same graph.
///
/// An edge `a -> b` means file `a` mentions a name that file `b` owns. Three
/// rules decide whether the mention is evidence at all: only
/// [`OWNING_KINDS`] can own a name, both files must be in the same
/// [`language_family`] (a `Menu` in TypeScript is not a `Menu` in Rust), and a
/// name more than [`shared_name_file_limit`] files mention is vocabulary rather
/// than a dependency. Every surviving mention weighs [`mention_weight`] of the
/// number of files owning the name. Degrees are the number of distinct files on
/// each side of those edges, so what a page prints and what the ranking used
/// cannot disagree.
fn rank(files: &mut [ParsedFile]) {
    let count = files.len();
    if count == 0 {
        return;
    }
    if count == 1 {
        files[0].rank = 1.;
        return;
    }

    let mut defined_in: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
    // How many files write each name, which is what tells a definition's name
    // from vocabulary; `defined_in.len()` cannot, because a name can be defined
    // once and mentioned by every file.
    let mut mentioned_by: HashMap<Arc<str>, usize> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        if is_data_file(file) {
            continue;
        }
        for mention in &file.mentions {
            *mentioned_by.entry(mention.clone()).or_default() += 1;
        }
        for symbol in &file.symbols {
            if !owns_references(&symbol.kind) {
                continue;
            }
            defined_in
                .entry(symbol.name.clone())
                .or_default()
                .push(index);
        }
    }
    // `df` counts files, not definitions: a file that defines `new` three times
    // is still one file that owns the name.
    for definers in defined_in.values_mut() {
        definers.sort_unstable();
        definers.dedup();
    }

    // Outgoing weights per file, plus each file's total weight for
    // normalization. The cap is decided once here so the graph and the text
    // that describes it use the same number.
    let shared_limit = shared_name_file_limit(count);
    let mut edges: Vec<Vec<(usize, f32)>> = vec![Vec::new(); count];
    let mut totals: Vec<f32> = vec![0.; count];
    for (index, file) in files.iter().enumerate() {
        if is_data_file(file) {
            continue;
        }
        let mut weights: HashMap<usize, f32> = HashMap::new();
        for mention in &file.mentions {
            if mentioned_by.get(mention).copied().unwrap_or(0) > shared_limit {
                continue;
            }
            let Some(definers) = defined_in.get(mention) else {
                continue;
            };
            let weight = mention_weight(definers.len());
            for &target in definers {
                if target != index && files[target].family == file.family {
                    *weights.entry(target).or_default() += weight;
                }
            }
        }
        // Sorted, because floating point addition is not associative: two
        // builds of one file set must produce the same ranks bit for bit, and
        // a cached parse restores the mentions in a different order than the
        // parse that wrote it collected them.
        let mut outgoing: Vec<(usize, f32)> = weights.into_iter().collect();
        outgoing.sort_by_key(|(target, _)| *target);
        let total: f32 = outgoing.iter().map(|(_, weight)| weight).sum();
        if total > 0. {
            edges[index] = outgoing;
            totals[index] = total;
        }
    }

    let mut ranks = vec![1. / count as f32; count];
    let teleport = (1. - PAGERANK_DAMPING) / count as f32;
    for _ in 0..PAGERANK_ITERATIONS {
        let mut next = vec![teleport; count];
        for (index, outgoing) in edges.iter().enumerate() {
            if outgoing.is_empty() {
                continue;
            }
            let share = PAGERANK_DAMPING * ranks[index] / totals[index];
            for &(target, weight) in outgoing {
                next[target] += share * weight;
            }
        }
        ranks = next;
    }

    let mut referenced_by: Vec<u32> = vec![0; count];
    for outgoing in &edges {
        for &(target, _) in outgoing {
            referenced_by[target] += 1;
        }
    }
    for (index, file) in files.iter_mut().enumerate() {
        file.rank = ranks[index];
        file.out_degree = edges[index].len() as u32;
        file.in_degree = referenced_by[index];
    }
}

/// Lists the files of the project's visible worktrees, ignoring anything the
/// worktree already filtered out (`.gitignore`, exclusions).
pub fn project_sources(project: &Entity<Project>, cx: &App) -> Vec<SourceFile> {
    let mut sources = Vec::new();
    for worktree in project.read(cx).visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path();
        let snapshot = worktree.snapshot();
        for entry in snapshot.entries(false, 0) {
            if entry.kind != worktree::EntryKind::File {
                continue;
            }
            sources.push(SourceFile {
                rel: entry.path.clone(),
                abs_path: root.join(entry.path.as_std_path()),
                size: entry.size,
                modified_ns: modified_nanos(entry.mtime),
            });
        }
    }
    sources
}

/// Builds a symbol map for the whole project off the foreground thread, reusing
/// the cache. The returned task is cancelled when dropped.
pub fn symbol_map_task(project: Entity<Project>, cx: &App) -> Task<Arc<SymbolMap>> {
    if !crate::builds_map(cx) {
        return Task::ready(Arc::new(SymbolMap::default()));
    }
    let sources = project_sources(&project, cx);
    let roots: Vec<PathBuf> = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    let fs = project.read(cx).fs().clone();
    let languages = project.read(cx).languages().clone();
    let cache_dir = paths::repo_map_cache_dir();
    cx.background_spawn(async move {
        build_cached(sources, fs, languages, &roots, &cache_dir)
            .await
            .map
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;

    fn entry(name: &str, kind: &str, row: u32) -> SymbolEntry {
        SymbolEntry {
            name: name.into(),
            kind: kind.into(),
            row,
            owner: None,
            depth: 0,
            access: None,
            signature: None,
            local: false,
        }
    }

    fn test_rel(rel: &str) -> Arc<RelPath> {
        RelPath::new(std::path::Path::new(rel), path::PathStyle::local())
            .expect("valid test path")
            .into_arc()
    }

    fn file_symbols(rel: &str, rank: f32, symbols: Vec<SymbolEntry>) -> FileSymbols {
        FileSymbols {
            rel: test_rel(rel),
            size: 10,
            rank,
            lines: 1,
            in_degree: 0,
            out_degree: 0,
            dep_in_degree: 0,
            symbols,
        }
    }

    fn file(rel: &str, defs: &[&str], mentions: &[&str]) -> ParsedFile {
        ParsedFile {
            rel: test_rel(rel),
            path: rel.to_owned(),
            size: 1,
            lines: 1,
            rank: 0.,
            in_degree: 0,
            out_degree: 0,
            dep_in_degree: 0,
            symbols: defs.iter().map(|name| entry(name, "function", 0)).collect(),
            mentions: mentions.iter().map(|name| Arc::from(*name)).collect(),
            family: "rust".into(),
            includes: Vec::new(),
        }
    }

    /// The same fixture in another language family.
    fn file_in_family(rel: &str, family: &str, defs: &[&str], mentions: &[&str]) -> ParsedFile {
        let mut file = file(rel, defs, mentions);
        file.family = family.into();
        file
    }

    #[test]
    fn a_file_everyone_mentions_outranks_an_isolated_one() {
        let mut files = vec![
            file("shared.rs", &["shared_api"], &[]),
            file("user_a.rs", &["a"], &["shared_api"]),
            file("user_b.rs", &["b"], &["shared_api"]),
            file("isolated.rs", &["lonely"], &[]),
        ];
        rank(&mut files);

        let rank_of = |name: &str| {
            files
                .iter()
                .find(|file| file.rel.as_std_path().to_string_lossy() == name)
                .expect("file present")
                .rank
        };
        assert!(rank_of("shared.rs") > rank_of("isolated.rs"));
        assert!(rank_of("shared.rs") > rank_of("user_a.rs"));
    }

    #[test]
    fn self_references_do_not_create_edges() {
        let mut files = vec![file("only.rs", &["thing"], &["thing"])];
        rank(&mut files);
        // A single file keeps the whole probability mass.
        assert_eq!(files[0].rank, 1.);
    }

    #[test]
    fn ranks_sum_to_about_one() {
        let mut files = vec![
            file("a.rs", &["alpha"], &["beta"]),
            file("b.rs", &["beta"], &["alpha"]),
        ];
        rank(&mut files);
        let sum: f32 = files.iter().map(|file| file.rank).sum();
        assert!((sum - 1.).abs() < 0.01, "sum was {sum}");
    }

    fn source(rel: &str, size: u64) -> SourceFile {
        SourceFile {
            rel: RelPath::new(std::path::Path::new(rel), path::PathStyle::local())
                .expect("valid test path")
                .into_arc(),
            abs_path: PathBuf::from(rel),
            size,
            modified_ns: None,
        }
    }

    /// What a worktree snapshot records for one file.
    fn file_mtime_nanos(path: &Path) -> Option<i64> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        i64::try_from(since_epoch.as_nanos()).ok()
    }

    /// Re-reads size and mtime the way the worktree watcher would, so a test
    /// edit looks like an edit to the cache instead of a stale entry.
    fn refresh_sources(sources: &mut [SourceFile]) {
        for source in sources {
            match std::fs::metadata(&source.abs_path) {
                Ok(metadata) => {
                    source.size = metadata.len();
                    source.modified_ns = file_mtime_nanos(&source.abs_path);
                }
                Err(_) => source.modified_ns = None,
            }
        }
    }

    /// Parses a real Rust source file through the same path the builder uses,
    /// so the grammar's outline query and the identifier walk are both
    /// exercised against real code rather than a fixture.
    /// End-to-end build over real directories of this repository: exercises
    /// the filesystem reader, language lookup, parsing, graph building and
    /// ranking together. Ignored by default because it walks the repository.
    #[gpui::test]
    async fn builds_a_ranked_map_over_real_directories(cx: &mut gpui::TestAppContext) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate lives in the workspace");
        let mut sources = Vec::new();
        for directory in ["path/src", "repo_wiki/src", "text/src"] {
            let directory = root.join(directory);
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let Ok(rel) = RelPath::new(relative, path::PathStyle::local()) else {
                    continue;
                };
                sources.push(SourceFile {
                    rel: rel.into_arc(),
                    abs_path: path.clone(),
                    size: metadata.len(),
                    modified_ns: file_mtime_nanos(&path),
                });
            }
        }
        assert!(!sources.is_empty(), "no sources found under {root:?}");

        let executor = cx.background_executor.clone();
        let fs = Arc::new(fs::RealFs::new(None, executor.clone()));
        let registry = Arc::new(language::LanguageRegistry::new(executor));
        registry.add(languages::language(
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
        ));

        let map = build(sources, fs, registry).await;
        assert!(map.stats.parsed > 0, "stats: {:?}", map.stats);
        assert!(
            map.files.iter().any(|file| !file.symbols.is_empty()),
            "no definitions extracted"
        );
        let markdown = map.to_markdown(4_000);
        assert!(markdown.contains("Repository map"));
    }

    /// A small Rust project on disk: its root, and the sources a worktree
    /// snapshot would list for it.
    fn rust_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<SourceFile>) {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().to_path_buf();
        let mut sources = Vec::with_capacity(files.len());
        for (rel, text) in files {
            let rel = *rel;
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent directory");
            }
            std::fs::write(&path, text).expect("source file");
            sources.push(SourceFile {
                rel: RelPath::new(Path::new(rel), path::PathStyle::local())
                    .expect("valid path")
                    .into_arc(),
                size: text.len() as u64,
                modified_ns: file_mtime_nanos(&path),
                abs_path: path,
            });
        }
        (directory, root, sources)
    }

    fn rust_registry(cx: &gpui::TestAppContext) -> Arc<LanguageRegistry> {
        let registry = Arc::new(language::LanguageRegistry::new(
            cx.background_executor.clone(),
        ));
        registry.add(languages::language(
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
        ));
        registry
    }

    fn project_files() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "src/main.rs",
                "mod store;\nuse crate::store::Store;\nfn main() {\n    let store = Store::new();\n    store.save();\n}\n",
            ),
            (
                "src/store.rs",
                "pub struct Store {\n    items: Vec<u8>,\n}\n\nimpl Store {\n    pub fn new() -> Self {\n        Store { items: Vec::new() }\n    }\n\n    pub fn save(&self) {}\n}\n",
            ),
            ("src/util.rs", "pub fn helper() -> u32 {\n    1\n}\n"),
        ]
    }

    /// The renderings must not be able to tell a cached build from an uncached
    /// one: same markdown, same digest, same snapshot bytes, same clipboard
    /// export.
    fn assert_same_output(cached: &SymbolMap, uncached: &SymbolMap, root: &Path) {
        assert_eq!(cached.stats.parsed, uncached.stats.parsed);
        assert_eq!(
            cached.to_markdown(MAX_REPO_MAP_CHARS),
            uncached.to_markdown(MAX_REPO_MAP_CHARS),
            "markdown differs"
        );
        assert_eq!(
            cached.to_digest(
                DEFAULT_DIGEST_FILES,
                DEFAULT_DIGEST_SYMBOLS_PER_FILE,
                MAX_DIGEST_CHARS
            ),
            uncached.to_digest(
                DEFAULT_DIGEST_FILES,
                DEFAULT_DIGEST_SYMBOLS_PER_FILE,
                MAX_DIGEST_CHARS
            ),
            "digest differs"
        );
        let cached_snapshot = snapshot::to_json(cached, root, "2026-01-02T03:04:05Z");
        let uncached_snapshot = snapshot::to_json(uncached, root, "2026-01-02T03:04:05Z");
        assert_eq!(
            serde_json::to_string(&cached_snapshot).expect("snapshot serializes"),
            serde_json::to_string(&uncached_snapshot).expect("snapshot serializes"),
            "snapshot bytes differ"
        );
        assert_eq!(
            serde_json::to_string(&cached.json_entries()).expect("export serializes"),
            serde_json::to_string(&uncached.json_entries()).expect("export serializes"),
            "clipboard export bytes differ"
        );
    }

    #[gpui::test]
    async fn a_cached_build_matches_the_uncached_map_and_reuses_the_second_time(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, root, sources) = rust_project(&project_files());
        let cache_dir = directory.path().join("repo-maps/.cache");
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor.clone()));
        let roots = std::slice::from_ref(&root);

        let uncached = build(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
        )
        .await;

        let first = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert_eq!(first.parsed, 3, "the first build parses everything");
        assert_eq!(first.reused, 0);
        assert!(!first.from_memory);
        assert_same_output(&first.map, &uncached, &root);

        // Same process, files untouched: the map itself is reused, so nothing is
        // even read.
        let second = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert!(second.from_memory, "an unchanged map comes from memory");
        assert_eq!(second.parsed, 0);
        assert_eq!(second.built_at, first.built_at);
        assert_same_output(&second.map, &uncached, &root);

        // A new process would remember nothing, so the parses come from disk.
        forget_cached_map(roots);
        let from_disk = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert!(!from_disk.from_memory);
        assert_eq!(from_disk.parsed, 0, "every file came from the disk cache");
        assert_eq!(from_disk.reused, 3);
        assert_same_output(&from_disk.map, &uncached, &root);
        assert!(
            cache_dir.join(format!("{}.json", symbol_cache::key(roots))).is_file(),
            "the build leaves a cache document behind"
        );
    }

    #[gpui::test]
    async fn each_kind_of_file_change_retries_only_the_files_it_touched(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, root, mut sources) = rust_project(&project_files());
        let cache_dir = directory.path().join("repo-maps/.cache");
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor.clone()));
        let registry = rust_registry(cx);
        let roots = std::slice::from_ref(&root);

        // A fresh build every time, so each step exercises the cache document
        // and not the in-memory map.
        let rebuild = |sources: Vec<SourceFile>| {
            forget_cached_map(roots);
            let fs = fs.clone();
            let registry = registry.clone();
            let cache_dir = cache_dir.clone();
            let roots = vec![root.clone()];
            async move { build_cached(sources, fs, registry, &roots, &cache_dir).await }
        };

        let first = rebuild(sources.clone()).await;
        assert_eq!(first.parsed, 3);

        // A modification time bump with the same bytes must not be trusted.
        let touched = root.join("src/util.rs");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&touched)
            .expect("the file opens")
            .set_modified(later)
            .expect("the file is touched");
        refresh_sources(&mut sources);
        let after_touch = rebuild(sources.clone()).await;
        assert_eq!(after_touch.parsed, 1, "the touched file is parsed again");
        assert_eq!(after_touch.reused, 2);

        // A size change is caught even when the clock is not.
        let grown = root.join("src/store.rs");
        let text = std::fs::read_to_string(&grown).expect("the file reads");
        std::fs::write(&grown, format!("{text}\npub fn extra() {{}}\n")).expect("the file grows");
        refresh_sources(&mut sources);
        let after_grow = rebuild(sources.clone()).await;
        assert_eq!(after_grow.parsed, 1, "the resized file is parsed again");
        assert_eq!(after_grow.reused, 2);

        // A deletion drops the file from the map and from the cache.
        std::fs::remove_file(root.join("src/util.rs")).expect("the file is removed");
        sources.retain(|source| source.abs_path != root.join("src/util.rs"));
        let after_delete = rebuild(sources.clone()).await;
        assert_eq!(after_delete.parsed, 0);
        assert_eq!(after_delete.reused, 2);
        assert_eq!(after_delete.map.files.len(), 2);
        let document = std::fs::read_to_string(cache_dir.join(format!("{}.json", symbol_cache::key(roots))))
            .expect("the document is written");
        assert!(
            !document.contains("util.rs"),
            "a deleted file must not stay in the cache"
        );

        // An addition is parsed while everything else is reused.
        let added = "src/extra.rs";
        std::fs::write(root.join(added), "pub fn added() -> u8 {\n    3\n}\n").expect("the file is added");
        let size = std::fs::metadata(root.join(added)).expect("the file stats").len();
        sources.push(SourceFile {
            rel: RelPath::new(Path::new(added), path::PathStyle::local())
                .expect("valid path")
                .into_arc(),
            abs_path: root.join(added),
            size,
            modified_ns: file_mtime_nanos(&root.join(added)),
        });
        let after_add = rebuild(sources.clone()).await;
        assert_eq!(after_add.parsed, 1, "the new file is parsed");
        assert_eq!(after_add.reused, 2);
        assert_eq!(after_add.map.files.len(), 3);

        // Whatever the history, the map equals one built from scratch.
        let uncached = build(sources, fs.clone(), rust_registry(cx)).await;
        assert_same_output(&after_add.map, &uncached, &root);
    }

    #[gpui::test]
    async fn a_truncated_cache_document_is_discarded_instead_of_trusted(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, root, sources) = rust_project(&project_files());
        let cache_dir = directory.path().join("repo-maps/.cache");
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor.clone()));
        let roots = std::slice::from_ref(&root);

        let first = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert_eq!(first.parsed, 3);

        let document = cache_dir.join(format!("{}.json", symbol_cache::key(roots)));
        let bytes = std::fs::read(&document).expect("the document was written");
        std::fs::write(&document, &bytes[..bytes.len() / 2]).expect("the document is truncated");

        forget_cached_map(roots);
        let second = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert_eq!(second.parsed, 3, "a corrupt document means parsing everything");
        assert_same_output(&second.map, &first.map, &root);

        // The next build finds a document it can use again.
        forget_cached_map(roots);
        let third = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert_eq!(third.parsed, 0);
        assert_eq!(third.reused, 3);
    }

    #[gpui::test]
    async fn the_in_memory_map_is_dropped_when_a_file_changes_or_on_request(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, root, mut sources) = rust_project(&project_files());
        let cache_dir = directory.path().join("repo-maps/.cache");
        let fs = Arc::new(fs::RealFs::new(None, cx.background_executor.clone()));
        let roots = std::slice::from_ref(&root);

        build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        let reused = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert!(reused.from_memory);

        // A real edit (here: a size change) is enough; no event is needed for the
        // map to notice.
        let edited = root.join("src/main.rs");
        std::fs::write(&edited, "fn main() {\n    println!(\"changed\");\n}\n").expect("the file is edited");
        refresh_sources(&mut sources);
        let after_edit = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert!(!after_edit.from_memory, "an edited file drops the map");
        assert_eq!(after_edit.reused, 2);
        assert_eq!(after_edit.parsed, 1);

        // A manual refresh drops the map even when nothing changed since.
        forget_cached_map(roots);
        let after_refresh = build_cached(
            sources.clone(),
            fs.clone(),
            rust_registry(cx),
            roots,
            &cache_dir,
        )
        .await;
        assert!(!after_refresh.from_memory);
        assert_eq!(after_refresh.reused, 3);
    }

    #[test]
    fn extracts_definitions_from_real_rust_source() {
        let language = languages::language("rust", tree_sitter_rust::LANGUAGE.into());
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/repo_map_panel.rs");
        let text = std::fs::read_to_string(&path).expect("read the panel source");
        let source = SourceFile {
            rel: RelPath::new(
                std::path::Path::new("src/repo_map_panel.rs"),
                path::PathStyle::local(),
            )
            .expect("valid path")
            .into_arc(),
            abs_path: path,
            size: text.len() as u64,
            modified_ns: None,
        };

        let parsed = parse_file(&source, &text, &language).expect("file parses");
        let names: Vec<&str> = parsed.symbols.iter().map(|symbol| &*symbol.name).collect();
        assert!(
            names.contains(&"RepoMapPanel"),
            "expected the panel type among {names:?}"
        );
        assert!(
            names.iter().any(|name| name.starts_with("push_dir_rows")),
            "expected a free function among {names:?}"
        );
        // The identifier walk should have collected plenty of mentions.
        assert!(
            parsed.mentions.len() > 50,
            "mentions: {}",
            parsed.mentions.len()
        );

        // Rust functions get a signature from their parameter list even though
        // they have no C-style declarator.
        let signature = parsed
            .symbols
            .iter()
            .find(|symbol| &*symbol.name == "push_dir_rows")
            .and_then(|symbol| symbol.signature.as_deref())
            .expect("push_dir_rows has a signature");
        assert!(signature.starts_with('('), "{signature}");

        // Kinds are normalized: no node names and no bare "symbol" fallbacks.
        let kinds: Vec<&str> = parsed.symbols.iter().map(|symbol| &*symbol.kind).collect();
        assert!(kinds.contains(&"struct"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"method"), "kinds: {kinds:?}");
        assert!(
            kinds
                .iter()
                .all(|kind| !kind.contains('_') && *kind != "symbol"),
            "kinds: {kinds:?}"
        );
    }

    #[test]
    fn select_skips_oversized_files_and_keeps_order() {
        let (kept, stats) = select(
            vec![
                source("a.rs", 10),
                source("huge.rs", MAX_FILE_BYTES + 1),
                source("b.rs", 10),
            ],
            |_| true,
        );
        let names: Vec<String> = kept
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert_eq!(names, ["a.rs", "b.rs"]);
        assert_eq!(stats.considered, 3);
        assert_eq!(stats.too_large, 1);
    }

    #[test]
    fn select_buckets_by_the_first_two_path_components() {
        // A monorepo keeps every crate under `crates/`: bucketing by the first
        // component alone would make one giant bucket, so each crate is a
        // bucket of its own. The bucket order follows the sorted paths, not the
        // order the caller walked them in.
        let (kept, _) = select(
            vec![
                source("crates/gpui/a.rs", 10),
                source("crates/gpui/b.rs", 10),
                source("crates/repo_wiki/c.rs", 10),
                source("src/root_file.rs", 10),
                source("ROOT.rs", 10),
            ],
            |_| true,
        );
        let names: Vec<String> = kept
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "ROOT.rs",
                "crates/gpui/a.rs",
                "crates/gpui/b.rs",
                "crates/repo_wiki/c.rs",
                "src/root_file.rs"
            ]
        );
    }

    #[test]
    fn select_shares_the_byte_budget_between_buckets() {
        // Both buckets hold more files at the per-file cap than the budget can
        // take. A first-come scan would empty the first bucket and then start
        // the second; the quota gives each bucket the same number.
        let per_bucket = 200u32;
        let mut sources = Vec::new();
        for bucket in ["crates/first", "crates/second"] {
            for index in 0..per_bucket {
                sources.push(source(&format!("{bucket}/f{index:03}.rs"), MAX_FILE_BYTES));
            }
        }
        let (kept, stats) = select(sources, |_| true);

        let count = |prefix: &str| {
            kept.iter()
                .filter(|file| file.rel.display(path::PathStyle::Unix).starts_with(prefix))
                .count()
        };
        let expected = (MAX_TOTAL_BYTES / MAX_FILE_BYTES) as usize / 2;
        assert_eq!(count("crates/first/"), count("crates/second/"));
        assert_eq!(count("crates/first/"), expected);
        assert_eq!(stats.over_budget, 2 * per_bucket as usize - kept.len());
    }

    #[test]
    fn a_demanding_bucket_cannot_crowd_out_a_small_one() {
        // The heavy bucket wants more files at the per-file cap than the budget
        // can take while the small bucket only wants ten tiny files. The tiny
        // bucket is offered its share before the heavy bucket can spend the
        // budget, so all ten fit.
        let mut sources = Vec::new();
        for index in 0..240u32 {
            sources.push(source(&format!("huge/f{index:03}.rs"), MAX_FILE_BYTES));
        }
        for index in 0..10u32 {
            sources.push(source(&format!("tiny/f{index:02}.rs"), 1_000));
        }
        let (kept, stats) = select(sources, |_| true);

        let count = |prefix: &str| {
            kept.iter()
                .filter(|file| file.rel.display(path::PathStyle::Unix).starts_with(prefix))
                .count()
        };
        assert_eq!(count("tiny/"), 10);
        assert_eq!(stats.over_budget, 240 - count("huge/"));
    }

    #[test]
    fn select_is_deterministic_for_any_input_order() {
        // The caller's walk order must not decide which files make the map. The
        // sizes are mixed and exceed the budget, so quotas and the tail really
        // matter here rather than every file simply fitting.
        let mut sources = Vec::new();
        for (bucket, count) in [("crates/alpha", 250u32), ("crates/beta", 250), ("docs", 60)] {
            for index in 0..count {
                let size = 1_000 + (u64::from(index) * 7_919) % MAX_FILE_BYTES;
                sources.push(source(&format!("{bucket}/file{index:03}.rs"), size));
            }
        }
        let baseline: Vec<String> = select(sources.clone(), |_| true)
            .0
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert!(
            baseline.len() < sources.len(),
            "the fixture must hit the budget, or this proves nothing"
        );

        for seed in 1..=32u64 {
            let mut shuffled = sources.clone();
            let mut state = seed;
            for index in (1..shuffled.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let swap = (state % (index as u64 + 1)) as usize;
                shuffled.swap(index, swap);
            }
            let names: Vec<String> = select(shuffled, |_| true)
                .0
                .iter()
                .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
                .collect();
            assert_eq!(names, baseline, "seed {seed} changed the selection");
        }
    }

    #[test]
    fn select_excludes_vendored_and_hidden_directories() {
        let (kept, stats) = select(
            vec![
                source("crates/repo_wiki/src/lib.rs", 10),
                source("crates/wu/resources/agents/node_modules/pi/index.js", 10),
                source("vendor/leftpad.c", 10),
                source("third_party/tree/sitter.c", 10),
                source("web/dist/app.js", 10),
                source("target/debug/build.rs", 10),
                source("python/.venv/lib/site-packages/x.py", 10),
                source("node_modules/pkg/top.js", 10),
                source(".hidden/file.rs", 10),
            ],
            |_| true,
        );
        let names: Vec<String> = kept
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert_eq!(names, ["crates/repo_wiki/src/lib.rs"]);
        assert_eq!(stats.vendored_skipped, 8);
        // Vendored paths are not candidates: they never reach the budget.
        assert_eq!(stats.considered, 1);
    }

    #[test]
    fn select_counts_files_without_a_language_as_unsupported() {
        let (kept, stats) = select(
            vec![
                source("src/lib.rs", 10),
                source("assets/logo.svg", 10),
                source("README.md", 10),
            ],
            |source| source.rel.extension() == Some("rs"),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(stats.considered, 3);
        assert_eq!(stats.unsupported, 2);
    }

    #[test]
    fn select_skips_a_file_that_does_not_fit_and_keeps_scanning() {
        // One bucket, in path order: the budget runs out before the last of the
        // 250 kB files, so they are skipped. The small file behind them still
        // fits, which a `break` on the budget would have lost.
        let fitting = (MAX_TOTAL_BYTES / 250_000) as usize;
        let mut sources: Vec<SourceFile> = (0..fitting + 32)
            .map(|index| source(&format!("bulk/f{index:03}.rs"), 250_000))
            .collect();
        sources.push(source("bulk/z_small.rs", 1_000));
        let (kept, stats) = select(sources, |_| true);

        assert_eq!(kept.len(), fitting + 1);
        assert_eq!(
            kept.last().unwrap().rel.display(path::PathStyle::Unix),
            "bulk/z_small.rs"
        );
        assert_eq!(stats.over_budget, 32);
        assert_eq!(stats.considered, fitting + 33);
    }

    #[test]
    fn select_stops_at_the_total_byte_budget() {
        // Many files at the per-file cap reach the total budget before the
        // file count cap does.
        let sources = (0..400u32)
            .map(|index| source(&format!("f{index:03}.rs"), MAX_FILE_BYTES))
            .collect::<Vec<_>>();
        let (kept, stats) = select(sources, |_| true);

        let total: u64 = kept.iter().map(|file| file.size).sum();
        assert!(total <= MAX_TOTAL_BYTES, "total was {total}");
        assert_eq!(kept.len(), (MAX_TOTAL_BYTES / MAX_FILE_BYTES) as usize);
        assert_eq!(stats.too_large, 0);
        assert_eq!(stats.over_budget, 400 - kept.len());
        assert_eq!(stats.considered, 400);
    }

    #[test]
    fn markdown_lists_ranked_files_and_respects_the_budget() {
        let map = SymbolMap {
            files: vec![
                file_symbols("src/lib.rs", 0.5, vec![entry("main", "function", 0)]),
                file_symbols("src/big.rs", 0.1, vec![entry("other", "function", 0)]),
            ],
            stats: BuildStats::default(),
        };

        let full = map.to_markdown(10_000);
        assert!(full.contains("src/lib.rs"));
        assert!(full.contains("function main"));
        assert!(full.find("src/lib.rs") < full.find("src/big.rs"));
        assert!(full.contains("Order: referencing fan-in"));

        // The byte budget is a last resort: the first file is always shown even
        // when it does not fit, and the rest are counted rather than listed.
        let tiny = map.to_markdown(10);
        assert!(tiny.contains("src/lib.rs"), "{tiny}");
        assert!(tiny.contains("Omitted files"), "{tiny}");
        assert!(tiny.contains("… +1 more omitted"), "{tiny}");
    }

    #[test]
    fn limit_pages_files_and_lists_the_ones_left_out() {
        let map = SymbolMap {
            files: (0..5)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (5 - index) as f32,
                        vec![entry(&format!("item{index}"), "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let first = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 2,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(first.contains("src/file0.rs"));
        assert!(first.contains("src/file1.rs"));
        assert!(!first.contains("`src/file2.rs`"), "{first}");
        // Omitted files keep their paths, so the agent can page to them.
        assert!(first.contains("Omitted files (3, paths only)"), "{first}");
        assert!(first.contains("Next page: offset=2"), "{first}");
        assert!(first.contains("src/file4.rs"), "{first}");

        let second = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 2,
                offset: 2,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(second.contains("`src/file2.rs`"), "{second}");
        assert!(!second.contains("`src/file0.rs`"), "{second}");
    }

    #[test]
    fn a_block_shows_at_most_forty_symbols_and_counts_the_rest() {
        let symbols = (0..MAX_SYMBOLS_PER_FILE_IN_MARKDOWN + 5)
            .map(|index| entry(&format!("item{index}"), "function", index as u32))
            .collect();
        let map = SymbolMap {
            files: vec![file_symbols("src/big.rs", 1., symbols)],
            stats: BuildStats::default(),
        };

        let text = map.to_markdown(100_000);
        assert_eq!(
            text.matches("- function item").count(),
            MAX_SYMBOLS_PER_FILE_IN_MARKDOWN,
            "{text}"
        );
        assert!(text.contains("… +5 more symbols"), "{text}");
        assert!(
            !text.contains("item44 (line 45)"),
            "the tail belongs to `symbols`, not the page: {text}"
        );

        // `files_only` lists no symbols, so it needs no note either.
        let files_only = map.repo_map_markdown(
            &RepoMapOptions {
                files_only: true,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(!files_only.contains("more symbols"), "{files_only}");
    }

    #[test]
    fn a_page_ended_by_the_budget_says_so_in_the_header() {
        let map = SymbolMap {
            files: (0..3)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (3 - index) as f32,
                        vec![entry(&format!("item{index}"), "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        // Room for one block and nothing more.
        let squeezed = map.repo_map_markdown(&RepoMapOptions::default(), 70);
        assert!(
            squeezed.contains("showing 1–1 of 3 files with definitions"),
            "{squeezed}"
        );
        assert!(squeezed.contains("output budget reached"), "{squeezed}");
        assert!(
            squeezed.contains("offset=1 for the next page"),
            "{squeezed}"
        );
        assert!(
            !squeezed.contains("Next page: offset=1"),
            "the header already carries the paging hint: {squeezed}"
        );

        // A page cut by `limit` is the page that was asked for.
        let paged = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 1,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(
            paged.contains("showing 1–1 of 3 files with definitions"),
            "{paged}"
        );
        assert!(!paged.contains("output budget reached"), "{paged}");
        assert!(paged.contains("Next page: offset=1, limit=1."), "{paged}");
    }

    #[test]
    fn an_offset_past_the_end_says_so_instead_of_reversing_the_range() {
        let map = SymbolMap {
            files: (0..3)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (3 - index) as f32,
                        vec![entry(&format!("item{index}"), "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let at_the_end = map.repo_map_markdown(
            &RepoMapOptions {
                offset: 3,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(
            at_the_end.contains("showing 0 of 3 files (offset 3 is past the end; use an offset below 3)"),
            "{at_the_end}"
        );
        assert!(!at_the_end.contains("showing 4–3"), "{at_the_end}");
        assert!(!at_the_end.contains("Omitted files"), "{at_the_end}");

        let beyond = map.repo_map_markdown(
            &RepoMapOptions {
                offset: 99,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(
            beyond.contains("offset 99 is past the end; use an offset below 3"),
            "{beyond}"
        );
    }

    #[test]
    fn every_page_fits_the_character_budget() {
        // 240 files with a dozen definitions each: the body and the omitted
        // list together used to cost more than the budget.
        let map = SymbolMap {
            files: (0..240)
                .map(|index| {
                    file_symbols(
                        &format!("src/module{}/file{index}.rs", index % 12),
                        (240 - index) as f32,
                        (0..12)
                            .map(|symbol| entry(&format!("item{symbol}"), "function", symbol))
                            .collect(),
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        for limit in [10usize, 20, 40] {
            let text = map.repo_map_markdown(
                &RepoMapOptions {
                    limit,
                    ..RepoMapOptions::default()
                },
                MAX_REPO_MAP_CHARS,
            );
            assert!(
                text.chars().count() <= MAX_REPO_MAP_CHARS,
                "limit {limit} wrote {} characters",
                text.chars().count()
            );
        }

        // `files_only` pages the same way and obeys the same budget.
        let files_only = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 240,
                files_only: true,
                ..RepoMapOptions::default()
            },
            MAX_REPO_MAP_CHARS,
        );
        assert!(
            files_only.chars().count() <= MAX_REPO_MAP_CHARS,
            "files_only wrote {} characters",
            files_only.chars().count()
        );
    }

    #[test]
    fn the_omitted_list_stops_at_twenty_paths_and_counts_the_rest() {
        let map = SymbolMap {
            files: (0..250)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (250 - index) as f32,
                        vec![entry("item", "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let text = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 40,
                ..RepoMapOptions::default()
            },
            1_000_000,
        );
        assert!(text.contains("Omitted files (210, paths only)"), "{text}");
        let listed = text
            .lines()
            .filter(|line| line.starts_with("src/file"))
            .count();
        // The page's own file is written in backticks, so only list paths match.
        assert_eq!(listed, MAX_OMITTED_PATHS, "{text}");
        assert!(text.contains("`src/file0.rs`"), "{text}");
        assert!(
            text.contains(&format!(
                "… +{} more omitted (call files_only or raise offset to list them)",
                210 - MAX_OMITTED_PATHS
            )),
            "{text}"
        );
    }

    #[test]
    fn a_one_file_page_lists_half_as_many_omitted_paths() {
        let map = SymbolMap {
            files: (0..250)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (250 - index) as f32,
                        vec![entry("item", "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let text = map.repo_map_markdown(
            &RepoMapOptions {
                limit: 1,
                ..RepoMapOptions::default()
            },
            1_000_000,
        );
        let listed = text
            .lines()
            .filter(|line| line.starts_with("src/file"))
            .count();
        assert_eq!(listed, MAX_OMITTED_PATHS_SMALL, "{text}");
        assert!(
            text.contains(&format!(
                "… +{} more omitted (call files_only or raise offset to list them)",
                249 - MAX_OMITTED_PATHS_SMALL
            )),
            "{text}"
        );
        // `limit` picks the cap; a larger page gets the larger cap.
        assert_eq!(omitted_path_limit(5), MAX_OMITTED_PATHS_SMALL);
        assert_eq!(omitted_path_limit(6), MAX_OMITTED_PATHS);
    }

    #[test]
    fn the_header_states_coverage_and_excluded_paths() {
        let map = SymbolMap {
            files: vec![file_symbols(
                "src/lib.rs",
                1.,
                vec![entry("main", "function", 0)],
            )],
            stats: BuildStats {
                considered: 3_368,
                parsed: 812,
                too_large: 60,
                unsupported: 1_310,
                unreadable: 2,
                over_budget: 98,
                file_limit: 1_086,
                vendored_skipped: 16_189,
            },
        };

        let text = map.to_markdown(100_000);
        assert!(
            text.contains(
                "Parsed 812 / 3368 candidate files (1310 unsupported language, 60 over 1024 KiB, \
                 98 over the byte budget, 1086 beyond the 3000-file limit, 2 unreadable); \
                 1 with definitions."
            ),
            "{text}"
        );
        assert!(
            text.contains("Excluded before scanning: 16189 vendored or generated paths."),
            "{text}"
        );
    }

    #[test]
    fn an_empty_page_prints_no_coverage_lines() {
        let map = SymbolMap {
            files: vec![file_symbols(
                "src/lib.rs",
                1.,
                vec![entry("main", "function", 0)],
            )],
            stats: BuildStats::default(),
        };
        let text = map.to_markdown(100_000);
        assert!(!text.contains("candidate files"), "{text}");
        assert!(!text.contains("Excluded before scanning"), "{text}");
    }

    #[test]
    fn files_only_lists_paths_and_line_counts_without_symbols() {
        let map = SymbolMap {
            files: vec![file_symbols(
                "src/lib.rs",
                1.,
                vec![entry("main", "function", 0)],
            )],
            stats: BuildStats::default(),
        };
        let text = map.repo_map_markdown(
            &RepoMapOptions {
                files_only: true,
                ..RepoMapOptions::default()
            },
            100_000,
        );
        assert!(
            text.contains("`src/lib.rs` (in 0 / out 0 / deps 0, 1 lines)"),
            "{text}"
        );
        assert!(!text.contains("function main"), "{text}");
    }

    #[test]
    fn local_variables_are_hidden_unless_requested() {
        let symbols = vec![
            entry("find", "method", 0),
            SymbolEntry {
                local: true,
                owner: Some("find".into()),
                depth: 2,
                ..entry("i", "variable", 3)
            },
        ];
        let hidden = symbol_tree_lines(&symbols, false);
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        let shown = symbol_tree_lines(&symbols, true);
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown[1].contains("variable i"), "{shown:?}");
    }

    #[test]
    fn consecutive_variables_fold_but_different_scopes_do_not() {
        let variable = |name: &str, row: u32, owner: &str| SymbolEntry {
            local: true,
            owner: Some(owner.into()),
            depth: 2,
            ..entry(name, "variable", row)
        };
        let symbols = vec![
            variable("a", 0, "first"),
            variable("b", 1, "first"),
            variable("c", 2, "first"),
            variable("d", 3, "second"),
        ];
        let lines = symbol_tree_lines(&symbols, true);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("variable x3 (lines 1–3)"), "{lines:?}");
        assert!(lines[1].contains("variable d"), "{lines:?}");
    }

    fn cpp_symbols(text: &str) -> Vec<SymbolEntry> {
        let language = languages::language("cpp", tree_sitter_cpp::LANGUAGE.into());
        let grammar = language.grammar().expect("cpp grammar is built in");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar.ts_language)
            .expect("cpp is a parseable language");
        let tree = parser.parse(text, None).expect("cpp source parses");
        collect_symbols(grammar.outline_config.as_ref(), tree.root_node(), text)
    }

    fn kind_of<'a>(symbols: &'a [SymbolEntry], name: &str) -> &'a str {
        symbols
            .iter()
            .find(|symbol| &*symbol.name == name)
            .map_or("<missing>", |symbol| &symbol.kind)
    }

    #[test]
    fn a_cpp_sorting_function_yields_one_function_and_no_parameters() {
        let symbols = cpp_symbols(
            r#"
void insertionSort(int arr[], int n) {
    int i, key, j;
    for (i = 1; i < n; i++) {
        key = arr[i];
        j = i - 1;
        while (j >= 0 && arr[j] > key) {
            arr[j + 1] = arr[j];
            j = j - 1;
        }
        arr[j + 1] = key;
    }
}
"#,
        );

        let functions: Vec<&SymbolEntry> = symbols
            .iter()
            .filter(|symbol| &*symbol.kind == "function")
            .collect();
        assert_eq!(functions.len(), 1, "symbols: {symbols:?}");
        assert_eq!(&*functions[0].name, "insertionSort");
        // The parameters are the loudest source of noise in C and C++.
        assert!(
            !symbols
                .iter()
                .any(|symbol| &*symbol.name == "arr" || &*symbol.name == "n"),
            "parameters leaked into {symbols:?}"
        );
        // Locals stay, but with a readable kind instead of `init_declarator`.
        assert_eq!(kind_of(&symbols, "i"), "variable");
        assert!(
            symbols
                .iter()
                .all(|symbol| !symbol.kind.contains("declarator"))
        );
    }

    #[test]
    fn names_inside_a_parameter_list_are_not_definitions() {
        let text = "void sort(int *values, int size);";
        let language = languages::language("cpp", tree_sitter_cpp::LANGUAGE.into());
        let grammar = language.grammar().expect("cpp grammar is built in");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar.ts_language)
            .expect("cpp is a parseable language");
        let tree = parser.parse(text, None).expect("cpp source parses");
        let root = tree.root_node();
        let node_at = |needle: &str| {
            let start = text.find(needle).expect("needle is in the source");
            root.descendant_for_byte_range(start, start + needle.len())
                .expect("node covers the needle")
        };

        assert!(is_inside_parameter_list(node_at("size")));
        assert!(!is_inside_parameter_list(node_at("sort")));
    }

    #[test]
    fn cpp_definitions_get_readable_kinds() {
        let symbols = cpp_symbols(
            r#"
#define LIMIT 10
namespace sorting {
typedef int Count;
enum Mode { Ascending, Descending };
class Sorter {
 public:
  int count;
  void sort(int *values, int size);
};
int helper(int value);
}
"#,
        );

        assert_eq!(kind_of(&symbols, "LIMIT"), "macro");
        assert_eq!(kind_of(&symbols, "sorting"), "module");
        assert_eq!(kind_of(&symbols, "Count"), "type");
        assert_eq!(kind_of(&symbols, "Mode"), "enum");
        assert_eq!(kind_of(&symbols, "Ascending"), "variant");
        assert_eq!(kind_of(&symbols, "Sorter"), "class");
        assert_eq!(kind_of(&symbols, "count"), "field");
        assert_eq!(kind_of(&symbols, "sort"), "method");
        assert_eq!(kind_of(&symbols, "helper"), "function");
        // No raw tree-sitter node name leaks into the panel.
        assert!(
            symbols.iter().all(|symbol| !symbol.kind.contains('_')),
            "symbols: {symbols:?}"
        );
    }

    #[test]
    fn repeated_names_collapse_within_a_definition_only() {
        let symbols = cpp_symbols(
            r#"
void first() {
    int i = 0;
    for (int i = 0; i < 2; i++) {
        int i = 3;
    }
}
void second() {
    int i = 0;
}
"#,
        );

        let count = |name: &str| {
            symbols
                .iter()
                .filter(|symbol| &*symbol.name == name)
                .count()
        };
        assert_eq!(count("i"), 2, "one `i` per function body: {symbols:?}");
        assert_eq!(count("first"), 1);
        assert_eq!(count("second"), 1);
    }

    #[test]
    fn the_symbol_cap_keeps_shallow_definitions_first() {
        let candidates: Vec<Candidate> = (0..(MAX_SYMBOLS_PER_FILE + 20))
            .map(|index| Candidate {
                name: format!("deep_{index}").into(),
                kind: "variable",
                row: index as u32,
                tree_depth: 4,
                owner: None,
                depth: 4,
                access: None,
                signature: None,
                local: true,
            })
            .chain([Candidate {
                name: "top_level".into(),
                kind: "function",
                row: (MAX_SYMBOLS_PER_FILE + 40) as u32,
                tree_depth: 1,
                owner: None,
                depth: 0,
                access: None,
                signature: None,
                local: false,
            }])
            .collect();

        let symbols = finish_symbols(candidates);
        assert_eq!(symbols.len(), MAX_SYMBOLS_PER_FILE);
        // A late top-level definition survives; only the deepest tail is cut.
        assert!(symbols.iter().any(|symbol| &*symbol.name == "top_level"));
        assert!(
            !symbols
                .iter()
                .any(|symbol| &*symbol.name == format!("deep_{}", MAX_SYMBOLS_PER_FILE + 19))
        );
        // Whatever the cap kept stays ordered by line for the panel.
        assert!(symbols.windows(2).all(|pair| pair[0].row <= pair[1].row));
    }

    #[test]
    fn a_parenthesized_initializer_is_a_variable_not_a_function() {
        // The exact false positive from Pi's C++ exercise project: a value list
        // that C++'s most vexing parse turns into a parameter list.
        let symbols = cpp_symbols(
            r#"
void f() {
    bool total = false;
    vector<bool> visited(total, false);
}
"#,
        );

        assert_eq!(
            kind_of(&symbols, "visited"),
            "variable",
            "symbols: {symbols:?}"
        );
        let visited = symbols
            .iter()
            .find(|symbol| &*symbol.name == "visited")
            .expect("visited is listed");
        assert_eq!(visited.signature.as_deref(), Some(": vector<bool>"));
        assert!(visited.local, "visited lives in f's body");
    }

    #[test]
    fn pointer_declarators_report_the_bare_name_and_their_type() {
        let symbols = cpp_symbols(
            r#"
void f() {
    ListNode dummy;
    ListNode *cur = &dummy;
    ListNode &alias = dummy;
}
"#,
        );

        let cur = symbols
            .iter()
            .find(|symbol| &*symbol.name == "cur")
            .expect("the pointer's name is `cur`, not `*cur`: {symbols:?}");
        assert_eq!(&*cur.kind, "variable");
        assert_eq!(cur.signature.as_deref(), Some(": ListNode*"));
        let alias = symbols
            .iter()
            .find(|symbol| &*symbol.name == "alias")
            .expect("aliases keep their name");
        assert_eq!(alias.signature.as_deref(), Some(": ListNode&"));
    }

    #[test]
    fn function_signatures_keep_pointer_returns() {
        let symbols = cpp_symbols(
            r#"
TreeNode *make_node(int value);
TreeNode **make_handle();
"#,
        );

        let make_node = symbols
            .iter()
            .find(|symbol| &*symbol.name == "make_node")
            .expect("the pointer-returning function is listed");
        assert_eq!(make_node.signature.as_deref(), Some("(int value) -> TreeNode*"));

        let make_handle = symbols
            .iter()
            .find(|symbol| &*symbol.name == "make_handle")
            .expect("the double pointer is listed");
        assert_eq!(make_handle.signature.as_deref(), Some("() -> TreeNode**"));
    }

    #[test]
    fn array_fields_are_listed_with_their_type() {
        let symbols = cpp_symbols(
            r#"
struct Bucket {
    int data[MAX];
    int *pointer;
};
"#,
        );

        let data = symbols
            .iter()
            .find(|symbol| &*symbol.name == "data")
            .expect("the array field is listed");
        assert_eq!(&*data.kind, "field");
        assert_eq!(data.signature.as_deref(), Some(": int"));
    }

    #[test]
    fn overloads_survive_collapsing_while_prototypes_and_macros_collapse() {
        let symbols = cpp_symbols(
            r#"
vector<int> BFSShortestPath(const Group& g, int s);
vector<int> BFSShortestPath(const Group& g, int s, int t);
vector<int> BFSShortestPath(const Group& g, int s) { return {}; }
TEST(Group, 构造与初始化) { }
TEST(DFS, 遍历) { }
"#,
        );

        let overloads: Vec<&str> = symbols
            .iter()
            .filter(|symbol| &*symbol.name == "BFSShortestPath")
            .filter_map(|symbol| symbol.signature.as_deref())
            .collect();
        assert_eq!(
            overloads,
            [
                "(const Group& g, int s) -> vector<int>",
                "(const Group& g, int s, int t) -> vector<int>"
            ],
            "symbols: {symbols:?}"
        );
        // Macro invocations are not overloads: they stay a single line.
        assert_eq!(
            symbols
                .iter()
                .filter(|symbol| &*symbol.name == "TEST")
                .count(),
            1,
            "symbols: {symbols:?}"
        );
    }

    #[test]
    fn class_members_carry_owner_access_and_signature() {
        let text = r#"
class UnionFind {
public:
    explicit UnionFind(int n);
    int find(int x);
    bool connected(int x, int y) { return find(x) == find(y); }
protected:
    std::vector<int> parent_;
    std::vector<int> rank_;
};
"#;
        let symbols = cpp_symbols(text);

        let find = symbols
            .iter()
            .find(|symbol| &*symbol.name == "find")
            .expect("find is listed");
        assert_eq!(&*find.kind, "method");
        assert_eq!(find.owner.as_deref(), Some("UnionFind"));
        assert_eq!(find.access.as_deref(), Some("public"));
        assert_eq!(find.signature.as_deref(), Some("(int x) -> int"));
        assert!(find.depth > 0);

        let constructor = symbols
            .iter()
            .find(|symbol| &*symbol.name == "UnionFind" && &*symbol.kind == "method")
            .unwrap_or_else(|| panic!("the constructor is listed: {symbols:?}"));
        assert_eq!(constructor.signature.as_deref(), Some("(int n)"));

        let parent = symbols
            .iter()
            .find(|symbol| &*symbol.name == "parent_")
            .expect("the field is listed");
        assert_eq!(&*parent.kind, "field");
        assert_eq!(parent.access.as_deref(), Some("protected"));
        assert_eq!(parent.signature.as_deref(), Some(": std::vector<int>"));

        // Rendered as the tree the panel and the agent both read.
        let lines = symbol_tree_lines(&symbols, false);
        let rendered = lines.join("\n");
        assert!(rendered.contains("class UnionFind"), "{rendered}");
        assert!(rendered.contains("public:"), "{rendered}");
        assert!(rendered.contains("method find(int x) -> int"), "{rendered}");
        assert!(rendered.contains("protected:"), "{rendered}");
        assert!(
            rendered.find("public:").unwrap() < rendered.find("method find").unwrap(),
            "{rendered}"
        );
        // Locals never show up by default, and this class body has none.
        assert!(
            symbols.iter().all(|symbol| !symbol.local),
            "members are not locals: {symbols:?}"
        );
    }

    #[test]
    fn locals_stay_out_of_the_default_rendering() {
        let text = r#"
vector<int> bfs(int start) {
    queue<int> pending;
    int u = 0;
    int v = 1;
    int w = 2;
    return {};
}
"#;
        let symbols = cpp_symbols(text);
        let locals: Vec<&str> = symbols
            .iter()
            .filter(|symbol| symbol.local)
            .map(|symbol| &*symbol.name)
            .collect();
        assert_eq!(locals, ["pending", "u", "v", "w"], "{symbols:?}");

        let hidden = symbol_tree_lines(&symbols, false);
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert!(hidden[0].contains("function bfs"), "{hidden:?}");
        assert_eq!(
            symbols
                .iter()
                .find(|symbol| &*symbol.name == "bfs")
                .and_then(|symbol| symbol.signature.as_deref()),
            Some("(int start) -> vector<int>")
        );

        let shown = symbol_tree_lines(&symbols, true);
        let rendered = shown.join("\n");
        assert!(rendered.contains("variable x4 (lines"), "{rendered}");
    }

    #[test]
    fn degrees_count_distinct_files_on_the_ranked_edges() {
        let mut files = vec![
            file("owner.rs", &["rare_api"], &[]),
            file("user_a.rs", &["a"], &["rare_api"]),
            file("user_b.rs", &["b"], &["rare_api"]),
        ];
        rank(&mut files);

        assert_eq!(files[0].in_degree, 2, "two files reference the definition");
        assert_eq!(files[0].out_degree, 0);
        assert_eq!(files[1].out_degree, 1);
        assert_eq!(files[1].in_degree, 0);
    }

    #[test]
    fn a_name_that_too_many_files_mention_makes_no_edge() {
        let callers = |count: usize| -> Vec<ParsedFile> {
            (0..count)
                .map(|index| file(&format!("caller{index}.rs"), &[], &["refresh"]))
                .collect()
        };
        let with_owner = |count: usize| -> Vec<ParsedFile> {
            let mut files = callers(count);
            files.push(file("owner.rs", &["refresh"], &[]));
            files
        };

        // Four files put the cap at its floor of three. At the cap the name is
        // still evidence; one more caller makes every mention vocabulary.
        let mut at_the_cap = with_owner(3);
        rank(&mut at_the_cap);
        assert_eq!(at_the_cap.last().unwrap().in_degree, 3);

        let mut past_the_cap = with_owner(4);
        rank(&mut past_the_cap);
        let owner = past_the_cap.last().unwrap();
        assert_eq!(owner.in_degree, 0, "the name is no evidence");
        assert_eq!(past_the_cap[0].out_degree, 0, "the callers gain no edge");
    }

    #[test]
    fn shared_name_limit_scales_with_the_repository() {
        assert_eq!(shared_name_file_limit(1), 3);
        assert_eq!(shared_name_file_limit(113), 3);
        assert_eq!(shared_name_file_limit(1_565), 31);
        assert_eq!(shared_name_file_limit(10_000), 50);
    }

    #[test]
    fn a_data_file_takes_no_part_in_the_reference_graph() {
        // JSON keys and Markdown headings are definitions of the generic
        // `symbol` kind; their names are spelled like the code that consumes
        // them, which is a coincidence rather than a dependency.
        let mut keys = file("assets/settings/default.json", &["dev", "light"], &[]);
        for symbol in &mut keys.symbols {
            symbol.kind = "symbol".into();
        }
        let mut files = vec![
            keys,
            file("src/settings.rs", &["Settings"], &[]),
            file("src/theme.rs", &["Theme"], &["dev", "light"]),
        ];
        rank(&mut files);

        assert_eq!(files[0].in_degree, 0, "no code references the keys");
        assert_eq!(files[0].out_degree, 0);
        // The mentioning side is ignored too, so `dev` gives `theme.rs` no edge
        // even though the data file defines it.
        assert_eq!(files[2].out_degree, 0);
    }

    #[test]
    fn only_callable_and_type_names_own_an_edge() {
        // A field, a variant and a local are spellings inside something, not an
        // API another file can name, so a mention of one proves nothing.
        let mut owner = file("settings.rs", &["refresh"], &[]);
        owner.symbols = vec![
            entry("menu", "field", 0),
            entry("open", "variant", 1),
            entry("refresh", "function", 2),
        ];
        let mut files = vec![owner, file("caller.rs", &["caller_api"], &["menu", "open", "refresh"])];
        rank(&mut files);

        // Only `refresh` is an owner, so the caller has exactly one edge.
        assert_eq!(files[1].out_degree, 1);
        assert_eq!(files[0].in_degree, 1);

        let mut fields_only = file("settings.rs", &[], &[]);
        fields_only.symbols = vec![entry("menu", "field", 0), entry("open", "variant", 1)];
        let mut files = vec![
            fields_only,
            file("caller.rs", &["caller_api"], &["menu", "open"]),
        ];
        rank(&mut files);
        assert_eq!(files[1].out_degree, 0, "no owner, no edge");
        assert_eq!(files[0].in_degree, 0);
    }

    #[test]
    fn dependency_specs_resolve_only_to_paths_that_exist() {
        let mut lib = file("crates/a/src/lib.rs", &["a"], &[]);
        lib.includes = vec![
            "mod:store".to_owned(),
            "mod:util".to_owned(),
            "mod:missing".to_owned(),
            "use:crate::util::Helper".to_owned(),
            "use:serde::Deserialize".to_owned(),
            "use:util::Thing".to_owned(),
        ];
        let mut store = file("crates/a/src/store.rs", &["b"], &[]);
        store.includes = vec!["mod:io".to_owned()];
        let util = file("crates/a/src/util.rs", &["c"], &[]);
        let io = file("crates/a/src/store/io.rs", &["d"], &[]);
        let mut page = file_in_family("web/app.tsx", "javascript", &["App"], &[]);
        page.includes = vec!["import:./theme".to_owned(), "import:react".to_owned()];
        let theme = file_in_family("web/theme.ts", "javascript", &["Theme"], &[]);

        let mut files = vec![lib, store, util, io, page, theme];
        resolve_dependencies(&mut files);

        // `mod:store` uses the `foo.rs` layout, `mod:io` the `foo/` layout, and
        // `use:crate::util::Helper` names a type in `util.rs`.
        assert_eq!(files[1].dep_in_degree, 1, "store.rs is declared by lib.rs");
        assert_eq!(files[2].dep_in_degree, 1, "util.rs is used by lib.rs");
        assert_eq!(files[3].dep_in_degree, 1, "store/io.rs is declared by store.rs");
        assert_eq!(files[5].dep_in_degree, 1, "theme.ts is imported by app.tsx");
        // Missing files, external crates and bare crate names create nothing.
        assert_eq!(files[0].dep_in_degree, 0);
    }

    #[test]
    fn a_name_only_counts_inside_one_language_family() {
        let mut files = vec![
            file_in_family("website/en.ts", "javascript", &["Menu"], &[]),
            file_in_family("src/ui.rs", "rust", &["Label"], &["Menu"]),
            file_in_family("website/app.tsx", "javascript", &["App"], &["Menu"]),
        ];
        rank(&mut files);

        // The Rust file writes `Menu` but means nothing the TypeScript file owns.
        assert_eq!(files[0].in_degree, 1, "only the TypeScript caller counts");
        assert_eq!(files[1].out_degree, 0);
        assert_eq!(files[2].out_degree, 1);
    }

    #[test]
    fn a_distinctive_name_carries_more_rank_than_a_shared_one() {
        let mut files = vec![
            file("rare_owner.rs", &["rare_api"], &[]),
            file("shared_owner_a.rs", &["shared_api"], &[]),
            file("shared_owner_b.rs", &["shared_api"], &[]),
            file("caller_rare.rs", &["caller_a"], &["rare_api"]),
            file("caller_shared.rs", &["caller_b"], &["shared_api"]),
        ];
        rank(&mut files);

        let rank_of = |name: &str| {
            files
                .iter()
                .find(|file| file.rel.as_std_path().to_string_lossy() == name)
                .expect("file present")
                .rank
        };
        // One mention of a name only one file owns is worth a whole unit; one
        // mention of a name two files own is worth half to each. Counting
        // mentions instead of weighting them would make these three equal. The
        // gap is capped below 2 by the damping factor: every page keeps a floor
        // of teleport mass that the half weight does not shrink.
        let shared = rank_of("shared_owner_a.rs");
        assert!(
            (shared - rank_of("shared_owner_b.rs")).abs() < 1e-6,
            "the half weights are symmetric"
        );
        assert!(rank_of("rare_owner.rs") > 1.25 * shared, "{files:?}");
    }

    #[test]
    fn json_export_uses_one_based_lines_and_relative_paths() {
        let map = SymbolMap {
            files: vec![file_symbols(
                "src/lib.rs",
                1.,
                vec![entry("foo", "function", 11)],
            )],
            stats: BuildStats::default(),
        };

        let json = serde_json::to_string(&map.json_entries()).expect("values serialize");
        let actual: serde_json::Value = serde_json::from_str(&json).expect("json parses");
        let file = &actual[0];
        assert_eq!(file["path"], "src/lib.rs");
        assert_eq!(file["lines"], 1);
        assert_eq!(file["in"], 0);
        assert_eq!(file["out"], 0);
        // The clipboard export and the snapshot share these field names.
        assert_eq!(file["symbols"][0]["name"], "foo");
        assert_eq!(file["symbols"][0]["line"], 12);
        assert_eq!(file["symbols"][0]["kind"], "function");
        assert_eq!(file["symbols"][0]["local"], false);
        assert_eq!(file["symbols"][0]["depth"], 0);
    }

    #[test]
    fn digest_shows_top_level_symbols_before_members_and_hides_locals() {
        let mut cache = entry("cache", "variable", 30);
        cache.local = true;
        let map = SymbolMap {
            files: vec![file_symbols(
                "src/game.cpp",
                0.9,
                vec![
                    entry("Game", "class", 1),
                    entry("board", "field", 4),
                    entry("start", "method", 9),
                    cache,
                    entry("main", "function", 40),
                ],
            )],
            stats: BuildStats::default(),
        };

        let digest = map.to_digest(20, 4, MAX_DIGEST_CHARS);
        let line = digest.lines().nth(1).expect("one file line");
        assert_eq!(
            line,
            "`src/game.cpp` — class Game (2)、function main (41)、field board (5)、method start (10)"
        );
        assert!(!digest.contains("cache"), "{digest}");
    }

    #[test]
    fn digest_lists_only_the_requested_files() {
        let map = SymbolMap {
            files: (0..30)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index:02}.rs"),
                        (30 - index) as f32,
                        vec![entry(&format!("item{index}"), "function", 0)],
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let digest = map.to_digest(20, 5, MAX_DIGEST_CHARS);
        assert!(digest.contains("共 30 个文件"), "{digest}");
        assert!(digest.contains("`src/file19.rs`"), "{digest}");
        assert!(!digest.contains("`src/file20.rs`"), "{digest}");
        assert!(digest.contains("…另有 10 个文件"), "{digest}");
    }

    #[test]
    fn digest_stops_at_the_budget_and_counts_the_rest() {
        let map = SymbolMap {
            files: (0..10)
                .map(|index| {
                    file_symbols(
                        &format!("src/file{index}.rs"),
                        (10 - index) as f32,
                        (0..6)
                            .map(|symbol| {
                                entry(&format!("item_{index}_{symbol}"), "function", symbol)
                            })
                            .collect(),
                    )
                })
                .collect(),
            stats: BuildStats::default(),
        };

        let digest = map.to_digest(10, 5, 400);
        assert!(digest.chars().count() <= 400, "{digest}");
        let listed = digest.lines().filter(|line| line.starts_with('`')).count();
        assert!(listed > 0 && listed < 10, "{digest}");
        assert!(
            digest.contains(&format!("…另有 {} 个文件", 10 - listed)),
            "{digest}"
        );
    }

    #[test]
    fn digest_handles_zero_limits() {
        let map = SymbolMap {
            files: vec![
                file_symbols("src/a.rs", 3., vec![entry("a", "function", 0)]),
                file_symbols("src/b.rs", 2., vec![entry("b", "function", 0)]),
                file_symbols("src/c.rs", 1., vec![entry("c", "function", 0)]),
            ],
            stats: BuildStats::default(),
        };

        let no_files = map.to_digest(0, 5, MAX_DIGEST_CHARS);
        assert!(
            !no_files.lines().any(|line| line.starts_with("`src/")),
            "{no_files}"
        );
        assert!(no_files.contains("…另有 3 个文件"), "{no_files}");

        let no_symbols = map.to_digest(3, 0, MAX_DIGEST_CHARS);
        assert!(
            no_symbols.lines().any(|line| line == "`src/a.rs`"),
            "{no_symbols}"
        );
    }

    #[test]
    fn an_empty_map_asks_for_the_repository_map_panel() {
        let digest = SymbolMap::default().to_digest(20, 5, MAX_DIGEST_CHARS);
        assert_eq!(
            digest,
            "这个项目还没有可用的仓库地图（请先在 WuIDE 里打开一次 Repository Map 面板）"
        );
    }
}





