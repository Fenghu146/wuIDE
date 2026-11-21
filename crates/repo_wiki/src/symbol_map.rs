//! Symbol-level repository map.
//!
//! Every file is parsed once with tree-sitter using the grammar's outline
//! query (the same queries the outline panel uses, so no per-language work is
//! needed here). Definitions become the file's symbol list; identifiers become
//! edges in a cross-file reference graph, which is ranked with PageRank so the
//! files that the rest of the repository talks about float to the top.
//!
//! The whole thing is a bounded, cancellable background job: it never runs
//! unless the repo map panel is opened or `/repo-map` is used, and it drops
//! its task as soon as the caller goes away.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language::{Language, LanguageRegistry};
use path::rel_path::RelPath;
use project::Project;
use streaming_iterator::StreamingIterator as _;

/// Upper bound on how many files one build parses. Parsing and walking a file
/// costs single-digit milliseconds, so this keeps a build in the low seconds
/// on the background thread.
pub const MAX_FILES: usize = 900;
/// Files larger than this are skipped: they are almost always generated or
/// vendored data, and parsing them dominates the runtime.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Cap on identifiers collected per file, which bounds the reference graph.
const MAX_IDENTIFIERS_PER_FILE: usize = 4_000;
/// Cap on symbols kept per file.
const MAX_SYMBOLS_PER_FILE: usize = 200;
/// Cap on the total bytes parsed in one build, so the work stays bounded even
/// when a repository has many medium-sized files.
const MAX_TOTAL_BYTES: u64 = 24 * 1024 * 1024;
const PAGERANK_ITERATIONS: usize = 24;
const PAGERANK_DAMPING: f32 = 0.85;

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Files including or importing this one.
    pub in_degree: u32,
    /// Files this one includes or imports.
    pub out_degree: u32,
    pub symbols: Vec<SymbolEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct BuildStats {
    /// Files handed to the builder, excluding those it could not parse at all.
    pub considered: usize,
    /// Files actually parsed and kept.
    pub parsed: usize,
    /// Files skipped because they were too large.
    pub too_large: usize,
    /// Files skipped because no language (or no grammar) matched.
    pub unsupported: usize,
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
        "Order: include fan-in (desc), then reference PageRank (desc), then path. \
         `in N / out M` counts `#include`, `use` and `import` edges to other files."
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
                "这个项目还没有可用的仓库地图（请先在 IDE 里打开一次 Repository Map 面板）",
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
    /// `limit` and `offset` page over the files that define something; the
    /// character budget only trims the tail and every file left out is listed
    /// by path at the end, so an agent always knows what exists.
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

        // The budget is a last resort: `limit` decides how many files are
        // shown, so only a pathological file list can shorten the page. The
        // body is built first so the header can state what was really written.
        let (body, written) = render_ranked(&files, start, end, max_chars, |file| {
            self.file_block(file, options)
        });

        let mut out = format!(
            "## Repository map\n\n\
             {total} files with definitions, showing {}–{written}.\n{}\n",
            start + 1,
            Self::order_description()
        );
        if let Some(truncated) = self.truncation_note() {
            out.push_str(&truncated);
            out.push('\n');
        }
        out.push_str(&body);

        let skipped = total.saturating_sub(written);
        if skipped > 0 {
            out.push_str(&format!("\nOmitted files ({skipped}, paths only):\n"));
            for file in files[written..].iter() {
                out.push_str(&format!("{}\n", file.rel.display(path::PathStyle::Unix)));
            }
            if written > start {
                out.push_str(&format!(
                    "\nNext page: offset={written}, limit={}.\n",
                    options.limit
                ));
            }
        }
        if options.include_locals {
            out.push_str("\nIncludes local variables.\n");
        }
        out
    }

    fn truncation_note(&self) -> Option<String> {
        let skipped = self.stats.considered.saturating_sub(self.stats.parsed);
        if skipped == 0 {
            return None;
        }
        Some(format!(
            "{skipped} candidate files were not indexed ({} too large, {} unsupported language).",
            self.stats.too_large, self.stats.unsupported
        ))
    }

    fn file_block(&self, file: &FileSymbols, options: &RepoMapOptions) -> String {
        let mut block = format!(
            "\n`{}` (in {} / out {}, {} lines)\n",
            file.rel.display(path::PathStyle::Unix),
            file.in_degree,
            file.out_degree,
            file.lines
        );
        if options.files_only {
            return block;
        }
        for line in symbol_tree_lines(&file.symbols, options.include_locals) {
            block.push_str(&line);
            block.push('\n');
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
}

/// Applies the size and count budgets to a candidate list.
///
/// Taking the first N files in path order would only ever describe the
/// alphabetically-first corner of a repository, so candidates are bucketed by
/// their first path component and picked round-robin, largest first inside
/// each bucket. Every top-level module gets represented before any bucket gets
/// a second helping.
fn select(sources: Vec<SourceFile>) -> (Vec<SourceFile>, BuildStats) {
    let mut stats = BuildStats {
        considered: sources.len(),
        ..BuildStats::default()
    };

    let mut buckets: Vec<(String, Vec<SourceFile>)> = Vec::new();
    let mut bucket_index: HashMap<String, usize> = HashMap::new();
    let mut total_bytes = 0u64;
    let mut kept = 0usize;
    for source in sources {
        if source.size > MAX_FILE_BYTES {
            stats.too_large += 1;
            continue;
        }
        if total_bytes.saturating_add(source.size) > MAX_TOTAL_BYTES {
            break;
        }
        total_bytes += source.size;
        kept += 1;
        let key = source
            .rel
            .components()
            .next()
            .unwrap_or_default()
            .to_string();
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

    for (_, bucket) in &mut buckets {
        bucket.sort_by_key(|source| std::cmp::Reverse(source.size));
    }

    let mut chosen = Vec::with_capacity(kept.min(MAX_FILES));
    let mut round = 0usize;
    'outer: loop {
        let mut added_any = false;
        for (_, bucket) in &buckets {
            let Some(source) = bucket.get(round) else {
                continue;
            };
            added_any = true;
            chosen.push(source.clone());
            if chosen.len() >= MAX_FILES {
                break 'outer;
            }
        }
        if !added_any {
            break;
        }
        round += 1;
    }

    (chosen, stats)
}

/// Parses `sources` and ranks them. Must run off the foreground thread.
pub async fn build(
    sources: Vec<SourceFile>,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
) -> SymbolMap {
    let (kept, mut stats) = select(sources);

    let mut parsed: Vec<ParsedFile> = Vec::with_capacity(kept.len());
    for source in kept {
        // Yielding between files lets the task be dropped promptly when the
        // panel closes or a newer build starts.
        futures::future::ready(()).await;
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
                unsupported(&mut stats);
                continue;
            }
        };
        // Without a grammar and an outline query there is nothing to collect;
        // such files do not count as considered so the stats stay meaningful.
        if !has_outline_query(&language) {
            unsupported(&mut stats);
            continue;
        }
        let Some(file) = parse_file(&source, &text, &language) else {
            unsupported(&mut stats);
            continue;
        };
        parsed.push(file);
    }

    stats.parsed = parsed.len();
    resolve_includes(&mut parsed);
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
                symbols: file.symbols,
            })
            .collect(),
        stats,
    }
}

struct ParsedFile {
    rel: Arc<RelPath>,
    size: u64,
    lines: u32,
    rank: f32,
    in_degree: u32,
    out_degree: u32,
    symbols: Vec<SymbolEntry>,
    /// Identifiers mentioned anywhere in the file, used to build the
    /// reference graph.
    mentions: HashSet<Arc<str>>,
    /// Include paths, module paths and import specifiers naming other files.
    includes: Vec<String>,
}

/// Records a file the builder cannot turn into symbols. Such files do not
/// occupy a `considered` slot, which counts only files that were inspected.
fn unsupported(stats: &mut BuildStats) {
    stats.unsupported += 1;
    stats.considered = stats.considered.saturating_sub(1);
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
    let mentions = collect_mentions(tree.root_node(), text);
    let includes = collect_includes(tree.root_node(), text);
    Some(ParsedFile {
        rel: source.rel.clone(),
        size: source.size,
        lines: text.lines().count() as u32,
        rank: 0.,
        in_degree: 0,
        out_degree: 0,
        symbols,
        mentions,
        includes,
    })
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
        return Some(match declared_type(item, bytes) {
            Some(return_type) => format!("{parameters} -> {return_type}").into(),
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

/// Collects identifier-shaped tokens. Every tree-sitter grammar in this
/// repository names identifiers with a `*_identifier` kind, which keeps this
/// language-agnostic without a per-language tags query.
fn collect_mentions(root: tree_sitter::Node<'_>, text: &str) -> HashSet<Arc<str>> {
    let bytes = text.as_bytes();
    let mut mentions: HashSet<Arc<str>> = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if mentions.len() >= MAX_IDENTIFIERS_PER_FILE {
            break;
        }
        let kind = node.kind();
        if kind.ends_with("identifier")
            && let Ok(name) = node.utf8_text(bytes)
            && !name.is_empty()
        {
            mentions.insert(name.into());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    mentions
}

/// Collects the file-level edges a source file declares: `#include` paths,
/// Rust `use`/`mod` paths and JavaScript/TypeScript import specifiers.
///
/// These are the edges that make "which file does the rest of the project pull
/// in" answerable without name resolution, unlike the identifier-reference
/// graph behind [`rank`].
fn collect_includes(root: tree_sitter::Node<'_>, text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut specs = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "preproc_include" => {
                if let Some(path) = node.child_by_field_name("path")
                    && let Ok(raw) = path.utf8_text(bytes)
                {
                    let spec = raw
                        .trim()
                        .trim_matches(|c| c == '"' || c == '<' || c == '>');
                    if !spec.is_empty() {
                        specs.push(spec.to_owned());
                    }
                }
            }
            "use_declaration" | "mod_item" => {
                if let Ok(raw) = node.utf8_text(bytes) {
                    if let Some(spec) = module_path(raw) {
                        specs.push(spec);
                    }
                }
            }
            "import_statement" | "import_declaration" | "export_statement" => {
                let source = node
                    .child_by_field_name("source")
                    .or_else(|| node.child_by_field_name("path"));
                if let Some(source) = source
                    && let Ok(raw) = source.utf8_text(bytes)
                {
                    let spec = raw.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !spec.is_empty() {
                        specs.push(spec.to_owned());
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
    specs
}

/// Turns `use crate::graph::{BFS, DFS};` or `mod graph;` into a path fragment.
fn module_path(source: &str) -> Option<String> {
    let source = source.trim().trim_end_matches(';').trim();
    let source = source.strip_prefix("pub ").unwrap_or(source);
    let path = source
        .strip_prefix("use ")
        .or_else(|| source.strip_prefix("mod "))?;
    let path = path
        .split(['{', '*', ' '])
        .next()?
        .trim_end_matches(':')
        .trim();
    if path.is_empty() || path == "self" || path == "super" || path == "crate" {
        return None;
    }
    Some(path.replace("::", "/"))
}

/// Resolves each file's include specs against the files the map actually has,
/// and records the in/out counts used by the ordering and the file headers.
fn resolve_includes(files: &mut [ParsedFile]) {
    let paths: Vec<String> = files
        .iter()
        .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
        .collect();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (index, path) in paths.iter().enumerate() {
        by_path.insert(path.clone(), index);
        if let Some(name) = path.rsplit('/').next() {
            // `#include "BFS.h"` names the file, not where it lives, so an
            // ambiguous name resolves to the shallowest path.
            by_name
                .entry(name.to_owned())
                .and_modify(|existing| {
                    if path.len() < paths[*existing].len() {
                        *existing = index;
                    }
                })
                .or_insert(index);
        }
    }

    let mut outgoing: Vec<HashSet<usize>> = vec![HashSet::new(); files.len()];
    for (index, file) in files.iter().enumerate() {
        let directory = paths[index]
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        for spec in &file.includes {
            if let Some(target) = resolve_spec(spec, directory, &by_path, &by_name)
                && target != index
            {
                outgoing[index].insert(target);
            }
        }
    }

    let mut incoming: Vec<u32> = vec![0; files.len()];
    for targets in &outgoing {
        for &target in targets {
            incoming[target] += 1;
        }
    }
    for (index, file) in files.iter_mut().enumerate() {
        file.out_degree = outgoing[index].len() as u32;
        file.in_degree = incoming[index];
    }
}

/// Extensions tried when an include omits one (`#include "BFS"`, `mod graph;`).
const RESOLVABLE_EXTENSIONS: &[&str] = &[
    "", ".h", ".hpp", ".hh", ".hxx", ".c", ".cc", ".cpp", ".cxx", ".rs", ".ts", ".tsx", ".js",
    ".jsx", ".mjs", ".py", ".go", ".java", ".cs", ".rb", ".kt",
];

fn resolve_spec(
    spec: &str,
    directory: &str,
    by_path: &HashMap<String, usize>,
    by_name: &HashMap<String, usize>,
) -> Option<usize> {
    let spec = spec.replace('\\', "/");
    let spec = spec.trim_start_matches("./").trim_end_matches('/');
    if spec.is_empty() {
        return None;
    }

    let mut bases = Vec::with_capacity(2);
    if !directory.is_empty() {
        bases.push(format!("{directory}/{spec}"));
    }
    bases.push(spec.to_owned());
    for base in &bases {
        for extension in RESOLVABLE_EXTENSIONS {
            if let Some(&index) = by_path.get(&format!("{base}{extension}")) {
                return Some(index);
            }
            if let Some(&index) = by_path.get(&format!("{base}/index{extension}")) {
                return Some(index);
            }
        }
    }

    // `#include "BFS.h"` names the file, not where it lives.
    let name = spec.rsplit('/').next().unwrap_or(spec);
    for extension in RESOLVABLE_EXTENSIONS {
        if let Some(&index) = by_name.get(&format!("{name}{extension}")) {
            return Some(index);
        }
    }
    None
}

/// Character budget for one `repo_map` page.
///
/// This is the single source of truth for the agent-facing map: the snapshot
/// records it as `max_chars` and the bundled MCP server reads it from there, so
/// the IDE's rendering and the server's cannot drift apart. Only the last file
/// that does not fit is trimmed; `limit` decides how many files a page holds.
pub const MAX_REPO_MAP_CHARS: usize = 48_000;

/// PageRank over the cross-file reference graph.
///
/// An edge `a -> b` means file `a` mentions an identifier that file `b`
/// defines; the edge weight is the number of such identifiers, so a file
/// referenced by many distinct identifiers counts more than one referenced
/// once.
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
    for (index, file) in files.iter().enumerate() {
        for symbol in &file.symbols {
            defined_in
                .entry(symbol.name.clone())
                .or_default()
                .push(index);
        }
    }

    // Outgoing weights per file, plus each file's total weight for
    // normalization.
    let mut edges: Vec<Vec<(usize, f32)>> = vec![Vec::new(); count];
    let mut totals: Vec<f32> = vec![0.; count];
    for (index, file) in files.iter().enumerate() {
        let mut weights: HashMap<usize, f32> = HashMap::new();
        for mention in &file.mentions {
            let Some(definers) = defined_in.get(mention) else {
                continue;
            };
            for &target in definers {
                if target != index {
                    *weights.entry(target).or_default() += 1.;
                }
            }
        }
        let total: f32 = weights.values().sum();
        if total > 0. {
            edges[index] = weights.into_iter().collect();
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

    for (index, file) in files.iter_mut().enumerate() {
        file.rank = ranks[index];
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
            });
        }
    }
    sources
}

/// Builds a symbol map for the whole project off the foreground thread. The
/// returned task is cancelled when dropped.
pub fn symbol_map_task(project: Entity<Project>, cx: &App) -> Task<SymbolMap> {
    let sources = project_sources(&project, cx);
    let fs = project.read(cx).fs().clone();
    let languages = project.read(cx).languages().clone();
    cx.background_spawn(async move { build(sources, fs, languages).await })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            symbols,
        }
    }

    fn file(rel: &'static str, defs: &[&'static str], mentions: &[&'static str]) -> ParsedFile {
        ParsedFile {
            rel: test_rel(rel),
            size: 1,
            lines: 1,
            rank: 0.,
            in_degree: 0,
            out_degree: 0,
            symbols: defs.iter().map(|name| entry(name, "function", 0)).collect(),
            mentions: mentions.iter().map(|name| Arc::from(*name)).collect(),
            includes: Vec::new(),
        }
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

    fn source(rel: &'static str, size: u64) -> SourceFile {
        SourceFile {
            rel: RelPath::new(std::path::Path::new(rel), path::PathStyle::local())
                .expect("valid test path")
                .into_arc(),
            abs_path: PathBuf::from(rel),
            size,
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
                    abs_path: path,
                    size: metadata.len(),
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
        let (kept, stats) = select(vec![
            source("a.rs", 10),
            source("huge.rs", MAX_FILE_BYTES + 1),
            source("b.rs", 10),
        ]);
        let names: Vec<String> = kept
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert_eq!(names, ["a.rs", "b.rs"]);
        assert_eq!(stats.considered, 3);
        assert_eq!(stats.too_large, 1);
    }

    #[test]
    fn select_covers_every_top_level_directory_before_going_deeper() {
        // `b` has one file and `a` has three: round-robin must still take
        // `b/x.rs` before `a`'s second helping, and `a` takes its largest
        // file first.
        let (kept, _) = select(vec![
            source("a/small.rs", 10),
            source("a/big.rs", 90),
            source("a/mid.rs", 50),
            source("b/x.rs", 20),
        ]);
        let names: Vec<String> = kept
            .iter()
            .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
            .collect();
        assert_eq!(names, ["a/big.rs", "b/x.rs", "a/mid.rs", "a/small.rs"]);
    }

    #[test]
    fn select_stops_at_the_total_byte_budget() {
        // Many files at the per-file cap reach the total budget before the
        // file count cap does.
        let names: Vec<&'static str> = vec!["f.rs"; 200];
        let sources = names
            .iter()
            .map(|name| source(name, MAX_FILE_BYTES))
            .collect::<Vec<_>>();
        let (kept, stats) = select(sources);

        let total: u64 = kept.iter().map(|file| file.size).sum();
        assert!(total <= MAX_TOTAL_BYTES, "total was {total}");
        assert!(
            kept.len() < 200,
            "the byte budget should have stopped the scan"
        );
        assert_eq!(stats.too_large, 0);
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
        assert!(full.contains("Order: include fan-in"));

        // The byte budget is a last resort: the first file is always shown even
        // when it does not fit, and the rest are listed by path.
        let tiny = map.to_markdown(10);
        assert!(tiny.contains("src/lib.rs"), "{tiny}");
        assert!(tiny.contains("Omitted files"), "{tiny}");
        assert!(tiny.contains("src/big.rs"), "{tiny}");
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
            text.contains("`src/lib.rs` (in 0 / out 0, 1 lines)"),
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
    fn include_edges_set_in_and_out_degrees() {
        let mut files = vec![
            file("graph/BFS.h", &["bfs"], &[]),
            file("graph/BFS.cpp", &["bfs"], &[]),
            file("main.cpp", &["main"], &[]),
        ];
        files[1].includes.push("BFS.h".to_owned());
        files[2].includes.push("graph/BFS.h".to_owned());
        files[0].includes.push("vector".to_owned());

        resolve_includes(&mut files);

        assert_eq!(files[0].in_degree, 2, "BFS.h is pulled in twice");
        assert_eq!(files[0].out_degree, 0);
        assert_eq!(files[1].out_degree, 1);
        assert_eq!(files[1].in_degree, 0);
        assert_eq!(files[2].in_degree, 0);
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
            "这个项目还没有可用的仓库地图（请先在 IDE 里打开一次 Repository Map 面板）"
        );
    }
}
