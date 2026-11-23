use crate::{
    CaptureRequest, SKIP_BINARY, SKIP_DELETED, SKIP_TOO_LARGE, SessionSnapshotDb,
    SessionSnapshotSettings, SnapshotKind,
    capture::{
        capture_snapshot, find_worktree_root, read_file_text, restore_file, restore_snapshot,
    },
    store::{NewSnapshot, NewSnapshotFile},
};
use fs::{Fs, RealFs};
use gpui::TestAppContext;
use std::{
    path::Path,
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

fn settings() -> SessionSnapshotSettings {
    SessionSnapshotSettings {
        enabled: true,
        max_total_mb: 64,
        retention_days: 7,
    }
}

fn run_git(directory: &Path, args: &[&str]) {
    #[allow(clippy::disallowed_methods)]
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(directory: &Path) {
    run_git(directory, &["init", "-b", "main"]);
    run_git(
        directory,
        &["config", "user.email", "snapshots@example.com"],
    );
    run_git(directory, &["config", "user.name", "Snapshots"]);
    run_git(directory, &["config", "commit.gpgsign", "false"]);
    std::fs::write(directory.join("clean.txt"), "clean\n").unwrap();
    std::fs::write(directory.join("changed.txt"), "before\n").unwrap();
    run_git(directory, &["add", "."]);
    run_git(directory, &["commit", "-m", "base"]);
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn capture(
    db: &SessionSnapshotDb,
    fs: Arc<dyn Fs>,
    root: &Path,
    kind: SnapshotKind,
) -> anyhow::Result<i64> {
    capture_snapshot(
        db,
        fs,
        &settings(),
        CaptureRequest {
            root: root.to_path_buf(),
            session_id: "session-1".to_string(),
            session_title: "Fix the login bug".to_string(),
            kind,
        },
    )
    .await?;
    let snapshots = db.list_snapshots(root.to_string_lossy().into_owned(), 10)?;
    Ok(snapshots.first().map_or(0, |snapshot| snapshot.id))
}

#[gpui::test]
async fn test_store_round_trip_and_cascade(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SessionSnapshotDb::open_test_db("snapshots_round_trip").await;

    let snapshot_id = db
        .insert_snapshot(NewSnapshot {
            session_id: "session-1",
            session_title: "Fix the login bug",
            kind: SnapshotKind::BeforeSession,
            root: "/project",
            created_at: 1_000,
        })
        .await
        .unwrap();
    db.insert_file(
        snapshot_id,
        NewSnapshotFile {
            path: "src/main.rs".to_string(),
            git_status: "modified".to_string(),
            size: 12,
            mtime: 900,
            sha256: "abc".to_string(),
            compressed_size: 7,
            body: Some(vec![1, 2, 3]),
            skip_reason: None,
        },
    )
    .await
    .unwrap();
    db.insert_file(
        snapshot_id,
        NewSnapshotFile {
            path: "assets/logo.png".to_string(),
            git_status: "untracked".to_string(),
            size: 4_000_000,
            mtime: 901,
            sha256: String::new(),
            compressed_size: 0,
            body: None,
            skip_reason: Some(SKIP_TOO_LARGE.to_string()),
        },
    )
    .await
    .unwrap();
    // Incomplete snapshots stay out of the list.
    assert!(
        db.list_snapshots("/project".to_string(), 10)
            .unwrap()
            .is_empty()
    );

    db.finish_snapshot(snapshot_id, 1, 1, 7).await.unwrap();

    let snapshots = db.list_snapshots("/project".to_string(), 10).unwrap();
    assert_eq!(snapshots.len(), 1);
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.session_title, "Fix the login bug");
    assert_eq!(snapshot.kind, SnapshotKind::BeforeSession);
    assert_eq!(snapshot.file_count, 1);
    assert_eq!(snapshot.skipped_count, 1);
    assert_eq!(snapshot.total_bytes, 7);
    assert!(
        db.list_snapshots("/elsewhere".to_string(), 10)
            .unwrap()
            .is_empty(),
        "snapshots are listed per worktree"
    );

    let files = db.list_files(snapshot_id).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "assets/logo.png");
    assert_eq!(files[0].skip_reason.as_deref(), Some(SKIP_TOO_LARGE));
    assert!(!files[0].is_stored());
    assert_eq!(files[1].path, "src/main.rs");
    assert!(files[1].is_stored());
    assert_eq!(
        db.file_body(snapshot_id, "src/main.rs".to_string())
            .unwrap(),
        Some(vec![1, 2, 3])
    );

    // Trimming past the retention window removes the snapshot and its files.
    db.trim(64 * 1024 * 1024, 0, now()).await.unwrap();
    assert!(
        db.list_snapshots("/project".to_string(), 10)
            .unwrap()
            .is_empty()
    );
    assert!(db.list_files(snapshot_id).unwrap().is_empty());
}

#[gpui::test]
async fn test_list_snapshots_pages_with_keyset(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SessionSnapshotDb::open_test_db("snapshots_paging").await;
    let mut ids = Vec::new();
    for index in 0..5 {
        let session_id = format!("session-{index}");
        let id = db
            .insert_snapshot(NewSnapshot {
                session_id: &session_id,
                session_title: "Chat",
                kind: SnapshotKind::AfterSession,
                root: "/project",
                created_at: 1_000 + index,
            })
            .await
            .unwrap();
        db.finish_snapshot(id, 0, 0, 0).await.unwrap();
        ids.push(id);
    }

    let first = db.list_snapshots("/project".to_string(), 2).unwrap();
    assert_eq!(
        first.iter().map(|snapshot| snapshot.id).collect::<Vec<_>>(),
        vec![ids[4], ids[3]]
    );
    let second = db
        .list_snapshots_before("/project".to_string(), ids[3], 2)
        .unwrap();
    assert_eq!(
        second
            .iter()
            .map(|snapshot| snapshot.id)
            .collect::<Vec<_>>(),
        vec![ids[2], ids[1]]
    );
}

#[gpui::test]
async fn test_trim_removes_the_oldest_snapshot_first(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SessionSnapshotDb::open_test_db("snapshots_trim_bytes").await;
    let mut ids = Vec::new();
    let base = now();
    for index in 0..4 {
        let session_id = format!("session-{index}");
        let id = db
            .insert_snapshot(NewSnapshot {
                session_id: &session_id,
                session_title: "Chat",
                kind: SnapshotKind::AfterSession,
                root: "/project",
                created_at: base + index,
            })
            .await
            .unwrap();
        db.finish_snapshot(id, 1, 0, 10).await.unwrap();
        ids.push(id);
    }

    // 40 bytes stored, a 25 byte budget drops the two oldest.
    db.trim(25, 365 * 24 * 60 * 60, now()).await.unwrap();
    let remaining = db.list_snapshots("/project".to_string(), 10).unwrap();
    assert_eq!(
        remaining
            .iter()
            .map(|snapshot| snapshot.id)
            .collect::<Vec<_>>(),
        vec![ids[3], ids[2]]
    );
}

#[gpui::test]
async fn test_trim_keeps_recent_within_budget(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SessionSnapshotDb::open_test_db("snapshots_trim_keep").await;
    let current = now();
    for (index, created_at) in [current - 10 * 24 * 60 * 60, current - 60, current]
        .iter()
        .enumerate()
    {
        let session_id = format!("session-{index}");
        let id = db
            .insert_snapshot(NewSnapshot {
                session_id: &session_id,
                session_title: "Chat",
                kind: SnapshotKind::BeforeSession,
                root: "/project",
                created_at: *created_at,
            })
            .await
            .unwrap();
        db.finish_snapshot(id, 1, 0, 10).await.unwrap();
    }

    db.trim(1000, 7 * 24 * 60 * 60, current).await.unwrap();
    let snapshots = db.list_snapshots("/project".to_string(), 10).unwrap();
    assert_eq!(snapshots.len(), 2, "only the 10 day old snapshot expires");
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot.created_at != current - 10 * 24 * 60 * 60)
    );
}

#[gpui::test]
async fn test_capture_stores_dirty_files_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);
    std::fs::write(root.join("changed.txt"), "after the agent\n").unwrap();
    std::fs::write(root.join("new.txt"), "a brand new file\n").unwrap();

    let db = SessionSnapshotDb::open_test_db("snapshots_capture").await;
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor.clone()));
    let snapshot_id = capture(&db, fs, root, SnapshotKind::BeforeSession)
        .await
        .unwrap();

    let files = db.list_files(snapshot_id).unwrap();
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["changed.txt", "new.txt"]);
    assert!(files.iter().all(|file| file.is_stored()));

    let changed = &files[0];
    assert_eq!(changed.git_status, "modified");
    assert_eq!(changed.size, "after the agent\n".len() as i64);
    assert!(changed.compressed_size > 0);
    assert_eq!(
        read_file_text(&db, snapshot_id, "changed.txt")
            .unwrap()
            .unwrap(),
        "after the agent\n"
    );
    assert_eq!(files[1].git_status, "untracked");

    let snapshot = db
        .snapshot_summary(snapshot_id)
        .unwrap()
        .expect("the snapshot exists");
    assert_eq!(snapshot.file_count, 2);
    assert_eq!(snapshot.skipped_count, 0);
    assert_eq!(
        snapshot.root,
        root.to_string_lossy(),
        "the worktree root is recorded, not the repository-relative path"
    );
}

#[gpui::test]
async fn test_capture_without_changes_stores_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);

    let db = SessionSnapshotDb::open_test_db("snapshots_clean").await;
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor.clone()));
    capture(&db, fs, root, SnapshotKind::AfterSession)
        .await
        .unwrap();

    assert!(
        db.list_snapshots(root.to_string_lossy().into_owned(), 10)
            .unwrap()
            .is_empty()
    );
}

#[gpui::test]
async fn test_capture_skips_large_binary_and_deleted_files(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);

    std::fs::write(root.join("large.txt"), vec![b'a'; 2 * 1024 * 1024 + 1]).unwrap();
    std::fs::write(root.join("binary.bin"), b"text\0more text").unwrap();
    std::fs::remove_file(root.join("changed.txt")).unwrap();
    std::fs::write(root.join("small.txt"), "kept\n").unwrap();

    let db = SessionSnapshotDb::open_test_db("snapshots_skips").await;
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor.clone()));
    let snapshot_id = capture(&db, fs, root, SnapshotKind::AfterSession)
        .await
        .unwrap();

    let files = db.list_files(snapshot_id).unwrap();
    let reason = |path: &str| {
        files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} is listed"))
            .skip_reason
            .clone()
    };
    assert_eq!(reason("large.txt").as_deref(), Some(SKIP_TOO_LARGE));
    assert_eq!(reason("binary.bin").as_deref(), Some(SKIP_BINARY));
    assert_eq!(reason("changed.txt").as_deref(), Some(SKIP_DELETED));
    assert_eq!(reason("small.txt"), None);
    assert_eq!(
        db.snapshot_summary(snapshot_id)
            .unwrap()
            .unwrap()
            .skipped_count,
        3
    );
    assert!(
        db.file_body(snapshot_id, "large.txt".to_string())
            .unwrap()
            .is_none()
    );
}

#[gpui::test]
async fn test_restore_single_file_and_whole_snapshot(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);
    std::fs::write(root.join("changed.txt"), "the good version\n").unwrap();
    std::fs::write(root.join("new.txt"), "another good version\n").unwrap();

    let db = SessionSnapshotDb::open_test_db("snapshots_restore").await;
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor.clone()));
    let snapshot_id = capture(&db, fs.clone(), root, SnapshotKind::BeforeSession)
        .await
        .unwrap();
    let snapshot = db.snapshot_summary(snapshot_id).unwrap().unwrap();

    std::fs::write(root.join("changed.txt"), "the broken version\n").unwrap();
    std::fs::write(root.join("new.txt"), "also broken\n").unwrap();

    let changed = db
        .list_files(snapshot_id)
        .unwrap()
        .into_iter()
        .find(|file| file.path == "changed.txt")
        .unwrap();
    assert!(
        restore_file(&db, fs.clone(), root, snapshot_id, &changed)
            .await
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("changed.txt")).unwrap(),
        "the good version\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "also broken\n",
        "restoring one file leaves the others alone"
    );

    std::fs::write(root.join("changed.txt"), "broken again\n").unwrap();
    let report = restore_snapshot(&db, fs.clone(), &snapshot).await.unwrap();
    assert_eq!(report.restored, 2);
    assert!(report.failed.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("changed.txt")).unwrap(),
        "the good version\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "another good version\n"
    );
}

#[gpui::test]
async fn test_restore_does_not_touch_skipped_files(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);
    std::fs::write(root.join("binary.bin"), b"one\0two").unwrap();

    let db = SessionSnapshotDb::open_test_db("snapshots_restore_skips").await;
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor.clone()));
    let snapshot_id = capture(&db, fs.clone(), root, SnapshotKind::BeforeSession)
        .await
        .unwrap();
    let snapshot = db.snapshot_summary(snapshot_id).unwrap().unwrap();
    std::fs::write(root.join("binary.bin"), b"changed\0bytes").unwrap();

    let report = restore_snapshot(&db, fs.clone(), &snapshot).await.unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(
        std::fs::read(root.join("binary.bin")).unwrap(),
        b"changed\0bytes"
    );
}

#[gpui::test]
async fn test_delete_snapshots_in_a_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let db = SessionSnapshotDb::open_test_db("snapshots_delete_all").await;
    for (index, root) in ["/project", "/other", "/project"].iter().enumerate() {
        let session_id = format!("session-{index}");
        let id = db
            .insert_snapshot(NewSnapshot {
                session_id: &session_id,
                session_title: "Chat",
                kind: SnapshotKind::AfterSession,
                root,
                created_at: now() + index as i64,
            })
            .await
            .unwrap();
        db.insert_file(
            id,
            NewSnapshotFile {
                path: format!("file-{index}.txt"),
                git_status: "modified".to_string(),
                size: 4,
                mtime: 1,
                sha256: "abc".to_string(),
                compressed_size: 4,
                body: Some(vec![1, 2, 3, 4]),
                skip_reason: None,
            },
        )
        .await
        .unwrap();
        db.finish_snapshot(id, 1, 0, 4).await.unwrap();
    }

    db.delete_snapshots_in_worktree("/project".to_string())
        .await
        .unwrap();
    assert!(
        db.list_snapshots("/project".to_string(), 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.list_snapshots("/other".to_string(), 10).unwrap().len(),
        1,
        "another worktree keeps its snapshots"
    );
    assert!(db.list_files(1).unwrap().is_empty());
}

#[gpui::test]
async fn test_capture_finds_the_worktree_above_a_subdirectory(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path();
    init_repo(root);
    let nested = root.join("crates").join("thing");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("nested.txt"), "nested\n").unwrap();

    assert_eq!(
        find_worktree_root(&nested).as_deref(),
        Some(root),
        "a session started in a subdirectory snapshots its worktree"
    );
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(find_worktree_root(outside.path()), None);
}

#[gpui::test]
fn test_default_json_matches_runtime_defaults(cx: &mut gpui::App) {
    use settings::{
        RootUserSettings as _, Settings as _, SettingsContent, SettingsFile, SettingsStore,
    };

    // What an empty settings file yields, i.e. the values snapshots actually run
    // with when the user has not configured them.
    let code_defaults = SessionSnapshotSettings::from_settings(&SettingsContent::default());

    let defaults = SettingsContent::parse_json_with_comments(&settings::default_settings())
        .expect("assets/settings/default.json parses");
    let section = defaults
        .session_snapshots
        .as_ref()
        .expect("assets/settings/default.json must contain a session_snapshots section");

    // Every key has to be spelled out and equal the code default; a missing key
    // falls back to the code default at runtime but shows as off in settings.
    assert_eq!(
        section.enabled,
        Some(code_defaults.enabled),
        "session_snapshots.enabled in default.json drifted from the code default"
    );
    assert_eq!(
        section.max_total_mb,
        Some(code_defaults.max_total_mb),
        "session_snapshots.max_total_mb in default.json drifted from the code default"
    );
    assert_eq!(
        section.retention_days,
        Some(code_defaults.retention_days),
        "session_snapshots.retention_days in default.json drifted from the code default"
    );

    // The bool toggle renders the value this pick returns, so it must show what
    // sessions run with rather than a missing-key false.
    let mut store = SettingsStore::new(cx, &settings::default_settings());
    store.register_setting::<SessionSnapshotSettings>();
    let (_, shown) = store.get_value_from_file(SettingsFile::User, |content| {
        content.session_snapshots.as_ref()?.enabled
    });
    assert_eq!(
        shown,
        Some(store.get::<SessionSnapshotSettings>(None).enabled),
        "the settings page shows a different snapshot default than sessions use"
    );
}
