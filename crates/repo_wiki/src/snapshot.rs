//! JSON snapshot of one worktree's symbol map.
//!
//! The repo map panel writes a file per project root so the bundled Pi MCP
//! server can answer `repo_map` and `symbols` with no IPC channel: Pi runs the
//! server inside the project directory, and both sides derive the same file
//! name from that path.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::symbol_map::SymbolMap;

/// Bump when the layout changes. The server reads version 2, falls back to
/// name/line/kind for version 1 files, and refuses files it does not know.
pub const SNAPSHOT_VERSION: u32 = 2;

/// Bytes of the SHA-256 digest that name a project's snapshot (16 hex digits).
const KEY_BYTES: usize = 8;

/// A stable file name for one project root.
///
/// The server only sees its working directory, so both sides normalize the same
/// way: forward slashes, no trailing slash, lowercased on Windows.
/// `agents/mcp/wu-repo-map/server.js` repeats this rule and must stay in step.
pub fn project_key(project_root: &Path) -> String {
    let normalized = normalize_project_root(project_root);
    let digest = Sha256::digest(normalized.as_bytes());
    digest[..KEY_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize_project_root(project_root: &Path) -> String {
    let mut path = project_root.to_string_lossy().replace('\\', "/");
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if cfg!(windows) {
        path = path.to_lowercase();
    }
    path
}

/// Where the snapshot for `key` lives inside `dir`.
pub fn snapshot_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

/// The snapshot document: only files that define something, best-ranked first.
///
/// `generated_at` is passed in so tests can pin it.
pub fn to_json(map: &SymbolMap, project_root: &Path, generated_at: &str) -> Value {
    let files = map.json_entries();
    // Files the map parsed but that define nothing stay out of `files` (their
    // semantics do not change), yet the server still needs to know about them
    // to answer `symbols` honestly.
    let empty_files: Vec<String> = map
        .files
        .iter()
        .filter(|file| file.symbols.is_empty())
        .map(|file| file.rel.display(path::PathStyle::Unix).into_owned())
        .collect();
    json!({
        "version": SNAPSHOT_VERSION,
        "project": project_root.to_string_lossy(),
        "generated_at": generated_at,
        "order": SymbolMap::order_description(),
        "max_chars": crate::symbol_map::MAX_REPO_MAP_CHARS,
        "stats": {
            "considered": map.stats.considered,
            "parsed": map.stats.parsed,
            "too_large": map.stats.too_large,
            "unsupported": map.stats.unsupported,
        },
        "files": files,
        "empty_files": empty_files,
    })
}

/// Writes one project's snapshot into `dir`.
///
/// Errors belong to the caller to log: a missing snapshot only costs the agent
/// its structural hint.
pub fn write(map: &SymbolMap, project_root: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let path = snapshot_path(dir, &project_key(project_root));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = serde_json::to_vec(&to_json(map, project_root, &generated_at))
        .map_err(std::io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use path::PathStyle;
    use path::rel_path::RelPath;

    use super::*;
    use crate::symbol_map::{FileSymbols, SymbolEntry};

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

    fn file(rel: &str, rank: f32, symbols: Vec<SymbolEntry>) -> FileSymbols {
        FileSymbols {
            rel: RelPath::new(Path::new(rel), PathStyle::local())
                .unwrap()
                .into_arc(),
            size: 120,
            rank,
            lines: 42,
            in_degree: 2,
            out_degree: 1,
            symbols,
        }
    }

    fn map_with_two_files() -> SymbolMap {
        SymbolMap {
            files: vec![
                file("src/main.rs", 2.500_04, vec![entry("main", "function", 11)]),
                file("src/lib.rs", 0.5, Vec::new()),
            ],
            ..SymbolMap::default()
        }
    }

    #[test]
    fn key_is_hex_and_stable_across_separators() {
        let key = project_key(Path::new("D:/work/project"));
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));

        assert_eq!(key, project_key(Path::new("D:\\work\\project")));
        assert_eq!(key, project_key(Path::new("D:/work/project/")));
    }

    #[cfg(windows)]
    #[test]
    fn key_ignores_case_on_windows() {
        assert_eq!(
            project_key(Path::new("D:/Work/Project")),
            project_key(Path::new("d:/work/project"))
        );
    }

    #[test]
    fn different_roots_get_different_keys() {
        assert_ne!(
            project_key(Path::new("/work/one")),
            project_key(Path::new("/work/two"))
        );
    }

    #[test]
    fn json_carries_only_files_with_symbols() {
        let map = map_with_two_files();
        let json = to_json(&map, Path::new("/work/project"), "2026-01-02T03:04:05Z");

        assert_eq!(json["version"], 2);
        assert_eq!(json["project"], "/work/project");
        assert_eq!(json["generated_at"], "2026-01-02T03:04:05Z");
        assert!(json["order"].as_str().unwrap().contains("include fan-in"));

        // src/lib.rs defines nothing, so the server cannot say anything about
        // it beyond "this file exists and is empty".
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "src/main.rs");
        assert_eq!(files[0]["rank"], 2.5);
        assert_eq!(files[0]["lines"], 42);
        assert_eq!(files[0]["in"], 2);
        assert_eq!(files[0]["out"], 1);
        assert_eq!(files[0]["symbols"][0]["name"], "main");
        assert_eq!(files[0]["symbols"][0]["kind"], "function");
        // One-based, because the agent opens files by line number.
        assert_eq!(files[0]["symbols"][0]["line"], 12);
        assert_eq!(files[0]["symbols"][0]["local"], false);
        assert_eq!(json["empty_files"], json!(["src/lib.rs"]));
    }

    #[test]
    fn write_creates_the_directory_and_one_file_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("repo-maps");
        let map = map_with_two_files();

        let first = write(&map, Path::new("/work/one"), &target).unwrap();
        let second = write(&map, Path::new("/work/two"), &target).unwrap();

        assert!(first.is_file() && second.is_file());
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(target.as_path()));
        let written: Value = serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
        assert_eq!(written["files"][0]["path"], "src/main.rs");
    }
}
