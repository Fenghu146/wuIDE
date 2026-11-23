//! Caches for the repository map: per-file parse results on disk, and the last
//! built map in memory.
//!
//! Parsing a repository of a few thousand files with tree-sitter costs about
//! twenty seconds, yet two builds in a row almost always see the same bytes.
//! This module keeps what a build learned in two places:
//!
//! * On disk, one document per project holding every parsed file's symbols and
//!   mentions, keyed by the file's path, size and modification time. A later
//!   build re-parses only the files whose stat no longer matches.
//! * In memory, the last map built for a project, so the panel and `/repo-map`
//!   share one build instead of racing two.
//!
//! The cache is an optimisation and never a source of truth. Every entry is
//! checked against the file's stat before it is used, a document that cannot be
//! read or parsed is discarded, and a write that fails is logged: the map is
//! always what the files on disk say, and a cache entry that disagrees with
//! them is dropped rather than trusted.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

use crate::symbol_map::SymbolMap;

/// Bump when the document layout, or what a payload means, changes. Documents
/// written by another version are ignored instead of reinterpreted.
///
/// Version 2 added each file's `family` (the language group its names belong to)
/// and its include specs; a version 1 document has neither, and reusing it would
/// silently rank the map with every file in one family and no dependencies.
const CACHE_VERSION: u32 = 2;

/// Total bytes the cache may occupy on disk.
///
/// A project's document is roughly the size of its source tree in symbols and
/// identifiers (about 40 MB for this repository), so this holds a handful of
/// projects. Beyond it the least recently built project is dropped, and a
/// project whose own document does not fit is left uncached: a cache is an
/// optimisation, so running without one is a slower build, not an error.
const MAX_CACHE_BYTES: u64 = 150 * 1024 * 1024;

/// Maps kept in memory. Each holds one project's whole map, a few megabytes;
/// this is enough for the projects a session has open and keeps the map bounded
/// for a process that visits many repositories.
const MAX_REMEMBERED_MAPS: usize = 8;

/// One file as the cache needs to recognize it: its path, size and modification
/// time. A file whose stamp is unchanged is assumed to hold the bytes it held
/// when it was parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    /// Absolute path, in the same spelling the cache uses as its key.
    pub path: String,
    /// Length in bytes as the filesystem reports it.
    pub file_size: u64,
    /// Modification time in nanoseconds since the Unix epoch, or 0 when the
    /// filesystem cannot say.
    pub modified_ns: i64,
}

/// One parsed file in the document, with the stat it was parsed under.
#[derive(Serialize, Deserialize)]
pub(crate) struct CacheEntry<T> {
    pub file_size: u64,
    pub modified_ns: i64,
    pub payload: T,
}

#[derive(Serialize)]
struct CacheDocumentRef<'a, T> {
    version: u32,
    project: &'a str,
    entries: &'a [CacheEntry<T>],
}

/// The read side of a document. `project` is written for whoever has to debug
/// a cache on disk, but nothing depends on it, so it is not read back.
#[derive(Deserialize)]
struct CacheDocument<T> {
    version: u32,
    entries: Vec<CacheEntry<T>>,
}

/// The name of `scope`'s cache document inside `dir`.
///
/// `paths::project_key` normalizes the same way, so a worktree spelled with
/// either separator maps to one cache; the whole root list is hashed so a map
/// covering several worktrees never collides with the map of one of them.
pub(crate) fn key(roots: &[PathBuf]) -> String {
    let mut roots: Vec<String> = roots.iter().map(|root| normalized(root)).collect();
    roots.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"repo-map-cache-v1\n");
    for root in &roots {
        hasher.update(root.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalized(root: &Path) -> String {
    let mut normalized = root.to_string_lossy().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if cfg!(windows) {
        normalized = normalized.to_lowercase();
    }
    normalized
}

/// Where `key`'s document lives inside `dir`.
pub(crate) fn cache_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

/// Reads the cached parses for `key`, or `None` when there is nothing usable.
///
/// A document that is missing, truncated, written by another version, or not
/// valid JSON for `T` is discarded: the caller parses those files again.
pub(crate) fn load<T: DeserializeOwned>(dir: &Path, key: &str) -> Option<Vec<CacheEntry<T>>> {
    let path = cache_path(dir, key);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::error!("repo map: cannot read the parse cache {path:?}: {error}");
            return None;
        }
    };
    let document: CacheDocument<T> = match serde_json::from_slice(&bytes) {
        Ok(document) => document,
        Err(error) => {
            log::error!(
                "repo map: dropping the unreadable parse cache {path:?}: {error}; \
                 every file will be parsed again"
            );
            return None;
        }
    };
    if document.version != CACHE_VERSION {
        log::info!(
            "repo map: ignoring parse cache version {} (expected {CACHE_VERSION})",
            document.version
        );
        return None;
    }
    // The document is in use, so it is not the least recently used one.
    touch(&path);
    Some(document.entries)
}

/// Writes `entries` as `key`'s document and keeps the directory inside its
/// budget. Errors are logged: the map itself is already built.
pub(crate) fn store<T: Serialize>(
    dir: &Path,
    key: &str,
    project: &str,
    entries: &[CacheEntry<T>],
) {
    if let Err(error) = store_with_budget(dir, key, project, entries, MAX_CACHE_BYTES) {
        log::error!("repo map: failed to write the parse cache: {error}");
    }
}

fn store_with_budget<T: Serialize>(
    dir: &Path,
    key: &str,
    project: &str,
    entries: &[CacheEntry<T>],
    budget: u64,
) -> std::io::Result<()> {
    let path = cache_path(dir, key);
    if entries.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    std::fs::create_dir_all(dir)?;
    let document = CacheDocumentRef {
        version: CACHE_VERSION,
        project,
        entries,
    };
    let body = serde_json::to_vec(&document).map_err(std::io::Error::other)?;

    // Write beside the document and rename over it, so a build interrupted
    // mid-write leaves the previous document rather than half of a new one.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, &body)?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(cleanup) => log::error!("repo map: cannot remove {temporary:?}: {cleanup}"),
        }
        return Err(error);
    }
    trim(dir, &path, budget);
    Ok(())
}

/// Drops the least recently built documents until the directory fits `budget`.
///
/// The document just written is never dropped to make room: when it alone
/// exceeds the budget, the project is left uncached so the next build parses
/// everything again instead of every build paying for a cache that never fits.
fn trim(dir: &Path, written: &Path, budget: u64) {
    let mut documents: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total = 0u64;
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        log::error!("repo map: cannot read an entry of {dir:?}: {error}");
                        continue;
                    }
                };
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        log::error!("repo map: cannot stat {:?}: {error}", entry.path());
                        continue;
                    }
                };
                if !metadata.is_file() {
                    continue;
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                total += metadata.len();
                documents.push((entry.path(), metadata.len(), modified));
            }
        }
        Err(error) => {
            log::error!("repo map: cannot list the cache directory {dir:?}: {error}");
            return;
        }
    }
    if total <= budget {
        return;
    }

    documents.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in documents {
        if total <= budget {
            return;
        }
        if path == written {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                log::info!("repo map: dropped the parse cache {path:?} to stay inside the budget");
            }
            Err(error) => log::error!("repo map: cannot remove {path:?}: {error}"),
        }
    }
    if total > budget {
        log::error!(
            "repo map: {written:?} alone is {total} bytes, over the {budget}-byte budget; \
             leaving this project uncached"
        );
        if let Err(error) = std::fs::remove_file(written) {
            log::error!("repo map: cannot remove {written:?}: {error}");
        }
    }
}

fn touch(path: &Path) {
    let file = match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error) => {
            log::debug!("repo map: cannot touch {path:?}: {error}");
            return;
        }
    };
    if let Err(error) = file.set_modified(SystemTime::now()) {
        log::debug!("repo map: cannot touch {path:?}: {error}");
    }
}

/// A map kept in memory with the stamps it was built from.
struct RememberedMap {
    map: Arc<SymbolMap>,
    built_at: DateTime<Utc>,
    stamps: Vec<FileStamp>,
}

fn remembered() -> &'static Mutex<HashMap<String, RememberedMap>> {
    static REMEMBERED: OnceLock<Mutex<HashMap<String, RememberedMap>>> = OnceLock::new();
    REMEMBERED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remembered_maps() -> MutexGuard<'static, HashMap<String, RememberedMap>> {
    remembered()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The remembered map for `key`, when every file it was built from still has
/// the stamp it had then.
///
/// Comparing stamps against the filesystem, rather than trusting a change
/// event, is what makes a stale map impossible to hand out: an edit, a
/// deletion, an addition and a branch switch all change some file's stat, and
/// nothing else does.
pub(crate) fn reuse(key: &str, stamps: &[FileStamp]) -> Option<(Arc<SymbolMap>, DateTime<Utc>)> {
    let maps = remembered_maps();
    let entry = maps.get(key)?;
    if entry.stamps != stamps {
        return None;
    }
    Some((entry.map.clone(), entry.built_at))
}

/// Keeps `map` as the current map for `key`, replacing what was remembered.
pub(crate) fn remember(
    key: &str,
    map: Arc<SymbolMap>,
    built_at: DateTime<Utc>,
    stamps: Vec<FileStamp>,
) {
    let mut maps = remembered_maps();
    if !maps.contains_key(key) && maps.len() >= MAX_REMEMBERED_MAPS {
        let oldest = maps
            .iter()
            .min_by_key(|(_, entry)| entry.built_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            maps.remove(&oldest);
        }
    }
    maps.insert(
        key.to_owned(),
        RememberedMap {
            map,
            built_at,
            stamps,
        },
    );
}

/// Forgets the remembered map for `key`, so the next build takes the files and
/// the disk cache as they are now. The disk cache stays: a manual refresh asks
/// for a fresh look, not for everything to be parsed again.
pub(crate) fn forget(key: &str) {
    remembered_maps().remove(key);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn entry(path: &str, size: u64, modified_ns: i64) -> CacheEntry<String> {
        CacheEntry {
            file_size: size,
            modified_ns,
            payload: path.to_owned(),
        }
    }

    fn write_document(dir: &Path, key: &str, entries: &[CacheEntry<String>], budget: u64) {
        store_with_budget(dir, key, "test", entries, budget).expect("the document is written");
    }

    #[test]
    fn a_key_is_hex_and_ignores_separator_spelling() {
        let backslashes = key(&[PathBuf::from("D:\\work\\project")]);
        let slashes = key(&[PathBuf::from("D:/work/project/")]);

        assert_eq!(backslashes.len(), 16);
        assert!(backslashes.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(backslashes, slashes);
        assert_ne!(
            backslashes,
            key(&[PathBuf::from("D:/work/project"), PathBuf::from("D:/other")]),
            "a map over several worktrees is a different map"
        );
        assert_eq!(
            key(&[PathBuf::from("D:/a"), PathBuf::from("D:/b")]),
            key(&[PathBuf::from("D:/b"), PathBuf::from("D:/a")]),
            "the key must not depend on the order roots were opened in"
        );
    }

    #[test]
    fn a_written_document_reads_back_with_its_stamps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let entries = vec![entry("src/main.rs", 42, 1_700_000_000_000_000_000)];

        write_document(dir.path(), "abc", &entries, MAX_CACHE_BYTES);
        let loaded = load::<String>(dir.path(), "abc").expect("the document loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].file_size, 42);
        assert_eq!(loaded[0].modified_ns, 1_700_000_000_000_000_000);
        assert_eq!(loaded[0].payload, "src/main.rs");
    }

    #[test]
    fn a_truncated_document_is_dropped_instead_of_trusted() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_document(dir.path(), "abc", &[entry("src/main.rs", 1, 1)], MAX_CACHE_BYTES);

        let path = cache_path(dir.path(), "abc");
        let bytes = std::fs::read(&path).expect("the document was written");
        std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("the document is truncated");

        assert!(load::<String>(dir.path(), "abc").is_none());
        assert!(path.is_file(), "an unreadable document is left for the next write to replace");
    }

    #[test]
    fn a_document_from_another_version_is_ignored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = cache_path(dir.path(), "abc");
        std::fs::write(
            &path,
            br#"{"version":999,"project":"test","entries":[{"file_size":1,"modified_ns":1,"payload":"x"}]}"#,
        )
        .expect("a future document is written");

        assert!(load::<String>(dir.path(), "abc").is_none());
    }

    #[test]
    fn the_oldest_documents_are_dropped_to_stay_inside_the_budget() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body = "x".repeat(2_000);
        write_document(dir.path(), "old", &[entry(&body, 1, 1)], MAX_CACHE_BYTES);
        let old = cache_path(dir.path(), "old");
        let past = SystemTime::now() - std::time::Duration::from_secs(3_600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("the old document opens")
            .set_modified(past)
            .expect("the old document is backdated");

        // Two documents of about 2 KB each, with room for one of them.
        write_document(dir.path(), "new", &[entry(&body, 1, 2)], 3_000);

        assert!(cache_path(dir.path(), "new").is_file(), "the newest document stays");
        assert!(!old.is_file(), "the least recently used document is dropped");
    }

    #[test]
    fn a_document_larger_than_the_whole_budget_is_left_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body = "x".repeat(4_000);

        write_document(dir.path(), "huge", &[entry(&body, 1, 1)], 1_000);

        assert!(
            !cache_path(dir.path(), "huge").is_file(),
            "a document that cannot fit must not be kept, and must not fail the build"
        );
    }

    #[test]
    fn a_remembered_map_is_reused_only_while_its_stamps_hold() {
        let map = Arc::new(SymbolMap::default());
        let stamp = FileStamp {
            path: "/work/src/main.rs".to_owned(),
            file_size: 10,
            modified_ns: 5,
        };
        let built_at = Utc::now();

        remember("test-reuse", map.clone(), built_at, vec![stamp.clone()]);
        let (remembered, at) =
            reuse("test-reuse", std::slice::from_ref(&stamp)).expect("the map is reused");
        assert!(Arc::ptr_eq(&remembered, &map), "reuse must not rebuild the map");
        assert_eq!(at, built_at);

        let edited = FileStamp {
            modified_ns: 6,
            ..stamp.clone()
        };
        assert!(reuse("test-reuse", std::slice::from_ref(&edited)).is_none(), "an edited file invalidates");

        let grown = FileStamp {
            file_size: 11,
            ..stamp.clone()
        };
        assert!(reuse("test-reuse", std::slice::from_ref(&grown)).is_none(), "a resized file invalidates");
        assert!(
            reuse("test-reuse", &[]).is_none(),
            "a file that is gone invalidates"
        );

        forget("test-reuse");
        assert!(reuse("test-reuse", std::slice::from_ref(&stamp)).is_none(), "a manual refresh invalidates");
    }
}
