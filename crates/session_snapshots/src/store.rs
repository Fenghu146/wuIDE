//! Storage for session snapshots: one row per snapshot, one row per dirty file.
//!
//! Bodies live in their own column and are only read for a single file at a
//! time, so listing snapshots or their files never loads compressed content.

use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};

/// A snapshot taken before the agent starts working on a session.
pub const KIND_BEFORE_SESSION: &str = "before_session";
/// A snapshot taken after the agent stops working on a session.
pub const KIND_AFTER_SESSION: &str = "after_session";

/// The file did not fit in the per-file budget.
pub const SKIP_TOO_LARGE: &str = "too_large";
/// The file is not text.
pub const SKIP_BINARY: &str = "binary";
/// The file is gone from the working tree, so there is nothing to copy.
pub const SKIP_DELETED: &str = "deleted";
/// The path is not a regular file, such as a directory or a link.
pub const SKIP_NOT_A_FILE: &str = "not_a_file";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotKind {
    BeforeSession,
    AfterSession,
}

impl SnapshotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforeSession => KIND_BEFORE_SESSION,
            Self::AfterSession => KIND_AFTER_SESSION,
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            KIND_AFTER_SESSION => Self::AfterSession,
            _ => Self::BeforeSession,
        }
    }
}

impl Column for SnapshotKind {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (value, next_index): (String, i32) = Column::column(statement, start_index)?;
        Ok((Self::from_str(&value), next_index))
    }
}

/// A snapshot's metadata, without any file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotSummary {
    pub id: i64,
    pub session_id: String,
    pub session_title: String,
    pub kind: SnapshotKind,
    pub root: String,
    pub created_at: i64,
    pub file_count: i64,
    pub skipped_count: i64,
    pub total_bytes: i64,
}

impl Column for SnapshotSummary {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next_index): (i64, i32) = Column::column(statement, start_index)?;
        let (session_id, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (session_title, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (kind, next_index): (SnapshotKind, i32) = Column::column(statement, next_index)?;
        let (root, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (created_at, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (file_count, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (skipped_count, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (total_bytes, next_index): (i64, i32) = Column::column(statement, next_index)?;
        Ok((
            Self {
                id,
                session_id,
                session_title,
                kind,
                root,
                created_at,
                file_count,
                skipped_count,
                total_bytes,
            },
            next_index,
        ))
    }
}

/// One dirty file in a snapshot, without its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFileSummary {
    /// Repository-relative path, always with `/` separators.
    pub path: String,
    /// How git saw the file when the snapshot was taken.
    pub git_status: String,
    /// Size on disk when the snapshot was taken.
    pub size: i64,
    /// Modification time on disk when the snapshot was taken, in unix seconds.
    pub mtime: i64,
    pub sha256: String,
    /// Size of the stored body, which is 0 for skipped files.
    pub compressed_size: i64,
    /// Why the file has no body, or `None` when it was stored.
    pub skip_reason: Option<String>,
}

impl SnapshotFileSummary {
    pub fn is_stored(&self) -> bool {
        self.skip_reason.is_none()
    }
}

impl Column for SnapshotFileSummary {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (path, next_index): (String, i32) = Column::column(statement, start_index)?;
        let (git_status, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (size, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (mtime, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (sha256, next_index): (String, i32) = Column::column(statement, next_index)?;
        let (compressed_size, next_index): (i64, i32) = Column::column(statement, next_index)?;
        let (skip_reason, next_index): (Option<String>, i32) =
            Column::column(statement, next_index)?;
        Ok((
            Self {
                path,
                git_status,
                size,
                mtime,
                sha256,
                compressed_size,
                skip_reason,
            },
            next_index,
        ))
    }
}

/// The fields written when a snapshot starts.
pub struct NewSnapshot<'a> {
    pub session_id: &'a str,
    pub session_title: &'a str,
    pub kind: SnapshotKind,
    pub root: &'a str,
    pub created_at: i64,
}

/// A file row to insert into a snapshot.
pub struct NewSnapshotFile {
    pub path: String,
    pub git_status: String,
    pub size: i64,
    pub mtime: i64,
    pub sha256: String,
    pub compressed_size: i64,
    pub body: Option<Vec<u8>>,
    pub skip_reason: Option<String>,
}

pub struct SessionSnapshotDb(ThreadSafeConnection);

impl Domain for SessionSnapshotDb {
    const NAME: &str = "SessionSnapshotDb";

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS session_snapshots(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            session_title TEXT NOT NULL,
            kind TEXT NOT NULL,
            root TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            complete INTEGER NOT NULL DEFAULT 0,
            file_count INTEGER NOT NULL DEFAULT 0,
            skipped_count INTEGER NOT NULL DEFAULT 0,
            total_bytes INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE INDEX IF NOT EXISTS session_snapshots_by_root
            ON session_snapshots(root, id);

        CREATE TABLE IF NOT EXISTS session_snapshot_files(
            snapshot_id INTEGER NOT NULL REFERENCES session_snapshots(id) ON DELETE CASCADE,
            path TEXT NOT NULL,
            git_status TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            compressed_size INTEGER NOT NULL,
            body BLOB,
            skip_reason TEXT,
            PRIMARY KEY (snapshot_id, path)
        ) STRICT;
    )];
}

db::static_connection!(SessionSnapshotDb, []);

impl SessionSnapshotDb {
    /// Inserts the snapshot row and returns its id. The row stays incomplete
    /// until [`Self::finish_snapshot`] runs, so a capture that dies halfway does
    /// not show up as a usable snapshot.
    pub async fn insert_snapshot(&self, snapshot: NewSnapshot<'_>) -> Result<i64> {
        let session_id = snapshot.session_id.to_string();
        let session_title = snapshot.session_title.to_string();
        let kind = snapshot.kind.as_str().to_string();
        let root = snapshot.root.to_string();
        let created_at = snapshot.created_at;
        self.insert_snapshot_internal(session_id, session_title, kind, root, created_at)
            .await?
            .context("inserting a snapshot did not return an id")
    }

    query! {
        async fn insert_snapshot_internal(
            session_id: String,
            session_title: String,
            kind: String,
            root: String,
            created_at: i64
        ) -> Result<Option<i64>> {
            INSERT INTO session_snapshots (session_id, session_title, kind, root, created_at)
            VALUES ((?), (?), (?), (?), (?)) RETURNING id
        }
    }

    pub async fn insert_file(&self, snapshot_id: i64, file: NewSnapshotFile) -> Result<()> {
        self.insert_file_internal(
            snapshot_id,
            file.path,
            file.git_status,
            file.size,
            file.mtime,
            file.sha256,
            file.compressed_size,
            file.body,
            file.skip_reason,
        )
        .await
    }

    query! {
        async fn insert_file_internal(
            snapshot_id: i64,
            path: String,
            git_status: String,
            size: i64,
            mtime: i64,
            sha256: String,
            compressed_size: i64,
            body: Option<Vec<u8>>,
            skip_reason: Option<String>
        ) -> Result<()> {
            INSERT INTO session_snapshot_files (
                snapshot_id, path, git_status, size, mtime, sha256, compressed_size, body,
                skip_reason
            ) VALUES ((?), (?), (?), (?), (?), (?), (?), (?), (?))
        }
    }

    pub async fn finish_snapshot(
        &self,
        id: i64,
        file_count: i64,
        skipped_count: i64,
        total_bytes: i64,
    ) -> Result<()> {
        self.finish_snapshot_internal(file_count, skipped_count, total_bytes, id)
            .await
    }

    query! {
        async fn finish_snapshot_internal(
            file_count: i64,
            skipped_count: i64,
            total_bytes: i64,
            id: i64
        ) -> Result<()> {
            UPDATE session_snapshots
            SET complete = 1, file_count = (?), skipped_count = (?), total_bytes = (?)
            WHERE id = (?)
        }
    }

    // Lists the newest complete snapshots of a worktree.
    query! {
        pub fn list_snapshots(
            root: String,
            limit: i64
        ) -> Result<Vec<SnapshotSummary>> {
            SELECT id, session_id, session_title, kind, root, created_at, file_count,
                skipped_count, total_bytes
            FROM session_snapshots
            WHERE root = (?) AND complete = (1)
            ORDER BY id DESC
            LIMIT (?)
        }
    }

    // Lists the next page of a worktree's snapshots, older than `before`.
    query! {
        pub fn list_snapshots_before(
            root: String,
            before: i64,
            limit: i64
        ) -> Result<Vec<SnapshotSummary>> {
            SELECT id, session_id, session_title, kind, root, created_at, file_count,
                skipped_count, total_bytes
            FROM session_snapshots
            WHERE root = (?) AND complete = (1) AND id < (?)
            ORDER BY id DESC
            LIMIT (?)
        }
    }

    // Lists a snapshot's files without reading any bodies.
    query! {
        pub fn list_files(snapshot_id: i64) -> Result<Vec<SnapshotFileSummary>> {
            SELECT path, git_status, size, mtime, sha256, compressed_size, skip_reason
            FROM session_snapshot_files
            WHERE snapshot_id = (?)
            ORDER BY path
        }
    }

    query! {
        pub fn snapshot_summary(snapshot_id: i64) -> Result<Option<SnapshotSummary>> {
            SELECT id, session_id, session_title, kind, root, created_at, file_count,
                skipped_count, total_bytes
            FROM session_snapshots
            WHERE id = (?)
        }
    }

    // Reads the zstd-compressed body of a single file.
    query! {
        pub fn file_body(snapshot_id: i64, path: String) -> Result<Option<Vec<u8>>> {
            SELECT body
            FROM session_snapshot_files
            WHERE snapshot_id = (?) AND path = (?)
        }
    }

    // Returns the id, creation time, and stored size of every complete snapshot,
    // oldest first. Sizes are enough to plan a trim without touching bodies.
    query! {
        fn snapshot_sizes() -> Result<Vec<(i64, i64, i64)>> {
            SELECT id, created_at, total_bytes
            FROM session_snapshots
            WHERE complete = (1)
            ORDER BY id ASC
        }
    }

    query! {
        async fn delete_snapshot_internal(id: i64) -> Result<()> {
            DELETE FROM session_snapshots WHERE id = (?)
        }
    }

    /// Deletes snapshots until the stored size fits in `max_total_bytes` and
    /// nothing is older than `retention_seconds`, oldest first. Snapshots left
    /// incomplete by an interrupted capture are deleted after an hour.
    pub async fn trim(&self, max_total_bytes: u64, retention_seconds: i64, now: i64) -> Result<()> {
        let snapshots = self.snapshot_sizes()?;
        let mut remaining: u64 = snapshots
            .iter()
            .map(|(_, _, total_bytes)| u64::try_from(*total_bytes).unwrap_or(0))
            .sum();
        let mut expired = Vec::new();
        for (id, created_at, total_bytes) in snapshots {
            let too_old = now.saturating_sub(created_at) > retention_seconds;
            if too_old || remaining > max_total_bytes {
                remaining = remaining.saturating_sub(u64::try_from(total_bytes).unwrap_or(0));
                expired.push(id);
            }
        }
        for id in expired {
            log::debug!("deleting session snapshot {id}");
            self.delete_snapshot_internal(id).await?;
        }
        self.delete_incomplete_snapshots(now - 60 * 60).await
    }

    query! {
        async fn delete_incomplete_snapshots(cutoff: i64) -> Result<()> {
            DELETE FROM session_snapshots WHERE complete = (0) AND created_at < (?)
        }
    }
}
