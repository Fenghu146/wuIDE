//! Session snapshots: a copy of the worktree's dirty files taken around an agent
//! session, so the files can be put back when the agent leaves them broken.
//!
//! A snapshot only ever contains files git reports as changed, including
//! untracked files. Every file is stored whole and compressed on its own, with
//! its sha256, size, and mtime, and files over [`MAX_FILE_BYTES`] or with binary
//! content are recorded as skipped with the reason instead of being stored.
//!
//! Capture and restore always run on the background executor, and both read and
//! write one file at a time so a snapshot never sits in memory as a whole.

mod capture;
mod settings;
mod store;

#[cfg(test)]
mod tests;

use capture::capture_snapshot;
pub use capture::{
    CaptureReport, CaptureRequest, MAX_FILE_BYTES, RestoreReport, find_worktree_root,
    read_file_text, restore_file, restore_snapshot,
};
use fs::Fs;
use gpui::{Action, App, AppContext as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
pub use settings::SessionSnapshotSettings;
use ::settings::Settings as _;
pub use store::{
    SKIP_BINARY, SKIP_DELETED, SKIP_NOT_A_FILE, SKIP_TOO_LARGE, SessionSnapshotDb,
    SnapshotFileSummary, SnapshotKind, SnapshotSummary,
};

/// Lists the files agents changed in this worktree and lets you put them back.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = session_snapshots)]
#[serde(deny_unknown_fields)]
pub struct OpenSessionSnapshots {
    /// The worktree to list snapshots for. Defaults to the first worktree of the project.
    #[serde(default)]
    pub root: Option<String>,
}

/// Returns the worktree roots of a project, so a snapshot can be attributed to
/// the same key the snapshot list uses.
pub fn worktree_root_for(path: &std::path::Path) -> PathBuf {
    find_worktree_root(path).unwrap_or_else(|| path.to_path_buf())
}

/// Captures a session boundary in the background.
///
/// The snapshot runs on the background executor and is detached, so it never
/// blocks the caller and cannot be cancelled by dropping a handle.
pub fn capture_session_boundary(cx: &mut App, request: CaptureRequest) {
    let Some(settings) = SessionSnapshotSettings::try_get(cx).cloned() else {
        return;
    };
    if !settings.enabled {
        return;
    }
    let Some(fs) = <dyn Fs>::try_global(cx) else {
        return;
    };
    let db = SessionSnapshotDb::global(cx);
    cx.background_spawn(async move {
        match capture_snapshot(&db, fs, &settings, request).await {
            Ok(report) if report.stored > 0 || report.skipped > 0 => log::debug!(
                "session snapshot stored {} files ({} skipped) in {} bytes",
                report.stored,
                report.skipped,
                report.stored_bytes
            ),
            Ok(_) => {}
            Err(error) => log::warn!("could not take a session snapshot: {error:#}"),
        }
    })
    .detach();
}
