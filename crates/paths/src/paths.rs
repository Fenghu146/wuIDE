//! Paths to locations used by WuIDE.

use anyhow::Context as _;
use sha2::Digest as _;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use util::paths::SanitizedPath;
pub use util::paths::home_dir;
use util::rel_path::RelPath;

/// A default editorconfig file name to use when resolving project settings.
pub const EDITORCONFIG_NAME: &str = ".editorconfig";

/// The application name, used to derive platform-specific data, config, cache,
/// and state directory paths.
///
/// Forks should change this to avoid colliding with Zed's user data. This fork
/// uses its own name rather than the product name so that its profile cannot
/// collide with another application that happens to be called IDE.
pub const APP_NAME: &str = "WuIDE";

/// Lowercased form of [`APP_NAME`], for use in XDG-style paths on
/// Linux/FreeBSD and the macOS `~/.config` fallback.
pub const APP_NAME_LOWERCASE: &str = {
    assert!(!APP_NAME.is_empty(), "APP_NAME must not be empty");
    assert!(APP_NAME.as_bytes().is_ascii(), "APP_NAME must be ASCII");
    const BYTES: [u8; APP_NAME.len()] = {
        let mut bytes = [0u8; APP_NAME.len()];
        let mut i = 0;
        while i < APP_NAME.len() {
            assert!(
                APP_NAME.as_bytes()[i] != b'/' && APP_NAME.as_bytes()[i] != b'\\',
                "APP_NAME must not contain path separators",
            );
            assert!(
                APP_NAME.as_bytes()[i] >= 0x20,
                "APP_NAME must not contain control characters"
            );
            bytes[i] = APP_NAME.as_bytes()[i];
            i += 1;
        }
        bytes.make_ascii_lowercase();
        bytes
    };
    match std::str::from_utf8(&BYTES) {
        Ok(s) => s,
        Err(_) => unreachable!(),
    }
};

/// A custom data directory override, set only by `set_custom_data_dir`.
/// This is used to override the default data directory location.
/// The directory will be created if it doesn't exist when set.
static CUSTOM_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved data directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On macOS, this is `~/Library/Application Support/WuIDE`.
/// On Linux/FreeBSD, this is `$XDG_DATA_HOME/wuide`.
/// On Windows, this is `%LOCALAPPDATA%\WuIDE`.
static CURRENT_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved config directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On macOS, this is `~/.config/wuide`.
/// On Linux/FreeBSD, this is `$XDG_CONFIG_HOME/wuide`.
/// On Windows, this is `%APPDATA%\WuIDE`.
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the relative path to the wu_server directory on the ssh host.
pub fn remote_server_dir_relative() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".wu_server").unwrap());
    *CACHED
}

// Remove this once 223 goes stable
/// Returns the relative path to the wu_wsl_server directory on the wsl host.
pub fn remote_wsl_server_dir_relative() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".wu_wsl_server").unwrap());
    *CACHED
}

/// Sets a custom directory for all user data, overriding the default data directory.
/// This function must be called before any other path operations that depend on the data directory.
/// The directory's path will be canonicalized to an absolute path by a blocking FS operation.
/// The directory will be created if it doesn't exist.
///
/// # Arguments
///
/// * `dir` - The path to use as the custom data directory. This will be used as the base
///   directory for all user data, including databases, extensions, and logs.
///
/// # Returns
///
/// A reference to the static `PathBuf` containing the custom data directory path.
///
/// # Errors
///
/// Returns an error if:
/// * Called after the data directory has been initialized (e.g., via `data_dir` or `config_dir`)
/// * The directory's path cannot be canonicalized to an absolute path
/// * The directory cannot be created
pub fn set_custom_data_dir(dir: &str) -> anyhow::Result<&'static PathBuf> {
    if CURRENT_DATA_DIR.get().is_some() || CONFIG_DIR.get().is_some() {
        anyhow::bail!("set_custom_data_dir called after data_dir or config_dir was initialized");
    }
    if let Some(existing) = CUSTOM_DATA_DIR.get() {
        return Ok(existing);
    }
    let path = PathBuf::from(dir);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create custom data directory {}", path.display()))?;
    let canonicalized = path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize custom data directory {} to an absolute path",
            path.display()
        )
    })?;
    // On Windows, `canonicalize` produces extended-length paths prefixed
    // with `\\?\`. Strip that prefix so downstream consumers (e.g.
    // Node.js language servers) that receive derived paths as arguments
    // don't choke on the verbatim syntax.
    let sanitized = SanitizedPath::new(&canonicalized).as_path().to_path_buf();
    Ok(CUSTOM_DATA_DIR.get_or_init(|| sanitized))
}

/// Returns the data directory given via `--user-data-dir`, if any.
pub fn custom_data_dir() -> Option<&'static PathBuf> {
    CUSTOM_DATA_DIR.get()
}

/// A stable hash of the `--user-data-dir` override, used to keep the
/// single-instance handshake separate per data directory. `None` when the
/// default data directory is in use. Shared by the app and the CLI so both
/// derive the same instance identity.
pub fn custom_data_dir_instance_hash() -> Option<u64> {
    // FNV-1a: deterministic across builds, unlike `DefaultHasher`, so an
    // older running instance and a newer launcher agree on the identity.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in custom_data_dir()?.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash)
}

/// Returns the path to the configuration directory used by WuIDE.
pub fn config_dir() -> &'static PathBuf {
    CONFIG_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.join("config")
        } else if cfg!(target_os = "windows") {
            dirs::config_dir()
                .expect("failed to determine RoamingAppData directory")
                .join(APP_NAME)
        } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            if let Ok(flatpak_xdg_config) = std::env::var("FLATPAK_XDG_CONFIG_HOME") {
                flatpak_xdg_config.into()
            } else {
                dirs::config_dir().expect("failed to determine XDG_CONFIG_HOME directory")
            }
            .join(APP_NAME_LOWERCASE)
        } else {
            home_dir().join(".config").join(APP_NAME_LOWERCASE)
        }
    })
}

/// Returns the path to the data directory used by WuIDE.
///
/// On macOS, this is `~/Library/Application Support/WuIDE`.
/// On Linux/FreeBSD, this is `$XDG_DATA_HOME/wuide`.
/// On Windows, this is `%LOCALAPPDATA%\WuIDE`.
pub fn data_dir() -> &'static PathBuf {
    CURRENT_DATA_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.clone()
        } else {
            default_data_dir()
        }
    })
}

/// The platform data directory used when `--user-data-dir` was not given.
///
/// Kept separate from [`data_dir`] so the default can be inspected without
/// fixing the process-wide choice made by the first call.
fn default_data_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir()
            .join("Library/Application Support")
            .join(APP_NAME)
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        if let Ok(flatpak_xdg_data) = std::env::var("FLATPAK_XDG_DATA_HOME") {
            flatpak_xdg_data.into()
        } else {
            dirs::data_local_dir().expect("failed to determine XDG_DATA_HOME directory")
        }
        .join(APP_NAME_LOWERCASE)
    } else if cfg!(target_os = "windows") {
        dirs::data_local_dir()
            .expect("failed to determine LocalAppData directory")
            .join(APP_NAME)
    } else {
        config_dir().clone() // Fallback
    }
}

/// Directory holding each agent runtime's isolated home.
///
/// This is the one place that decides where agents keep their state, and every
/// agent-facing path below hangs off it. `agent_harness` passes this directory
/// to the agents themselves, so it must describe the same location the editor
/// writes to, including under `--user-data-dir`.
///
/// `WU_AGENT_DATA_DIR` overrides it for users who keep agent state elsewhere.
pub fn agents_data_dir() -> PathBuf {
    agents_data_dir_for(data_dir())
}

fn agents_data_dir_for(data_dir: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("WU_AGENT_DATA_DIR") {
        return PathBuf::from(dir);
    }
    data_dir.join("agents")
}

/// The isolated Pi agent directory, which Wu passes to Pi as
/// `PI_CODING_AGENT_DIR`.
///
/// Keep in step with `prepare_pi_home` in `agent_harness/src/acp.rs`: Pi reads
/// its settings, `AGENTS.md` and `APPEND_SYSTEM.md` from here.
pub fn pi_agent_dir() -> PathBuf {
    agents_data_dir().join("pi-home").join(".pi").join("agent")
}

/// Where the IDE drops repository map snapshots for Pi's MCP server.
///
/// The repo map panel writes one file per project root; the bundled
/// `agents/mcp/wu-repo-map/server.js` reads it through `PI_CODING_AGENT_DIR`.
pub fn repo_map_snapshots_dir() -> PathBuf {
    pi_agent_dir().join("repo-maps")
}

/// Where the repo map keeps its per-file parse cache.
///
/// Beside the snapshots but in its own directory, because the snapshots are the
/// agent-facing document and the cache is the IDE's own bookkeeping: the
/// snapshot layout must not gain fields the MCP server does not know.
pub fn repo_map_cache_dir() -> PathBuf {
    repo_map_snapshots_dir().join(".cache")
}

/// Where the IDE drops LSP diagnostics snapshots for Pi's MCP server.
///
/// Same shape as [`repo_map_snapshots_dir`]: one `<project-key>.json` per
/// project root, read by the bundled MCP server through `PI_CODING_AGENT_DIR`.
pub fn diagnostics_snapshots_dir() -> PathBuf {
    pi_agent_dir().join("diagnostics")
}

/// The stable file name Wu gives one project root's snapshot.
///
/// The MCP server only sees its working directory, so both sides normalize the
/// same way: forward slashes, no trailing slash, lowercased on Windows, then the
/// first 16 hex digits of the SHA-256 digest. `agents/mcp/wu-repo-map/server.js`
/// repeats this rule and must stay in step.
pub fn project_key(project_root: &Path) -> String {
    const KEY_BYTES: usize = 8;

    let mut normalized = project_root.to_string_lossy().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if cfg!(windows) {
        normalized = normalized.to_lowercase();
    }
    let digest = sha2::Sha256::digest(normalized.as_bytes());
    digest[..KEY_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn state_dir() -> &'static PathBuf {
    static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
    STATE_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            return home_dir().join(".local").join("state").join(APP_NAME);
        }

        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            return if let Ok(flatpak_xdg_state) = std::env::var("FLATPAK_XDG_STATE_HOME") {
                flatpak_xdg_state.into()
            } else {
                dirs::state_dir().expect("failed to determine XDG_STATE_HOME directory")
            }
            .join(APP_NAME_LOWERCASE);
        } else {
            // Windows
            return dirs::data_local_dir()
                .expect("failed to determine LocalAppData directory")
                .join(APP_NAME);
        }
    })
}

/// Returns the path to the temp directory used by WuIDE.
pub fn temp_dir() -> &'static PathBuf {
    static TEMP_DIR: OnceLock<PathBuf> = OnceLock::new();
    TEMP_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            return dirs::cache_dir()
                .expect("failed to determine cachesDirectory directory")
                .join(APP_NAME);
        }

        if cfg!(target_os = "windows") {
            return dirs::cache_dir()
                .expect("failed to determine LocalAppData directory")
                .join(APP_NAME);
        }

        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            return if let Ok(flatpak_xdg_cache) = std::env::var("FLATPAK_XDG_CACHE_HOME") {
                flatpak_xdg_cache.into()
            } else {
                dirs::cache_dir().expect("failed to determine XDG_CACHE_HOME directory")
            }
            .join(APP_NAME_LOWERCASE);
        }

        home_dir().join(".cache").join(APP_NAME_LOWERCASE)
    })
}

/// Returns the path to the logs directory.
pub fn logs_dir() -> &'static PathBuf {
    static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    LOGS_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            home_dir().join("Library/Logs").join(APP_NAME)
        } else {
            data_dir().join("logs")
        }
    })
}

/// Returns the path to the WuIDE server directory on this SSH host.
pub fn remote_server_state_dir() -> &'static PathBuf {
    static REMOTE_SERVER_STATE: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_SERVER_STATE.get_or_init(|| data_dir().join("server_state"))
}

/// Returns the path to the `WuIDE.log` file.
pub fn log_file() -> &'static PathBuf {
    static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    LOG_FILE.get_or_init(|| logs_dir().join(format!("{}.log", APP_NAME)))
}

/// Returns the path to the `WuIDE.log.old` file.
pub fn old_log_file() -> &'static PathBuf {
    static OLD_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    OLD_LOG_FILE.get_or_init(|| logs_dir().join(format!("{}.log.old", APP_NAME)))
}

/// Returns the path to the database directory.
pub fn database_dir() -> &'static PathBuf {
    static DATABASE_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATABASE_DIR.get_or_init(|| data_dir().join("db"))
}

/// Directory names this fork used before settling on [`APP_NAME`], newest first.
///
/// Renaming `APP_NAME` starts every profile from scratch, which silently drops
/// settings, sessions and the agent data directories. `migrate_legacy_profile`
/// copies an old profile into place the first time the new name is used.
const LEGACY_DATA_DIR_NAMES: &[&str] = &["IDE", "ide", "Wu", "wu"];

/// Copy a profile written under an older [`APP_NAME`] into the current data
/// directory, if the current one is still empty.
///
/// Returns the directory that was migrated from. Only the data directory is
/// moved: settings live in the config directory, which is migrated separately
/// by [`migrate_legacy_config_dir`].
pub fn migrate_legacy_data_dir() -> Option<PathBuf> {
    let current = data_dir();
    if custom_data_dir().is_some() || directory_has_entries(current) {
        return None;
    }
    let parent = current.parent()?;
    for name in LEGACY_DATA_DIR_NAMES {
        let candidate = parent.join(name);
        if candidate != *current && directory_has_entries(&candidate) {
            return copy_dir_contents(&candidate, current)
                .ok()
                .map(|()| candidate);
        }
    }
    None
}

/// Copy settings written under an older [`APP_NAME`] into the current config
/// directory, if the current one is still empty.
pub fn migrate_legacy_config_dir() -> Option<PathBuf> {
    let current = config_dir();
    if custom_data_dir().is_some() || directory_has_entries(current) {
        return None;
    }
    let parent = current.parent()?;
    for name in LEGACY_DATA_DIR_NAMES {
        let candidate = parent.join(name);
        if candidate != *current && directory_has_entries(&candidate) {
            return copy_dir_contents(&candidate, current)
                .ok()
                .map(|()| candidate);
        }
    }
    None
}

fn directory_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Recursively copy `from` into `to`, leaving the source in place. The old
/// profile is kept rather than moved so that a downgrade still finds its data.
fn copy_dir_contents(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_contents(&source, &target)?;
        } else if !target.exists() {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// Returns the path to the crashes directory, if it exists for the current platform.
pub fn crashes_dir() -> &'static Option<PathBuf> {
    static CRASHES_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    CRASHES_DIR.get_or_init(|| {
        cfg!(target_os = "macos").then_some(home_dir().join("Library/Logs/DiagnosticReports"))
    })
}

/// Returns the path to the retired crashes directory, if it exists for the current platform.
pub fn crashes_retired_dir() -> &'static Option<PathBuf> {
    static CRASHES_RETIRED_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    CRASHES_RETIRED_DIR.get_or_init(|| crashes_dir().as_ref().map(|dir| dir.join("Retired")))
}

/// Returns the path to the `settings.json` file.
pub fn settings_file() -> &'static PathBuf {
    static SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    SETTINGS_FILE.get_or_init(|| config_dir().join("settings.json"))
}

/// Returns the path to the global settings file.
pub fn global_settings_file() -> &'static PathBuf {
    static GLOBAL_SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    GLOBAL_SETTINGS_FILE.get_or_init(|| config_dir().join("global_settings.json"))
}

/// Returns the path to the `settings_backup.json` file.
pub fn settings_backup_file() -> &'static PathBuf {
    static SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    SETTINGS_FILE.get_or_init(|| config_dir().join("settings_backup.json"))
}

/// Returns the path to the `keymap.json` file.
pub fn keymap_file() -> &'static PathBuf {
    static KEYMAP_FILE: OnceLock<PathBuf> = OnceLock::new();
    KEYMAP_FILE.get_or_init(|| config_dir().join("keymap.json"))
}

/// Returns the path to the `keymap_backup.json` file.
pub fn keymap_backup_file() -> &'static PathBuf {
    static KEYMAP_FILE: OnceLock<PathBuf> = OnceLock::new();
    KEYMAP_FILE.get_or_init(|| config_dir().join("keymap_backup.json"))
}

/// Returns the path to the `tasks.json` file.
pub fn tasks_file() -> &'static PathBuf {
    static TASKS_FILE: OnceLock<PathBuf> = OnceLock::new();
    TASKS_FILE.get_or_init(|| config_dir().join("tasks.json"))
}

/// Returns the path to the `debug.json` file.
pub fn debug_scenarios_file() -> &'static PathBuf {
    static DEBUG_SCENARIOS_FILE: OnceLock<PathBuf> = OnceLock::new();
    DEBUG_SCENARIOS_FILE.get_or_init(|| config_dir().join("debug.json"))
}

/// Returns the path to the user-global `AGENTS.md` file.
///
/// This file holds personal agent instructions that apply to every project the
/// user opens, and is loaded into the native Wu agent's system prompt.
pub fn agents_file() -> &'static PathBuf {
    static AGENTS_FILE: OnceLock<PathBuf> = OnceLock::new();
    AGENTS_FILE.get_or_init(|| config_dir().join("AGENTS.md"))
}

/// User-facing display form of the user-global `AGENTS.md` file path —
/// i.e. what a human should see in messages and prompts, with the
/// platform's native path separator and home/config directory shorthand.
///
/// Windows doesn't recognize `~` as the home directory, so the env-var
/// form (`%APPDATA%`) is used there instead. Note that this is the
/// *typical* location: a user with `XDG_CONFIG_HOME` set or running in a
/// Flatpak sandbox would see a different `agents_file()` at runtime than
/// this displays. The display string trades that precision for
/// readability in announcement copy.
#[cfg(target_os = "windows")]
pub const GLOBAL_AGENTS_FILE_DISPLAY: &str =
    const_format::concatcp!("%APPDATA%\\", APP_NAME, "\\AGENTS.md");
#[cfg(not(target_os = "windows"))]
pub const GLOBAL_AGENTS_FILE_DISPLAY: &str =
    const_format::concatcp!("~/.config/", APP_NAME_LOWERCASE, "/AGENTS.md");

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored.
pub fn extensions_dir() -> &'static PathBuf {
    static EXTENSIONS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTENSIONS_DIR.get_or_init(|| data_dir().join("extensions"))
}

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored on a remote.
pub fn remote_extensions_dir() -> &'static PathBuf {
    static EXTENSIONS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTENSIONS_DIR.get_or_init(|| data_dir().join("remote_extensions"))
}

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored on a remote.
pub fn remote_extensions_uploads_dir() -> &'static PathBuf {
    static UPLOAD_DIR: OnceLock<PathBuf> = OnceLock::new();
    UPLOAD_DIR.get_or_init(|| remote_extensions_dir().join("uploads"))
}

/// Returns the path to the themes directory.
///
/// This is where themes that are not provided by extensions are stored.
pub fn themes_dir() -> &'static PathBuf {
    static THEMES_DIR: OnceLock<PathBuf> = OnceLock::new();
    THEMES_DIR.get_or_init(|| config_dir().join("themes"))
}

/// Returns the path to the snippets directory.
pub fn snippets_dir() -> &'static PathBuf {
    static SNIPPETS_DIR: OnceLock<PathBuf> = OnceLock::new();
    SNIPPETS_DIR.get_or_init(|| config_dir().join("snippets"))
}

/// Returns the path to the contexts directory.
///
/// This is where the prompts for use with the Assistant are stored.
pub fn prompts_dir() -> &'static PathBuf {
    static PROMPTS_DIR: OnceLock<PathBuf> = OnceLock::new();
    PROMPTS_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            config_dir().join("prompts")
        } else {
            data_dir().join("prompts")
        }
    })
}

/// Returns the path to the prompt templates directory.
///
/// This is where the prompt templates for core features can be overridden with templates.
///
/// # Arguments
///
/// * `dev_mode` - If true, assumes the current working directory is the Zed repository.
pub fn prompt_overrides_dir(repo_path: Option<&Path>) -> PathBuf {
    if let Some(path) = repo_path {
        let dev_path = path.join("assets").join("prompts");
        if dev_path.exists() {
            return dev_path;
        }
    }

    static PROMPT_TEMPLATES_DIR: OnceLock<PathBuf> = OnceLock::new();
    PROMPT_TEMPLATES_DIR
        .get_or_init(|| {
            if cfg!(target_os = "macos") {
                config_dir().join("prompt_overrides")
            } else {
                data_dir().join("prompt_overrides")
            }
        })
        .clone()
}

/// Returns the path to the semantic search's embeddings directory.
///
/// This is where the embeddings used to power semantic search are stored.
pub fn embeddings_dir() -> &'static PathBuf {
    static EMBEDDINGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EMBEDDINGS_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            config_dir().join("embeddings")
        } else {
            data_dir().join("embeddings")
        }
    })
}

/// Returns the path to the languages directory.
///
/// This is where language servers are downloaded to for languages built-in to Wu.
pub fn languages_dir() -> &'static PathBuf {
    static LANGUAGES_DIR: OnceLock<PathBuf> = OnceLock::new();
    LANGUAGES_DIR.get_or_init(|| data_dir().join("languages"))
}

/// Returns the path to the debug adapters directory
///
/// This is where debug adapters are downloaded to for DAPs that are built-in to Wu.
pub fn debug_adapters_dir() -> &'static PathBuf {
    static DEBUG_ADAPTERS_DIR: OnceLock<PathBuf> = OnceLock::new();
    DEBUG_ADAPTERS_DIR.get_or_init(|| data_dir().join("debug_adapters"))
}

/// Returns the path to the external agents directory
///
/// This is where agent servers are downloaded to
pub fn external_agents_dir() -> &'static PathBuf {
    static EXTERNAL_AGENTS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTERNAL_AGENTS_DIR.get_or_init(|| data_dir().join("external_agents"))
}

/// Returns the path to the Copilot directory.
pub fn copilot_dir() -> &'static PathBuf {
    static COPILOT_DIR: OnceLock<PathBuf> = OnceLock::new();
    COPILOT_DIR.get_or_init(|| data_dir().join("copilot"))
}

/// Returns the path to the default Prettier directory.
pub fn default_prettier_dir() -> &'static PathBuf {
    static DEFAULT_PRETTIER_DIR: OnceLock<PathBuf> = OnceLock::new();
    DEFAULT_PRETTIER_DIR.get_or_init(|| data_dir().join("prettier"))
}

/// Returns the path to the remote server binaries directory.
pub fn remote_servers_dir() -> &'static PathBuf {
    static REMOTE_SERVERS_DIR: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_SERVERS_DIR.get_or_init(|| data_dir().join("remote_servers"))
}

/// Returns the relative path to a `.wu` folder within a project.
pub fn local_settings_folder_name() -> &'static str {
    ".wu"
}

/// Returns the relative path to a `.zed` folder within a project, used as a
/// fallback when there is no `.wu` folder.
pub fn legacy_local_settings_folder_name() -> &'static str {
    ".zed"
}

/// Returns the relative path to a `.vscode` folder within a project.
pub fn local_vscode_folder_name() -> &'static str {
    ".vscode"
}

/// Picks the `.wu` config file over its `.zed` counterpart. The `.zed` file is
/// only used when it exists and the `.wu` one does not.
pub fn resolve_local_config_path<P: AsRef<RelPath>>(
    path: P,
    legacy_path: P,
    exists: impl Fn(&RelPath) -> bool,
) -> P {
    if !exists(path.as_ref()) && exists(legacy_path.as_ref()) {
        legacy_path
    } else {
        path
    }
}

/// Returns the relative path to a `settings.json` file within a project.
pub fn local_settings_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".wu/settings.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.zed/settings.json` file within a project, used as a
/// fallback when there is no `.wu/settings.json`.
pub fn legacy_local_settings_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".zed/settings.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `tasks.json` file within a project.
pub fn local_tasks_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".wu/tasks.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.zed/tasks.json` file within a project, used as a
/// fallback when there is no `.wu/tasks.json`.
pub fn legacy_local_tasks_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".zed/tasks.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.vscode/tasks.json` file within a project.
pub fn local_vscode_tasks_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".vscode/tasks.json").unwrap());
    *CACHED
}

pub fn debug_task_file_name() -> &'static str {
    "debug.json"
}

pub fn task_file_name() -> &'static str {
    "tasks.json"
}

/// Returns the relative path to a `debug.json` file within a project.
/// .wu/debug.json
pub fn local_debug_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".wu/debug.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.zed/debug.json` file within a project, used as a
/// fallback when there is no `.wu/debug.json`.
pub fn legacy_local_debug_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".zed/debug.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.vscode/launch.json` file within a project.
pub fn local_vscode_launch_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".vscode/launch.json").unwrap());
    *CACHED
}

pub fn user_ssh_config_file() -> PathBuf {
    home_dir().join(".ssh/config")
}

pub fn global_ssh_config_file() -> Option<&'static Path> {
    if cfg!(windows) {
        None
    } else {
        Some(Path::new("/etc/ssh/ssh_config"))
    }
}

/// Returns candidate paths for the vscode user settings file
pub fn vscode_settings_file_paths() -> Vec<PathBuf> {
    let mut paths = vscode_user_data_paths();
    for path in paths.iter_mut() {
        path.push("User/settings.json");
    }
    paths
}

/// Returns candidate paths for the cursor user settings file
pub fn cursor_settings_file_paths() -> Vec<PathBuf> {
    let mut paths = cursor_user_data_paths();
    for path in paths.iter_mut() {
        path.push("User/settings.json");
    }
    paths
}

fn vscode_user_data_paths() -> Vec<PathBuf> {
    // https://github.com/microsoft/vscode/blob/23e7148cdb6d8a27f0109ff77e5b1e019f8da051/src/vs/platform/environment/node/userDataPath.ts#L45
    const VSCODE_PRODUCT_NAMES: &[&str] = &[
        "Code",
        "Code - Insiders",
        "Code - OSS",
        "VSCodium",
        "VSCodium - Insiders",
        "Code Dev",
        "Code - OSS Dev",
        "code-oss-dev",
    ];
    let mut paths = Vec::new();
    if let Ok(portable_path) = env::var("VSCODE_PORTABLE") {
        paths.push(Path::new(&portable_path).join("user-data"));
    }
    if let Ok(vscode_appdata) = env::var("VSCODE_APPDATA") {
        for product_name in VSCODE_PRODUCT_NAMES {
            paths.push(Path::new(&vscode_appdata).join(product_name));
        }
    }
    for product_name in VSCODE_PRODUCT_NAMES {
        add_vscode_user_data_paths(&mut paths, product_name);
    }
    paths
}

fn cursor_user_data_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    add_vscode_user_data_paths(&mut paths, "Cursor");
    paths
}

fn add_vscode_user_data_paths(paths: &mut Vec<PathBuf>, product_name: &str) {
    if cfg!(target_os = "macos") {
        paths.push(
            home_dir()
                .join("Library/Application Support")
                .join(product_name),
        );
    } else if cfg!(target_os = "windows") {
        if let Some(data_local_dir) = dirs::data_local_dir() {
            paths.push(data_local_dir.join(product_name));
        }
        if let Some(data_dir) = dirs::data_dir() {
            paths.push(data_dir.join(product_name));
        }
    } else {
        paths.push(
            dirs::config_dir()
                .unwrap_or(home_dir().join(".config"))
                .join(product_name),
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn global_gitignore_path() -> Option<PathBuf> {
    Some(home_dir().join(".config").join("git").join("ignore"))
}

#[cfg(not(any(test, feature = "test-support")))]
pub fn global_gitignore_path() -> Option<PathBuf> {
    static GLOBAL_GITIGNORE_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    GLOBAL_GITIGNORE_PATH
        .get_or_init(::ignore::gitignore::gitconfig_excludes_path)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_is_hex_and_stable_across_separators() {
        let key = project_key(Path::new("D:/work/project"));
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));

        assert_eq!(key, project_key(Path::new("D:\\work\\project")));
        assert_eq!(key, project_key(Path::new("D:/work/project/")));
    }

    #[cfg(windows)]
    #[test]
    fn project_key_ignores_case_on_windows() {
        assert_eq!(
            project_key(Path::new("D:/Work/Project")),
            project_key(Path::new("d:/work/project"))
        );
    }

    #[test]
    fn project_key_differs_between_roots() {
        assert_ne!(
            project_key(Path::new("/work/one")),
            project_key(Path::new("/work/two"))
        );
    }

    #[test]
    fn copy_dir_contents_copies_nested_files() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let nested = source.path().join("db");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("settings.json"), "{}").unwrap();
        std::fs::write(source.path().join("top.txt"), "hello").unwrap();

        copy_dir_contents(source.path(), target.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.path().join("top.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(target.path().join("db").join("settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn copy_dir_contents_leaves_existing_files_alone() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("keep.txt"), "new").unwrap();
        std::fs::write(target.path().join("keep.txt"), "old").unwrap();

        copy_dir_contents(source.path(), target.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(target.path().join("keep.txt")).unwrap(),
            "old",
            "an existing file in the new profile must win over the old one"
        );
    }

    #[test]
    fn directory_has_entries_ignores_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!directory_has_entries(dir.path()));
        std::fs::write(dir.path().join("file"), "").unwrap();
        assert!(directory_has_entries(dir.path()));
        assert!(!directory_has_entries(&dir.path().join("missing")));
    }

    #[test]
    fn a_user_data_dir_override_moves_every_agent_path_with_it() {
        // The override has to be installed before anything asks for `data_dir`,
        // which caches its answer for the whole process, so no other test in
        // this binary may call it.
        let profile = tempfile::tempdir().unwrap();
        let custom_dir = profile.path().join("profile");
        let custom = set_custom_data_dir(custom_dir.to_str().unwrap()).expect("custom data dir");
        let agent_root = agents_data_dir();

        assert_eq!(agent_root, custom.join("agents"));
        assert_eq!(
            pi_agent_dir(),
            agent_root.join("pi-home").join(".pi").join("agent")
        );
        assert!(
            repo_map_snapshots_dir().starts_with(&agent_root),
            "repo map snapshots must land under the agent root Pi is pointed at"
        );
        assert!(
            diagnostics_snapshots_dir().starts_with(&agent_root),
            "diagnostics snapshots must land under the agent root Pi is pointed at"
        );
    }

    #[test]
    fn without_an_override_the_agent_root_sits_under_local_app_data() {
        // `data_dir` caches the override process-wide, so check the default it
        // falls back to instead of calling `data_dir` in a second test.
        let expected = default_data_dir().join("agents");
        assert_eq!(agents_data_dir_for(&default_data_dir()), expected);
        #[cfg(windows)]
        assert_eq!(
            expected,
            PathBuf::from(
                std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA is set on Windows")
            )
            .join(APP_NAME)
            .join("agents")
        );
    }
}
