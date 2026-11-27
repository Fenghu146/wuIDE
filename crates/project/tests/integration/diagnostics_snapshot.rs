use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs::{CreateOptions, FakeFs, Fs};
use futures::StreamExt;
use gpui::TestAppContext;
use project::Project;
use language::{Diagnostic, DiagnosticEntry, DiagnosticMessage, PointUtf16, Unclipped};
use lsp::{DiagnosticSeverity, NumberOrString};
use path::rel_path::RelPath;
use project::diagnostics_snapshot::{
    Item, MAX_ITEMS, MAX_MESSAGE_CHARS, is_current_refresh_request, is_refresh_request, to_json,
    watch_refresh_requests, write,
};
use serde_json::json;
use util::path;

fn rel(path: &str) -> &RelPath {
    RelPath::from_unix_str(path).unwrap()
}

fn make_entry(
    row: u32,
    column: u32,
    severity: DiagnosticSeverity,
) -> DiagnosticEntry<Unclipped<PointUtf16>> {
    DiagnosticEntry {
        range: Unclipped(PointUtf16::new(row, column))..Unclipped(PointUtf16::new(row, column + 1)),
        diagnostic: Diagnostic {
            severity,
            code: Some(NumberOrString::String("expected_semi_after_stmt".into())),
            message: DiagnosticMessage::from("Expected ';' after return statement"),
            source: Some("clang".into()),
            is_primary: true,
            ..Diagnostic::default()
        },
        related_information: None,
    }
}

#[test]
fn item_is_one_based_and_keeps_severity_code_and_source() {
    let path = rel("src/activation_manager_fh.hpp");
    let item = Item::new(path, &make_entry(41, 4, DiagnosticSeverity::ERROR)).unwrap();

    assert_eq!(item.path, "src/activation_manager_fh.hpp");
    assert_eq!((item.line, item.column), (42, 5));
    assert_eq!(item.severity, "error");
    assert_eq!(item.code, "expected_semi_after_stmt");
    assert_eq!(item.message, "Expected ';' after return statement");
    assert_eq!(item.source, "clang");
}

#[test]
fn item_maps_severity_and_tolerates_missing_fields() {
    let mut entry = make_entry(0, 0, DiagnosticSeverity::WARNING);
    entry.diagnostic.code = None;
    entry.diagnostic.source = None;
    let item = Item::new(rel("a.rs"), &entry).unwrap();
    assert_eq!(item.severity, "warning");
    assert_eq!(item.code, "");
    assert_eq!(item.source, "");

    let item = Item::new(rel("a.rs"), &make_entry(0, 0, DiagnosticSeverity::HINT)).unwrap();
    assert_eq!(item.severity, "info");
}

#[test]
fn item_skips_secondary_diagnostics() {
    let mut entry = make_entry(0, 0, DiagnosticSeverity::ERROR);
    entry.diagnostic.is_primary = false;
    assert!(
        Item::new(rel("a.rs"), &entry).is_none(),
        "a secondary diagnostic only elaborates on its primary one"
    );
}

#[test]
fn item_cuts_long_messages_at_the_character_limit() {
    let mut entry = make_entry(0, 0, DiagnosticSeverity::ERROR);
    entry.diagnostic.message = DiagnosticMessage::from("é".repeat(MAX_MESSAGE_CHARS + 20));
    let item = Item::new(rel("a.rs"), &entry).unwrap();

    assert_eq!(item.message.chars().count(), MAX_MESSAGE_CHARS);
    assert!(item.message.starts_with('é'));
}

#[test]
fn json_matches_the_published_shape() {
    let items = vec![
        Item::new(
            rel("src/main.cpp"),
            &make_entry(41, 4, DiagnosticSeverity::ERROR),
        )
        .unwrap(),
    ];
    let doc = to_json(Path::new("/work/project"), "2026-01-02T03:04:05Z", &items);

    assert_eq!(doc["version"], 1);
    assert_eq!(doc["project"], "/work/project");
    assert_eq!(doc["generated_at"], "2026-01-02T03:04:05Z");
    assert_eq!(
        doc["items"],
        json!([{
            "path": "src/main.cpp",
            "line": 42,
            "column": 5,
            "severity": "error",
            "code": "expected_semi_after_stmt",
            "message": "Expected ';' after return statement",
            "source": "clang",
        }])
    );
    assert!(
        doc.get("truncated").is_none(),
        "a complete snapshot must not claim truncation"
    );
}

#[test]
fn json_caps_items_and_marks_truncation() {
    let entry = make_entry(0, 0, DiagnosticSeverity::ERROR);
    let items: Vec<Item> = (0..MAX_ITEMS + 5)
        .map(|index| Item::new(rel(&format!("src/{index}.rs")), &entry).unwrap())
        .collect();
    let doc = to_json(Path::new("/work/project"), "2026-01-02T03:04:05Z", &items);

    assert_eq!(doc["items"].as_array().unwrap().len(), MAX_ITEMS);
    assert_eq!(doc["truncated"], true);
}

#[test]
fn json_marks_truncation_when_a_message_was_cut() {
    let mut entry = make_entry(0, 0, DiagnosticSeverity::ERROR);
    entry.diagnostic.message = DiagnosticMessage::from("x".repeat(MAX_MESSAGE_CHARS + 1));
    let items = vec![Item::new(rel("a.rs"), &entry).unwrap()];
    let doc = to_json(Path::new("/work/project"), "2026-01-02T03:04:05Z", &items);

    assert_eq!(doc["truncated"], true);
    assert_eq!(
        doc["items"][0]["message"].as_str().unwrap().chars().count(),
        MAX_MESSAGE_CHARS
    );
}

#[test]
fn write_uses_the_same_key_as_the_repo_map_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("diagnostics");
    let written = write(Path::new("/work/project"), &target, &[]).unwrap();

    assert_eq!(
        written,
        target.join(format!(
            "{}.json",
            paths::project_key(Path::new("/work/project"))
        ))
    );
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["items"], json!([]));
}

#[gpui::test]
async fn only_refresh_markers_reach_the_watcher(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    let dir = Path::new("/agent/diagnostics").to_path_buf();
    let (mut requests, _watcher) = watch_refresh_requests(fs.clone(), dir.clone()).await;

    let key = paths::project_key(Path::new("/work/project"));
    let snapshot = dir.join(format!("{key}.json"));
    let marker = dir.join(format!("{key}.refresh"));
    fs.create_file(&snapshot, CreateOptions::default())
        .await
        .unwrap();
    fs.create_file(&marker, CreateOptions::default())
        .await
        .unwrap();

    assert!(is_refresh_request(&marker));
    assert!(!is_refresh_request(&snapshot));
    assert_eq!(
        requests.next().await.expect("a refresh marker is delivered"),
        vec![marker]
    );
}

#[gpui::test]
async fn a_refresh_marker_rewrites_the_snapshot(cx: &mut TestAppContext) {
    super::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/work"), json!({ "a.rs": "" }))
        .await;
    let _project = Project::test(fs.clone(), [Path::new(path!("/work"))], cx).await;
    cx.run_until_parked();

    let dir = paths::diagnostics_snapshots_dir();
    let key = paths::project_key(Path::new(path!("/work")));
    let marker = dir.join(format!("{key}.refresh"));
    let snapshot = dir.join(format!("{key}.json"));
    fs.create_file(&marker, CreateOptions::default())
        .await
        .unwrap();
    let requested_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string();
    fs.write(&marker, requested_at.as_bytes()).await.unwrap();
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();

    // The writer uses `std::fs`, so the resulting snapshot is a real file even
    // when the project's other files live in FakeFs.
    let written = std::fs::read_to_string(&snapshot).expect("snapshot was rewritten");
    std::fs::remove_file(&snapshot).expect("remove the test snapshot");
    let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["project"], path!("/work"));
    assert_eq!(doc["items"], json!([]));
    assert!(fs.metadata(&marker).await.unwrap().is_none());
}

#[test]
fn refresh_requests_expire_after_five_minutes() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let millis = |time: SystemTime| time.duration_since(UNIX_EPOCH).unwrap().as_millis();

    assert!(is_current_refresh_request(
        format!("{}\n", millis(now)).as_bytes(),
        now
    ));
    assert!(!is_current_refresh_request(
        format!("{}", millis(now - Duration::from_secs(10 * 60))).as_bytes(),
        now
    ));
    assert!(!is_current_refresh_request(b"no clock here", now));
}

#[gpui::test]
async fn startup_sweeps_abandoned_markers(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    let dir = Path::new("/agent/diagnostics").to_path_buf();
    fs.create_dir(&dir).await.unwrap();

    let abandoned = dir.join("abandoned.refresh");
    fs.create_file(&abandoned, CreateOptions::default())
        .await
        .unwrap();
    fs.write(&abandoned, b"1000\n").await.unwrap();

    let current = dir.join("current.refresh");
    fs.create_file(&current, CreateOptions::default())
        .await
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string();
    fs.write(&current, now.as_bytes()).await.unwrap();

    let (mut requests, _watcher) = watch_refresh_requests(fs.clone(), dir).await;
    assert!(
        fs.metadata(&abandoned).await.unwrap().is_none(),
        "a marker from a closed session must not linger"
    );
    assert_eq!(
        requests.next().await.expect("the live marker is replayed"),
        vec![current]
    );
}
