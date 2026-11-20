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
use project::Project;
use streaming_iterator::StreamingIterator as _;
use language::{Language, LanguageRegistry};
use path::rel_path::RelPath;

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
}

#[derive(Clone, Debug)]
pub struct FileSymbols {
    pub rel: Arc<RelPath>,
    pub size: u64,
    pub rank: f32,
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

impl SymbolMap {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Files that carry at least one definition, best-ranked first.
    pub fn ranked(&self) -> impl Iterator<Item = &FileSymbols> {
        self.files.iter().filter(|file| !file.symbols.is_empty())
    }

    /// Renders the map as markdown for a chat prompt.
    ///
    /// `max_chars` bounds the result so a huge repository cannot blow up the
    /// composer; files are added whole until the budget runs out.
    pub fn to_markdown(&self, max_chars: usize) -> String {
        let mut out = String::from(
            "## Repository map\n\nFiles ranked by how much the rest of the repository refers to them, with their top-level definitions.\n",
        );
        let mut budget = max_chars;
        let mut written = 0usize;
        for file in self.ranked() {
            // Always forward slashes: this text goes into an agent prompt and
            // should not change with the host platform.
            let mut block = format!("\n`{}`\n", file.rel.display(path::PathStyle::Unix));
            for symbol in &file.symbols {
                block.push_str(&format!("- {} {}\n", symbol.kind, symbol.name));
            }
            if block.len() > budget {
                break;
            }
            budget -= block.len();
            out.push_str(&block);
            written += 1;
        }
        if written == 0 {
            out.push_str("\nNo definitions were found in this repository.\n");
        }
        out
    }

    /// The map as JSON for the clipboard export: one entry per file that has
    /// definitions, best-ranked first, with one-based line numbers.
    pub fn json_entries(&self) -> Vec<serde_json::Value> {
        self.ranked()
            .map(|file| {
                serde_json::json!({
                    "path": file.rel.display(path::PathStyle::Unix),
                    "symbols": file
                        .symbols
                        .iter()
                        .map(|symbol| {
                            serde_json::json!({
                                "name": symbol.name,
                                "line": symbol.row + 1,
                                "kind": symbol.kind,
                            })
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect()
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
    rank(&mut parsed);
    parsed.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then_with(|| a.rel.as_std_path().cmp(b.rel.as_std_path()))
    });

    SymbolMap {
        files: parsed
            .into_iter()
            .map(|file| FileSymbols {
                rel: file.rel,
                size: file.size,
                rank: file.rank,
                symbols: file.symbols,
            })
            .collect(),
        stats,
    }
}

struct ParsedFile {
    rel: Arc<RelPath>,
    size: u64,
    rank: f32,
    symbols: Vec<SymbolEntry>,
    /// Identifiers mentioned anywhere in the file, used to build the
    /// reference graph.
    mentions: HashSet<Arc<str>>,
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
    Some(ParsedFile {
        rel: source.rel.clone(),
        size: source.size,
        rank: 0.,
        symbols,
        mentions,
    })
}

/// A definition name reported by the outline query, before the per-file cap.
struct Candidate {
    name: Arc<str>,
    kind: &'static str,
    row: u32,
    /// Depth of the matched node; shallow nodes win when the cap trims a file.
    depth: usize,
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
    // are one symbol: keyed by the definition body that contains them.
    let mut seen: HashSet<(usize, &str)> = HashSet::new();
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
        if !seen.insert((enclosing_body(item_node).start_byte(), name)) {
            continue;
        }
        candidates.push(Candidate {
            name: name.into(),
            kind: classify_kind(item_node),
            row: name_node.start_position().row as u32,
            depth: node_depth(item_node),
        });
    }
    finish_symbols(candidates)
}

/// Applies the per-file cap, keeping the shallowest definitions, then orders
/// the survivors by line as the rest of the map expects.
fn finish_symbols(mut candidates: Vec<Candidate>) -> Vec<SymbolEntry> {
    candidates.sort_by_key(|candidate| (candidate.depth, candidate.row));
    candidates.truncate(MAX_SYMBOLS_PER_FILE);
    candidates.sort_by_key(|candidate| candidate.row);
    candidates
        .into_iter()
        .map(|candidate| SymbolEntry {
            name: candidate.name,
            kind: candidate.kind.into(),
            row: candidate.row,
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
        | "function_declarator"
        | "function_definition"
        | "function_item"
        | "function_signature_item"
        | "method_declaration"
        | "method_definition" => "function",
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
        return true;
    }
    let mut cursor = declarator.walk();
    let nested: Vec<_> = declarator
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    nested.into_iter().any(declarator_is_function)
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

    fn file(rel: &'static str, defs: &[&'static str], mentions: &[&'static str]) -> ParsedFile {
        ParsedFile {
            rel: RelPath::new(std::path::Path::new(rel), path::PathStyle::local())
                .expect("valid test path")
                .into_arc(),
            size: 1,
            rank: 0.,
            symbols: defs
                .iter()
                .map(|name| SymbolEntry {
                    name: (*name).into(),
                    kind: "function".into(),
                    row: 0,
                })
                .collect(),
            mentions: mentions.iter().map(|name| Arc::from(*name)).collect(),
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
            rel: RelPath::new(std::path::Path::new("src/repo_map_panel.rs"), path::PathStyle::local())
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
        assert!(parsed.mentions.len() > 50, "mentions: {}", parsed.mentions.len());

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
        assert!(kept.len() < 200, "the byte budget should have stopped the scan");
        assert_eq!(stats.too_large, 0);
    }

    #[test]
    fn markdown_lists_ranked_files_and_respects_the_budget() {
        let map = SymbolMap {
            files: vec![
                FileSymbols {
                    rel: RelPath::new(std::path::Path::new("src/lib.rs"), path::PathStyle::local())
                        .unwrap()
                        .into_arc(),
                    size: 10,
                    rank: 0.5,
                    symbols: vec![SymbolEntry {
                        name: "main".into(),
                        kind: "function".into(),
                        row: 0,
                    }],
                },
                FileSymbols {
                    rel: RelPath::new(std::path::Path::new("src/big.rs"), path::PathStyle::local())
                        .unwrap()
                        .into_arc(),
                    size: 10,
                    rank: 0.1,
                    symbols: vec![SymbolEntry {
                        name: "other".into(),
                        kind: "function".into(),
                        row: 0,
                    }],
                },
            ],
            stats: BuildStats::default(),
        };

        let full = map.to_markdown(10_000);
        assert!(full.contains("src/lib.rs"));
        assert!(full.contains("function main"));
        assert!(full.find("src/lib.rs") < full.find("src/big.rs"));

        // A budget smaller than the first file block stops before anything is
        // written, and says so instead of emitting a bare header.
        let tiny = map.to_markdown(10);
        assert!(tiny.contains("No definitions were found"));
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
                depth: 4,
            })
            .chain([Candidate {
                name: "top_level".into(),
                kind: "function",
                row: (MAX_SYMBOLS_PER_FILE + 40) as u32,
                depth: 1,
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
    fn json_export_uses_one_based_lines_and_relative_paths() {
        let map = SymbolMap {
            files: vec![FileSymbols {
                rel: RelPath::new(std::path::Path::new("src/lib.rs"), path::PathStyle::local())
                    .expect("valid test path")
                    .into_arc(),
                size: 10,
                rank: 1.,
                symbols: vec![SymbolEntry {
                    name: "foo".into(),
                    kind: "function".into(),
                    row: 11,
                }],
            }],
            stats: BuildStats::default(),
        };

        let json = serde_json::to_string(&map.json_entries()).expect("values serialize");
        let actual: serde_json::Value = serde_json::from_str(&json).expect("json parses");
        assert_eq!(
            actual,
            serde_json::json!([{
                "path": "src/lib.rs",
                "symbols": [{"name": "foo", "line": 12, "kind": "function"}],
            }])
        );
    }
}
