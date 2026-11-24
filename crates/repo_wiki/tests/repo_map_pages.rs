//! One rendering, two implementations, one set of bytes.
//!
//! The WuIDE renders a repo map page in Rust and the bundled MCP server renders
//! it again in JavaScript from the snapshot Rust wrote. An agent reads whichever
//! one its environment gives it, so the two must produce the same text for the
//! same snapshot — including the paging and omission rules. These tests pin both
//! sides to the golden pages under `tests/fixtures/repo-map`, which
//! `server.test.js` compares against too.

use std::path::{Path, PathBuf};

use path::rel_path::RelPath;
use repo_wiki::symbol_map::{
    BuildStats, FileSymbols, MAX_REPO_MAP_CHARS, RepoMapOptions, SymbolEntry, SymbolMap,
};

/// The paging shapes an agent asks for, named by the golden file each one
/// renders into.
const CASES: &[(&str, RepoMapOptions)] = &[
    (
        "limit-1",
        RepoMapOptions {
            limit: 1,
            offset: 0,
            files_only: false,
            include_locals: false,
        },
    ),
    (
        "limit-10",
        RepoMapOptions {
            limit: 10,
            offset: 0,
            files_only: false,
            include_locals: false,
        },
    ),
    (
        "limit-40",
        RepoMapOptions {
            limit: 40,
            offset: 0,
            files_only: false,
            include_locals: false,
        },
    ),
    (
        "files-only",
        RepoMapOptions {
            limit: 10,
            offset: 0,
            files_only: true,
            include_locals: false,
        },
    ),
    (
        "offset-past-end",
        RepoMapOptions {
            limit: 40,
            offset: 51,
            files_only: false,
            include_locals: false,
        },
    ),
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/repo-map")
}

fn read_fixture() -> serde_json::Value {
    let path = fixture_dir().join("snapshot.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    serde_json::from_slice(&bytes).expect("fixture is JSON")
}

fn string(value: &serde_json::Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
        .to_owned()
}

fn number(value: &serde_json::Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} must be an unsigned number"))
}

fn optional_string(value: &serde_json::Value, key: &str) -> Option<std::sync::Arc<str>> {
    value[key].as_str().map(Into::into)
}

fn symbol(value: &serde_json::Value) -> SymbolEntry {
    SymbolEntry {
        name: string(value, "name").into(),
        kind: string(value, "kind").into(),
        // The document carries one-based lines; the map works with rows.
        row: number(value, "line") as u32 - 1,
        owner: optional_string(value, "owner"),
        depth: number(value, "depth") as u32,
        access: optional_string(value, "access"),
        signature: optional_string(value, "signature"),
        local: value["local"].as_bool().unwrap_or(false),
    }
}

/// The snapshot document as the map that wrote it: same files, same order, same
/// coverage counters, so the Rust renderer starts from exactly what the server
/// starts from.
fn map_from_fixture(doc: &serde_json::Value) -> SymbolMap {
    let files = doc["files"]
        .as_array()
        .expect("files is an array")
        .iter()
        .map(|file| FileSymbols {
            rel: RelPath::new(Path::new(&string(file, "path")), path::PathStyle::local())
                .expect("valid path")
                .into_arc(),
            size: number(file, "size"),
            rank: file["rank"].as_f64().expect("rank is a number") as f32,
            lines: number(file, "lines") as u32,
            in_degree: number(file, "in") as u32,
            out_degree: number(file, "out") as u32,
            dep_in_degree: number(file, "deps") as u32,
            symbols: file["symbols"]
                .as_array()
                .expect("symbols is an array")
                .iter()
                .map(symbol)
                .collect(),
        })
        .collect();

    let stats = &doc["stats"];
    SymbolMap {
        files,
        stats: BuildStats {
            considered: number(stats, "considered") as usize,
            parsed: number(stats, "parsed") as usize,
            too_large: number(stats, "too_large") as usize,
            unsupported: number(stats, "unsupported") as usize,
            unreadable: number(stats, "unreadable") as usize,
            over_budget: number(stats, "over_budget") as usize,
            file_limit: number(stats, "file_limit") as usize,
            vendored_skipped: number(stats, "vendored_skipped") as usize,
        },
    }
}

#[test]
fn the_fixture_states_the_ordering_the_ide_prints() {
    let doc = read_fixture();
    assert_eq!(
        doc["order"].as_str(),
        Some(SymbolMap::order_description()),
        "the fixture's order note must be the one WuIDE writes"
    );
    assert_eq!(doc["max_chars"].as_u64(), Some(MAX_REPO_MAP_CHARS as u64));
}

#[test]
fn pages_match_the_golden_rendering_byte_for_byte() {
    let doc = read_fixture();
    let map = map_from_fixture(&doc);
    let max_chars = doc["max_chars"].as_u64().expect("max_chars") as usize;

    for (name, options) in CASES {
        let path = fixture_dir().join(format!("{name}.md"));
        let expected =
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
        let actual = map.repo_map_markdown(options, max_chars);
        assert_eq!(actual, expected, "{name} differs from its golden page");
    }
}

/// Regenerates the golden pages. Run it by hand after an intentional change to
/// the rendering:
/// `REPO_MAP_WRITE_GOLDENS=1 cargo test -p repo_wiki --test repo_map_pages`.
#[test]
fn write_goldens_when_asked() {
    if std::env::var_os("REPO_MAP_WRITE_GOLDENS").is_none() {
        return;
    }
    let doc = read_fixture();
    let map = map_from_fixture(&doc);
    let max_chars = doc["max_chars"].as_u64().expect("max_chars") as usize;
    for (name, options) in CASES {
        let page = map.repo_map_markdown(options, max_chars);
        std::fs::write(fixture_dir().join(format!("{name}.md")), page).expect("golden written");
    }
}
