//! JSON snapshot of one worktree's LSP diagnostics.
//!
//! The diagnostics live in the LSP store, which only the IDE can read. Pi runs
//! its MCP server inside the project, so the IDE writes one file per project
//! root and the server derives the same name from its working directory, exactly
//! like the repository map snapshot.
//!
//! The writer belongs here rather than in `repo_wiki`: diagnostics are project
//! state, and `repo_wiki` already depends on `project`, so putting it there
//! would make the dependency point backwards. Both snapshots share
//! [`paths::project_key`] so their file names agree.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use language::{DiagnosticEntry, PointUtf16, Unclipped};
use lsp::{DiagnosticSeverity, NumberOrString};
use path::PathStyle;
use path::rel_path::RelPath;
use serde_json::{Value, json};

/// Bump when the layout changes. The MCP server refuses versions it does not
/// know and asks the user to refresh.
pub const SNAPSHOT_VERSION: u32 = 1;

/// More than this many diagnostics are cut, because no agent reads the tail.
pub const MAX_ITEMS: usize = 2000;

/// Longer messages are cut; the agent can open the file for the full text.
pub const MAX_MESSAGE_CHARS: usize = 500;

/// How long to wait for a burst of diagnostics updates to settle.
pub const DEBOUNCE: Duration = Duration::from_secs(1);

/// One diagnostic as the snapshot records it: one-based, project-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub severity: &'static str,
    pub code: String,
    pub message: String,
    pub source: String,
    message_truncated: bool,
}

impl Item {
    /// Converts one stored entry, or `None` for the secondary diagnostics that
    /// only elaborate on a primary one.
    pub fn new(path: &RelPath, entry: &DiagnosticEntry<Unclipped<PointUtf16>>) -> Option<Self> {
        let diagnostic = &entry.diagnostic;
        if !diagnostic.is_primary {
            return None;
        }
        let severity = match diagnostic.severity {
            DiagnosticSeverity::ERROR => "error",
            DiagnosticSeverity::WARNING => "warning",
            _ => "info",
        };
        let code = match &diagnostic.code {
            Some(NumberOrString::Number(number)) => number.to_string(),
            Some(NumberOrString::String(code)) => code.clone(),
            None => String::new(),
        };
        let mut message: String = diagnostic.message.as_str().to_owned();
        let message_truncated = message.chars().count() > MAX_MESSAGE_CHARS;
        if message_truncated {
            message = message.chars().take(MAX_MESSAGE_CHARS).collect();
        }
        Some(Self {
            path: path.display(PathStyle::Unix).into_owned(),
            // LSP rows and columns are zero-based; the agent addresses them from 1.
            line: entry.range.start.0.row + 1,
            column: entry.range.start.0.column + 1,
            severity,
            code,
            message,
            source: diagnostic.source.clone().unwrap_or_default(),
            message_truncated,
        })
    }
}

/// The snapshot document. `generated_at` is passed in so tests can pin it.
pub fn to_json(project_root: &Path, generated_at: &str, items: &[Item]) -> Value {
    let shown = &items[..items.len().min(MAX_ITEMS)];
    let truncated = items.len() > MAX_ITEMS || shown.iter().any(|item| item.message_truncated);

    let mut doc = json!({
        "version": SNAPSHOT_VERSION,
        "project": project_root.to_string_lossy(),
        "generated_at": generated_at,
        "items": shown
            .iter()
            .map(|item| json!({
                "path": item.path,
                "line": item.line,
                "column": item.column,
                "severity": item.severity,
                "code": item.code,
                "message": item.message,
                "source": item.source,
            }))
            .collect::<Vec<_>>(),
    });
    if truncated {
        doc["truncated"] = json!(true);
    }
    doc
}

/// Writes one worktree's diagnostics into `dir`.
///
/// Errors belong to the caller to log: a missing snapshot only costs the agent
/// its diagnostic list.
pub fn write(project_root: &Path, dir: &Path, items: &[Item]) -> io::Result<PathBuf> {
    let path = dir.join(format!("{}.json", paths::project_key(project_root)));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = serde_json::to_vec(&to_json(project_root, &generated_at, items))
        .map_err(io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Sorts diagnostics into the order the server renders them, so a snapshot
/// only changes when the diagnostics themselves do.
pub fn sort_items(items: &mut [Item]) {
    items.sort_by(|left, right| {
        (
            &left.path,
            left.line,
            left.column,
            left.severity,
            &left.code,
        )
            .cmp(&(
                &right.path,
                right.line,
                right.column,
                right.severity,
                &right.code,
            ))
    });
}
