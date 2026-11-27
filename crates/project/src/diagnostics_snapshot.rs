//! JSON snapshot of one worktree's LSP diagnostics.
//!
//! The diagnostics live in the LSP store, which only WuIDE can read. Pi runs
//! its MCP server inside the project, so WuIDE writes one file per project
//! root and the server derives the same name from its working directory, exactly
//! like the repository map snapshot.
//!
//! The writer belongs here rather than in `repo_wiki`: diagnostics are project
//! state, and `repo_wiki` already depends on `project`, so putting it there
//! would make the dependency point backwards. Both snapshots share
//! [`paths::project_key`] so their file names agree.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fs::{Fs, PathEventKind, RemoveOptions, Watcher};
use futures::StreamExt;
use language::{DiagnosticEntry, PointUtf16, Unclipped};
use lsp::{DiagnosticSeverity, NumberOrString};
use path::PathStyle;
use path::rel_path::RelPath;
use serde_json::{Value, json};
use util::ResultExt as _;

/// Bump when the layout changes. The MCP server refuses versions it does not
/// know and asks the user to refresh.
pub const SNAPSHOT_VERSION: u32 = 1;

/// More than this many diagnostics are cut, because no agent reads the tail.
pub const MAX_ITEMS: usize = 2000;

/// Longer messages are cut; the agent can open the file for the full text.
pub const MAX_MESSAGE_CHARS: usize = 500;

/// How long to wait for a burst of diagnostics updates to settle.
pub const DEBOUNCE: Duration = Duration::from_secs(1);

/// Pi's MCP server drops `<project-key>.refresh` beside the snapshot when an
/// agent asks for fresher diagnostics. Wu answers by rewriting the snapshots
/// and removing the marker, so a later tool call sees a new `generated_at`.
pub const REFRESH_REQUEST_SUFFIX: &str = ".refresh";
const REFRESH_WATCH_LATENCY: Duration = Duration::from_millis(100);
/// A marker left behind by a session with no open project must never trigger a
/// rewrite much later, so requests carry the server's clock in milliseconds.
const REFRESH_REQUEST_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Whether a path in the snapshot directory is an agent's refresh request.
pub fn is_refresh_request(path: &Path) -> bool {
    path.file_name()
        .and_then(|file_name| file_name.to_str())
        .is_some_and(|file_name| file_name.ends_with(REFRESH_REQUEST_SUFFIX))
}

/// Whether a marker is recent enough to answer. An unreadable timestamp is
/// treated as abandoned: the server always writes one.
pub fn is_current_refresh_request(contents: &[u8], now: SystemTime) -> bool {
    let Ok(text) = std::str::from_utf8(contents) else {
        return false;
    };
    let Ok(millis) = text.trim().parse::<u64>() else {
        return false;
    };
    let Some(written) = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis)) else {
        return false;
    };
    now.duration_since(written).unwrap_or_default() <= REFRESH_REQUEST_MAX_AGE
}

/// Watches `dir` for refresh requests. Markers already on disk when the watch
/// starts are swept: expired ones are removed, still-current ones are returned
/// as the first batch. The watcher must stay alive for later events to arrive.
pub async fn watch_refresh_requests(
    fs: Arc<dyn Fs>,
    dir: PathBuf,
) -> (
    Pin<Box<dyn futures::Stream<Item = Vec<PathBuf>> + Send>>,
    Arc<dyn Watcher>,
) {
    fs.create_dir(&dir).await.log_err();
    let (events, watcher) = fs.watch(&dir, REFRESH_WATCH_LATENCY).await;

    let mut pending = Vec::new();
    if let Ok(mut entries) = fs.read_dir(&dir).await {
        while let Some(entry) = entries.next().await {
            let Ok(path) = entry else {
                continue;
            };
            if !is_refresh_request(&path) {
                continue;
            }
            let current = fs
                .load(&path)
                .await
                .is_ok_and(|contents| is_current_refresh_request(contents.as_bytes(), SystemTime::now()));
            if current {
                pending.push(path);
            } else {
                fs.remove_file(
                    &path,
                    RemoveOptions {
                        ignore_if_not_exists: true,
                        ..Default::default()
                    },
                )
                .await
                .log_err();
            }
        }
    }

    let requests = futures::stream::iter((!pending.is_empty()).then_some(pending)).chain(
        events.filter_map(|batch| {
            let requests = batch
                .into_iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        Some(PathEventKind::Created | PathEventKind::Changed)
                    )
                })
                .map(|event| event.path)
                .filter(|path| is_refresh_request(path))
                .collect::<Vec<_>>();
            async move { (!requests.is_empty()).then_some(requests) }
        }),
    );
    (Box::pin(requests), watcher)
}

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
