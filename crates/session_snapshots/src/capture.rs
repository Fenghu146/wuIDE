//! Taking snapshots of a worktree's dirty files, and putting them back.

use crate::{
    SessionSnapshotDb,
    settings::SessionSnapshotSettings,
    store::{
        NewSnapshot, NewSnapshotFile, SKIP_BINARY, SKIP_DELETED, SKIP_NOT_A_FILE, SKIP_TOO_LARGE,
        SnapshotFileSummary, SnapshotKind, SnapshotSummary,
    },
};
use anyhow::{Context as _, Result};
use fs::Fs;
use git::status::FileStatus;
use sha2::{Digest as _, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use time::OffsetDateTime;

/// Files larger than this are skipped, matching what a person would call "a
/// file an agent can rewrite without the editor stalling".
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// Bytes inspected when deciding whether a file is text, as git does.
const BINARY_SNIFF_BYTES: usize = 8000;

/// A snapshot to take at a session boundary.
pub struct CaptureRequest {
    /// The directory the agent works in. Its worktree is what gets captured.
    pub root: PathBuf,
    pub session_id: String,
    pub session_title: String,
    pub kind: SnapshotKind,
}

pub struct CaptureReport {
    pub stored: usize,
    pub skipped: usize,
    pub stored_bytes: u64,
}

pub struct RestoreReport {
    pub restored: usize,
    pub skipped: usize,
    pub failed: Vec<(String, String)>,
}

impl RestoreReport {
    fn new() -> Self {
        Self {
            restored: 0,
            skipped: 0,
            failed: Vec::new(),
        }
    }
}

/// Captures the dirty files of `request.root`'s worktree into a new snapshot.
///
/// Files are read, compressed, and written one at a time, so the peak memory is
/// the largest dirty file rather than the whole snapshot.
pub async fn capture_snapshot(
    db: &SessionSnapshotDb,
    fs: Arc<dyn Fs>,
    settings: &SessionSnapshotSettings,
    request: CaptureRequest,
) -> Result<CaptureReport> {
    let worktree_root = find_worktree_root(&request.root).with_context(|| {
        format!(
            "{} is not in a git worktree",
            request.root.to_string_lossy()
        )
    })?;
    let dot_git = worktree_root.join(".git");
    let system_git_binary = which::which("git").ok();
    let repository = fs
        .open_repo(&dot_git, system_git_binary.as_deref())
        .with_context(|| format!("opening the git repository at {}", dot_git.display()))?;
    let status = repository.status(&[]).await?;

    let mut dirty = status
        .entries
        .iter()
        .filter(|(_, status)| status.has_changes())
        .map(|(path, status)| (path.clone(), *status))
        .collect::<Vec<_>>();
    dirty.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if dirty.is_empty() {
        return Ok(CaptureReport {
            stored: 0,
            skipped: 0,
            stored_bytes: 0,
        });
    }

    let created_at = OffsetDateTime::now_utc().unix_timestamp();
    let root = worktree_root.to_string_lossy().into_owned();
    let snapshot_id = db
        .insert_snapshot(NewSnapshot {
            session_id: &request.session_id,
            session_title: &request.session_title,
            kind: request.kind,
            root: &root,
            created_at,
        })
        .await?;

    let mut report = CaptureReport {
        stored: 0,
        skipped: 0,
        stored_bytes: 0,
    };
    for (repo_path, git_status) in dirty {
        let path = repo_path.as_unix_str().to_string();
        let absolute_path = worktree_root.join(repo_path.as_std_path());
        let file =
            capture_file(&*fs, &absolute_path, git_status).await.with_context(|| {
                format!("reading {} while taking a snapshot", absolute_path.display())
            })?;
        if file.is_stored() {
            report.stored += 1;
            report.stored_bytes += file.compressed_size as u64;
        } else {
            report.skipped += 1;
        }
        db.insert_file(
            snapshot_id,
            NewSnapshotFile {
                path,
                git_status: git_status_label(git_status).to_string(),
                size: file.size,
                mtime: file.mtime,
                sha256: file.sha256,
                compressed_size: file.compressed_size,
                body: file.body,
                skip_reason: file.skip_reason,
            },
        )
        .await?;
    }

    db.finish_snapshot(
        snapshot_id,
        report.stored as i64,
        report.skipped as i64,
        report.stored_bytes as i64,
    )
    .await?;

    if let Err(error) = db
        .trim(
            settings.max_total_bytes(),
            settings.retention_seconds(),
            created_at,
        )
        .await
    {
        log::warn!("could not trim session snapshots: {error:#}");
    }

    Ok(report)
}

/// Returns the worktree that contains `start`, if there is one.
pub fn find_worktree_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|ancestor| matches!(ancestor.join(".git").try_exists(), Ok(true)))
        .map(|ancestor| ancestor.to_path_buf())
}

struct CapturedFile {
    size: i64,
    mtime: i64,
    sha256: String,
    compressed_size: i64,
    body: Option<Vec<u8>>,
    skip_reason: Option<String>,
}

impl CapturedFile {
    fn stored(size: u64, mtime: i64, sha256: String, body: Vec<u8>) -> Self {
        Self {
            size: size as i64,
            mtime,
            sha256,
            compressed_size: body.len() as i64,
            body: Some(body),
            skip_reason: None,
        }
    }

    fn skipped(size: u64, mtime: i64, reason: &str) -> Self {
        Self {
            size: size as i64,
            mtime,
            sha256: String::new(),
            compressed_size: 0,
            body: None,
            skip_reason: Some(reason.to_string()),
        }
    }

    fn is_stored(&self) -> bool {
        self.skip_reason.is_none()
    }
}

async fn capture_file(fs: &dyn Fs, path: &Path, git_status: FileStatus) -> Result<CapturedFile> {
    if git_status.is_deleted() {
        return Ok(CapturedFile::skipped(0, 0, SKIP_DELETED));
    }
    let Some(metadata) = fs.metadata(path).await? else {
        return Ok(CapturedFile::skipped(0, 0, SKIP_DELETED));
    };
    let mtime = metadata
        .mtime
        .to_seconds_and_nanos_for_persistence()
        .map_or(0, |(seconds, _)| seconds as i64);
    if metadata.is_dir || metadata.is_symlink || metadata.is_fifo {
        return Ok(CapturedFile::skipped(metadata.len, mtime, SKIP_NOT_A_FILE));
    }
    if metadata.len > MAX_FILE_BYTES {
        return Ok(CapturedFile::skipped(metadata.len, mtime, SKIP_TOO_LARGE));
    }

    let content = fs.load_bytes(path).await?;
    if content.len() as u64 > MAX_FILE_BYTES {
        return Ok(CapturedFile::skipped(content.len() as u64, mtime, SKIP_TOO_LARGE));
    }
    if is_binary(&content) {
        return Ok(CapturedFile::skipped(content.len() as u64, mtime, SKIP_BINARY));
    }

    let sha256 = hex(&Sha256::digest(&content));
    let body = zstd::stream::encode_all(content.as_slice(), ZSTD_COMPRESSION_LEVEL)
        .context("compressing a snapshot file")?;
    Ok(CapturedFile::stored(
        content.len() as u64,
        mtime,
        sha256,
        body,
    ))
}

fn is_binary(content: &[u8]) -> bool {
    let sniffed = &content[..content.len().min(BINARY_SNIFF_BYTES)];
    sniffed.contains(&0)
}

fn hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn git_status_label(status: FileStatus) -> &'static str {
    if status.is_untracked() {
        "untracked"
    } else if status.is_conflicted() {
        "conflict"
    } else if status.is_created() {
        "added"
    } else if status.is_deleted() {
        "deleted"
    } else if status.is_modified() {
        "modified"
    } else {
        "changed"
    }
}

/// The uncompressed text of one snapshot file, for showing a diff.
pub fn read_file_text(
    db: &SessionSnapshotDb,
    snapshot_id: i64,
    path: &str,
) -> Result<Option<String>> {
    let Some(body) = db.file_body(snapshot_id, path.to_string())? else {
        return Ok(None);
    };
    let content = zstd::stream::decode_all(body.as_slice()).context("decompressing a snapshot")?;
    Ok(Some(String::from_utf8_lossy(&content).into_owned()))
}

/// Restores every stored file of a snapshot into the worktree it came from.
pub async fn restore_snapshot(
    db: &SessionSnapshotDb,
    fs: Arc<dyn Fs>,
    snapshot: &SnapshotSummary,
) -> Result<RestoreReport> {
    let files = db.list_files(snapshot.id)?;
    let root = PathBuf::from(&snapshot.root);
    let mut report = RestoreReport::new();
    for file in &files {
        match restore_file(db, fs.clone(), &root, snapshot.id, file).await {
            Ok(true) => report.restored += 1,
            Ok(false) => report.skipped += 1,
            Err(error) => report
                .failed
                .push((file.path.clone(), format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// Restores a single file. Returns `false` when the snapshot has no body for it,
/// which is how skipped files are reported.
pub async fn restore_file(
    db: &SessionSnapshotDb,
    fs: Arc<dyn Fs>,
    root: &Path,
    snapshot_id: i64,
    file: &SnapshotFileSummary,
) -> Result<bool> {
    if !file.is_stored() {
        return Ok(false);
    }
    let Some(body) = db.file_body(snapshot_id, file.path.clone())? else {
        return Ok(false);
    };
    let content =
        zstd::stream::decode_all(body.as_slice()).context("decompressing a snapshot file")?;
    drop(body);

    let absolute_path = root.join(Path::new(&file.path.replace('/', std::path::MAIN_SEPARATOR_STR)));
    if let Some(parent) = absolute_path.parent() {
        fs.create_dir(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    fs.write(&absolute_path, &content)
        .await
        .with_context(|| format!("writing {}", absolute_path.display()))?;
    Ok(true)
}
