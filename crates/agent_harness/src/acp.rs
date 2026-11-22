//! A generic [Agent Client Protocol](https://agentclientprotocol.com) harness.
//!
//! Wu ships no ACP support of its own, but a handful of agents already speak
//! ACP over newline-delimited JSON-RPC on stdio (`dsh-acp` for DeepSeek,
//! `pi-acp` for Pi). This module drives any such agent and translates its
//! `session/update` notifications into Wu's [`AgentEvent`] stream.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::jsonrpc::{Incoming, RpcClient};
use crate::process::{Child, Command, Stdio};
use crate::proto::*;
use crate::{Harness, HarnessError, RunRequest};

const ACP_PROTOCOL_VERSION: i64 = 1;

// The isolated Pi home inherits the user's global `settings.json`, whose
// `defaultProvider` (a cc-switch alias) does not exist inside the isolated
// model store. Without a valid provider Pi falls back to a model with no
// credentials and answers every prompt with an empty message. Seed a provider
// that actually resolves — the local model gateway this build talks to — and
// point the default at it unless the configured provider is one we can reach.
const LOCAL_PROVIDER_ID: &str = "magpie";
const LOCAL_PROVIDER_BASE_URL: &str = "http://127.0.0.1:3425/v1";
const LOCAL_PROVIDER_API_KEY: &str = "magpie";
const PI_DEFAULT_MODEL: &str = "auto/deepseek-v4.1-flash";

// Pi exposes its thinking effort as `off|minimal|low|medium|high`. Wu's slider
// cannot express `off`, so leaving it out keeps every position an explicit
// effort the agent understands.
const PI_REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
];

// ACP config option ids that pi-acp understands for `session/set_config_option`.
const PI_MODEL_CONFIG_ID: &str = "model";
const PI_THINKING_CONFIG_ID: &str = "thought_level";

// Bump when the addendum text changes, so a file written by an older build is
// recognizable. Pi reads it from the agent directory on every start.
const PI_ADDENDUM_VERSION: u32 = 3;

#[derive(Clone)]
struct AcpPreset {
    id: HarnessId,
    display_name: &'static str,
    /// Executable searched in the bundled `agents` directories and on `PATH`.
    program: &'static str,
    /// Extra arguments that always precede the user's own (e.g. a JS entrypoint).
    args: &'static [&'static str],
    /// Optional JS entrypoint (relative to the agent directory) for agents that
    /// are launched through a bundled Node runtime instead of a native binary.
    js_entry: Option<&'static str>,
    /// Optional flag for an isolated per-agent data directory (plugins, config).
    data_dir_flag: Option<&'static str>,
    fallback_models: &'static [(&'static str, &'static str)],
}

pub struct AcpHarness {
    preset: Arc<AcpPreset>,
    program_override: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

#[derive(Clone)]
struct Launch {
    program: PathBuf,
    args: Vec<String>,
    /// Directory the bundled agent runtime lives in; it owns `node_modules`,
    /// the agent's own data directory and anything else shipped beside them.
    agent_dir: PathBuf,
    /// Directories prepended to PATH for the child: the bundled runtime's own
    /// directory (so npm shims find our `node`) and `node_modules/.bin`.
    path_prepend: Vec<PathBuf>,
}

impl AcpHarness {
    fn new(preset: AcpPreset) -> Self {
        Self {
            preset: Arc::new(preset),
            program_override: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }

    pub fn deepseek() -> Self {
        Self::new(AcpPreset {
            id: HarnessId::Dsh,
            display_name: "DeepSeek",
            program: "dsh-acp",
            args: &[],
            js_entry: None,
            data_dir_flag: Some("--data-dir"),
            fallback_models: &[("deepseek-flash", "DeepSeek Flash")],
        })
    }

    pub fn pi() -> Self {
        Self::new(AcpPreset {
            id: HarnessId::Pi,
            display_name: "Pi",
            program: "node",
            args: &[],
            js_entry: Some("node_modules/pi-acp/dist/index.js"),
            data_dir_flag: None,
            fallback_models: &[("pi", "Pi")],
        })
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.program_override = Some(path.into());
        self
    }

    /// `project_roots` are the folders the session works in. Search them after
    /// every location a release ships a runtime in, so an executable that was
    /// copied out of its bundle still finds the checkout the project lives in.
    fn resolve_launch(&self, project_roots: &[PathBuf]) -> Result<Launch, HarnessError> {
        if let Some(path) = &self.program_override {
            return self.build_launch(path.clone(), &bundled_dirs(project_roots));
        }
        if let Some(launch) = remembered_launch(self.preset.id) {
            return Ok(launch);
        }
        let dirs = bundled_dirs(project_roots);
        let found = find_program(self.preset.program, &dirs).ok_or_else(|| {
            self.unresolved_agent_error(&bundled_program_file(self.preset.program), &dirs)
        })?;
        let launch = self.build_launch(found, &dirs)?;
        remember_launch(self.preset.id, &launch);
        Ok(launch)
    }

    /// A bundled runtime that lacks one of its files is a partial install or a
    /// checkout that never ran `npm install`, so name the missing file. Only a
    /// machine with no agent directory at all gets the plain "not available",
    /// which now lists the folders that were tried so the user can tell where a
    /// runtime belongs.
    fn unresolved_agent_error(&self, missing: &str, dirs: &[PathBuf]) -> HarnessError {
        match dirs.iter().find(|dir| dir.join("node_modules").is_dir()) {
            Some(dir) => HarnessError::AgentIncomplete(format!(
                "{}: {missing} is missing from {}",
                self.preset.display_name,
                dir.display()
            )),
            None => HarnessError::agent_missing(self.preset.id, &searched(missing, dirs)),
        }
    }

    fn build_launch(&self, program: PathBuf, dirs: &[PathBuf]) -> Result<Launch, HarnessError> {
        let mut args: Vec<String> = Vec::new();
        let mut agent_dir = program
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if let Some(entry) = self.preset.js_entry {
            agent_dir = self.agent_dir_for_entry(&program, entry, dirs)?;
            args.push(agent_dir.join(entry).to_string_lossy().into_owned());
        }
        args.extend(self.preset.args.iter().map(|a| (*a).to_owned()));
        self.push_data_dir(&mut args);
        let bin = agent_dir.join("node_modules").join(".bin");
        // npm's POSIX shims exec `node` from PATH, so the bundled node next to
        // them has to come first for the runtime to be used even on machines
        // without a system Node install.
        let mut path_prepend = vec![agent_dir.clone()];
        if bin.is_dir() {
            path_prepend.push(bin);
        }
        Ok(Launch {
            program,
            args,
            agent_dir,
            path_prepend,
        })
    }

    /// The directory that holds the JS entrypoint, which is the one with
    /// `node_modules` in it. A bundled `node` lives right next to those, but a
    /// Node runtime picked up from `PATH` (distro package, Homebrew) does not,
    /// so the bundled agent directories are searched as a fallback.
    fn agent_dir_for_entry(
        &self,
        program: &Path,
        entry: &str,
        dirs: &[PathBuf],
    ) -> Result<PathBuf, HarnessError> {
        let parent = program.parent().unwrap_or_else(|| Path::new("."));
        if parent.join(entry).is_file() {
            return Ok(parent.to_path_buf());
        }
        dirs.iter()
            .find(|dir| dir.join(entry).is_file())
            .cloned()
            .ok_or_else(|| self.unresolved_agent_error(entry, dirs))
    }

    /// Point the agent at an isolated data directory inside Wu's own folder so
    /// its plugins, credentials and session state never touch the machine's
    /// globally installed agent.
    fn push_data_dir(&self, args: &mut Vec<String>) {
        let Some(flag) = self.preset.data_dir_flag else {
            return;
        };
        let slug = match self.preset.id {
            HarnessId::Dsh => "deepseek",
            HarnessId::Pi => "pi",
            _ => "agent",
        };
        let dir = paths::agents_data_dir().join(slug);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            log::warn!("failed to create ACP agent data dir {dir:?}: {error}");
        }
        args.push(flag.to_owned());
        args.push(dir.to_string_lossy().into_owned());
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        self.preset.id
    }

    fn display_name(&self) -> &str {
        self.preset.display_name
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        if self.preset.id == HarnessId::Pi {
            PI_REASONING_LEVELS
        } else {
            &[]
        }
    }

    fn installed(&self) -> bool {
        self.resolve_launch(&[]).is_ok()
    }

    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_launch(&[]).ok().map(|launch| launch.program)
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.models_in(Path::new("")).await
    }

    async fn models_in(&self, cwd: &Path) -> Result<Vec<Model>, HarnessError> {
        if self.preset.id == HarnessId::Pi {
            if let Some(models) = self.pi_models(&session_roots(cwd)) {
                return Ok(models);
            }
        }
        Ok(self.fallback_models())
    }

    /// Pi's catalogue comes from `models.json`, which cc-switch rewrites while
    /// Wu is running, so the picker has to re-read it rather than cache forever.
    fn models_follow_config_file(&self) -> bool {
        self.preset.id == HarnessId::Pi
    }

    fn fallback_models(&self) -> Vec<Model> {
        self.preset
            .fallback_models
            .iter()
            .map(|(id, label)| Model {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
                description: None,
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            })
            .collect()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: crate::RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let launch = self.resolve_launch(&session_roots(Path::new(&request.cwd)))?;
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        let harness_id = self.preset.id;
        let interrupt_grace = self.interrupt_grace;
        let kill_grace = self.kill_grace;
        tokio::spawn(async move {
            drive(
                launch,
                request,
                controls,
                event_tx,
                harness_id,
                interrupt_grace,
                kill_grace,
            )
            .await;
        });
        Ok(futures::stream::unfold(event_rx, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed())
    }
}

impl AcpHarness {
    /// Enumerate the models the bundled Pi agent can select, read from the same
    /// `models.json` cc-switch writes for Pi (mirrored into the isolated home by
    /// `prepare_pi_home`), so the picker follows whatever the user configures
    /// there. Providers without a credential are skipped — Pi cannot use them.
    fn pi_models(&self, project_roots: &[PathBuf]) -> Option<Vec<Model>> {
        let launch = self.resolve_launch(project_roots).ok()?;
        let (_home, agent_dir) = prepare_pi_home(&launch.program, &launch.agent_dir)?;
        let ladder = PI_REASONING_LEVELS.to_vec();
        let mut models = Vec::new();
        let mut seen = HashSet::new();

        let json = read_json(&agent_dir.join("models.json"))?;
        let providers = provider_entries(&json)?;

        // The local provider leads so it becomes Wu's default; the rest keep the
        // order cc-switch wrote them in.
        let mut names: Vec<&String> = providers.keys().collect();
        names.sort_by_key(|name| u8::from(name.as_str() != LOCAL_PROVIDER_ID));
        for name in names {
            let entry = &providers[name];
            let has_key = entry
                .get("apiKey")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty());
            if has_key {
                collect_models(&mut models, &mut seen, name, entry, &ladder);
            }
        }

        (!models.is_empty()).then_some(models)
    }
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// Both `models.json` and `models-store.json` nest providers under `providers`;
/// tolerate a flat `{ provider: {...} }` layout too.
fn provider_entries(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    if let Some(map) = value.get("providers").and_then(Value::as_object) {
        return Some(map);
    }
    let map = value.as_object()?;
    if !map.is_empty() && map.values().all(Value::is_object) {
        Some(map)
    } else {
        None
    }
}

fn collect_models(
    out: &mut Vec<Model>,
    seen: &mut HashSet<String>,
    provider: &str,
    entry: &Value,
    ladder: &[ReasoningLevel],
) {
    let Some(list) = entry.get("models").and_then(Value::as_array) else {
        return;
    };
    for model in list {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = model.get("name").and_then(Value::as_str).unwrap_or(id);
        push_pi_model(
            out,
            seen,
            &format!("{provider}/{id}"),
            &format!("{provider}/{name}"),
            ladder,
        );
    }
}

fn push_pi_model(
    out: &mut Vec<Model>,
    seen: &mut HashSet<String>,
    id: &str,
    label: &str,
    ladder: &[ReasoningLevel],
) {
    if id.is_empty() || !seen.insert(id.to_owned()) {
        return;
    }
    out.push(Model {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        reasoning_levels: ladder.to_vec(),
        options: Vec::new(),
    });
}

fn pi_thinking_value(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        _ => "high",
    }
}

/// Push the chosen model and thinking effort to Pi before a prompt. Selecting a
/// missing model or an unsupported effort is not fatal — the run continues on
/// the agent's current settings — so failures are logged, not surfaced.
async fn apply_pi_selection(
    client: &RpcClient,
    session_id: &str,
    model: Option<&str>,
    reasoning: Option<ReasoningLevel>,
) {
    // Legacy fallback ids (`pi`, `deepseek-flash`) are not real Pi models; leave
    // them to the agent's own default instead of sending an unknown id.
    if let Some(model) = model.filter(|model| model.contains('/')) {
        if let Err(error) = client
            .request(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": PI_MODEL_CONFIG_ID, "value": model}),
            )
            .await
        {
            log::warn!("failed to select Pi model {model}: {error}");
        }
    }
    if let Some(level) = reasoning {
        let value = pi_thinking_value(level);
        if let Err(error) = client
            .request(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": PI_THINKING_CONFIG_ID, "value": value}),
            )
            .await
        {
            log::warn!("failed to set Pi thinking level {value}: {error}");
        }
    }
}

type EventSender = mpsc::Sender<Result<AgentEvent, HarnessError>>;

async fn send(events: &EventSender, event: AgentEvent) -> bool {
    events.send(Ok(event)).await.is_ok()
}

async fn drive(
    launch: Launch,
    request: RunRequest,
    controls: crate::RunControls,
    events: EventSender,
    harness_id: HarnessId,
    interrupt_grace: Duration,
    kill_grace: Duration,
) {
    if let Err(error) = drive_inner(
        launch,
        request,
        controls,
        &events,
        harness_id,
        interrupt_grace,
        kill_grace,
    )
    .await
    {
        let message = error.to_string();
        send(&events, AgentEvent::Error { message: message.clone() }).await;
        send(
            &events,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some(message),
                session_id: None,
            },
        )
        .await;
    }
}

/// Build an isolated `$HOME` for the bundled Pi agent and seed it with the
/// user's existing model/credential files (but not their globally installed
/// extensions, which would need `npm` at runtime).
///
/// The model, credential and settings files are mirrored from the user's global
/// Pi config on every launch, so a profile switch in cc-switch (which writes
/// that config) reaches the bundled Pi without wiping the isolated home.
fn prepare_pi_home(program: &Path, agent_root: &Path) -> Option<(PathBuf, PathBuf)> {
    // The per-user directory that holds each agent's isolated plugins,
    // credentials and session state. `paths` owns the location, including the
    // `--user-data-dir` override, so the agents look where the editor writes.
    let base = paths::agents_data_dir();
    let home = base.join("pi-home");
    let agent_dir = home.join(".pi").join("agent");
    let acp_dir = home.join(".pi").join("pi-acp");
    if let Err(error) = std::fs::create_dir_all(&agent_dir)
        .and_then(|_| std::fs::create_dir_all(&acp_dir))
    {
        log::warn!("failed to prepare Pi home {home:?}: {error}");
        return None;
    }
    // This directory holds the user's API credentials and full transcripts of
    // every conversation, so keep it owner-only. The default umask leaves it at
    // 0755, which is world-readable.
    for dir in [&home, &home.join(".pi"), &agent_dir, &acp_dir] {
        if let Err(error) = restrict_to_owner(dir) {
            log::warn!("failed to restrict permissions on {dir:?}: {error}");
        }
    }
    write_pi_addendum(&agent_dir);
    write_pi_mcp_config(&agent_dir, agent_root, program);

    let settings_path = agent_dir.join("settings.json");
    if let Some(source_agent) = source_pi_agent_dir() {
        for name in ["auth.json", "models.json", "models-store.json"] {
            let from = source_agent.join(name);
            if from.is_file() {
                let to = agent_dir.join(name);
                if let Err(error) = std::fs::copy(&from, &to) {
                    log::warn!("failed to sync Pi {name:?}: {error}");
                } else if let Err(error) = restrict_to_owner(&to) {
                    log::warn!("failed to restrict permissions on {to:?}: {error}");
                }
            }
        }
        // Mirror the settings on every launch as well: cc-switch rewrites them
        // when the user switches profiles, and the bundled Pi has to follow.
        // Globally installed packages and `enabledModels` are dropped because
        // they need npm at runtime.
        if let Some(settings) = read_json(&source_agent.join("settings.json")) {
            if let Err(error) = write_json_file(&settings_path, &isolated_pi_settings(&settings)) {
                log::warn!("failed to mirror Pi settings: {error}");
            }
        } else if !settings_path.exists() {
            if let Err(error) = std::fs::write(&settings_path, "{}") {
                log::warn!("failed to write Pi settings: {error}");
            }
        }
    } else if !settings_path.exists() {
        if let Err(error) = std::fs::write(&settings_path, "{}") {
            log::warn!("failed to write Pi settings: {error}");
        }
    }

    if let Err(error) = ensure_local_pi_provider(&agent_dir, &settings_path) {
        log::warn!("failed to configure the Pi model provider: {error}");
    }

    // Every write above created the file with the ambient umask.
    for name in [
        "settings.json",
        "auth.json",
        "models.json",
        "models-store.json",
    ] {
        let path = agent_dir.join(name);
        if path.is_file()
            && let Err(error) = restrict_to_owner(&path)
        {
            log::warn!("failed to restrict permissions on {path:?}: {error}");
        }
    }

    Some((home, agent_dir))
}

/// Where Pi looks for the instructions Wu appends to its system prompt.
fn pi_addendum_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("APPEND_SYSTEM.md")
}

/// The instructions Wu appends to Pi's system prompt.
///
/// Pi reads `APPEND_SYSTEM.md` from its agent directory on every start and adds
/// it to its own prompt as an `<addendum>` block. The agent directory is shared
/// by every chat and every project, so this text stays free of project and
/// session state: Pi derives whatever it needs from its working directory.
fn pi_addendum(version: u32) -> String {
    format!(
        r#"<!-- wu:pi-addendum:v{version} -->
# Working inside Wu

You are running inside Wu (wuIDE), a fork of the Zed editor built on GPUI. Wu
has a built-in Pi agent and adds these instructions to your system prompt every
time it starts you.

- Your working directory is the project root. Treat it as the base for every
  relative path.
- Project conventions live in `AGENTS.md` at the project root. Read it before
  making structural or stylistic decisions about this codebase.
- Wu serves the project's structure as two tools: `repo_map` lists the files the
  rest of the repository refers to most, with their definitions, and `symbols`
  lists one file's definitions. Call `repo_map` when structural context would
  help - which files matter, where something lives - instead of guessing from
  directory listings or search results. If it reports that no snapshot exists
  yet, ask the user to open the Repository Map panel and try again; never invent
  the map.
- Wu also records the language-server diagnostics of the project and serves them
  as `diagnostics`. When you need to know about compile errors or warnings, call
  `diagnostics` first instead of guessing from the source or asking the user to
  paste errors. If it reports that no snapshot exists yet, ask the user to open
  the Problems panel so the IDE can write one, then call it again.
- This file is shared by every project and every chat, so it holds no project or
  session state. Derive anything project-specific from the working directory.
- Reply in the language the user writes in.
"#
    )
}

/// Writes Wu's addendum into the isolated Pi agent directory.
///
/// This runs before every Pi launch and overwrites the file, so a launch never
/// depends on leftovers. It never touches the user's own `AGENTS.md`, and a
/// failure only costs the addendum - Pi still starts.
fn write_pi_addendum(agent_dir: &Path) {
    let path = pi_addendum_path(agent_dir);
    if let Err(error) = std::fs::write(&path, pi_addendum(PI_ADDENDUM_VERSION)) {
        log::error!("failed to write Pi addendum {path:?}: {error}");
        return;
    }
    if let Err(error) = restrict_to_owner(&path) {
        log::error!("failed to restrict permissions on {path:?}: {error}");
    }
}

/// The name Wu registers its MCP server under, and the prefix Pi gives its
/// tools (`mcp__wu__repo_map`).
const PI_MCP_SERVER_NAME: &str = "wu";

fn pi_mcp_config_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("mcp.json")
}

/// The script Wu ships for Pi to call Wu's repository map.
fn pi_mcp_server_script(agent_root: &Path) -> PathBuf {
    agent_root
        .join("mcp")
        .join("wu-repo-map")
        .join("server.js")
}

/// Wu's own MCP entry.
///
/// `exposure` must be `direct`: with Pi's default (`codemode`) the tools are
/// only reachable through codemode scripts and are never declared to the model,
/// which makes them look broken.
fn pi_mcp_server_entry(agent_root: &Path, node: &Path, agent_dir: &Path) -> Value {
    json!({
        "command": node,
        "args": [pi_mcp_server_script(agent_root)],
        "env": { "PI_CODING_AGENT_DIR": agent_dir },
        "exposure": "direct",
        "description": "Wu's repository map and language-server diagnostics for the open project."
    })
}

/// Adds Wu's server to Pi's `mcp.json` and leaves every other entry alone.
///
/// The file belongs to Pi, not to Wu, so an unreadable one is never replaced:
/// dropping the user's servers to register ours would be a bad trade. Returns
/// the new file contents, or `None` when the existing file must be kept as is.
fn merge_mcp_config(existing: Option<&str>, entry: Value) -> Option<String> {
    let mut config = match existing.map(str::trim) {
        None | Some("") => json!({}),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                log::error!("Pi's mcp.json is not a JSON object; leaving it untouched");
                return None;
            }
            Err(error) => {
                log::error!("Pi's mcp.json is not valid JSON ({error}); leaving it untouched");
                return None;
            }
        },
    };

    let Some(object) = config.as_object_mut() else {
        return None;
    };
    let servers = object.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        log::error!("Pi's mcp.json has a non-object mcpServers; leaving it untouched");
        return None;
    };
    servers.insert(PI_MCP_SERVER_NAME.to_owned(), entry);

    serde_json::to_string_pretty(&config).ok()
}

/// Registers Wu's repository map with Pi through the agent directory's
/// `mcp.json`. Failing here costs the tool, not the run.
fn write_pi_mcp_config(agent_dir: &Path, agent_root: &Path, node: &Path) {
    let path = pi_mcp_config_path(agent_dir);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            log::error!("failed to read Pi MCP config {path:?}: {error}");
            return;
        }
    };

    let entry = pi_mcp_server_entry(agent_root, node, agent_dir);
    let Some(merged) = merge_mcp_config(existing.as_deref(), entry) else {
        return;
    };
    if existing.as_deref() == Some(merged.as_str()) {
        return;
    }
    if let Err(error) = std::fs::write(&path, merged) {
        log::error!("failed to write Pi MCP config {path:?}: {error}");
        return;
    }
    if let Err(error) = restrict_to_owner(&path) {
        log::error!("failed to restrict permissions on {path:?}: {error}");
    }
}

fn isolated_pi_settings(source: &Value) -> Value {
    let mut settings = source.clone();
    if let Some(object) = settings.as_object_mut() {
        object.remove("packages");
        object.remove("enabledModels");
    }
    settings
}

/// Keep a file or directory inside the Pi home readable only by its owner.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn source_pi_agent_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".pi").join("agent"))
}

/// Guarantee the isolated Pi home points at a provider it can actually use.
/// `models.json` is mirrored from cc-switch, so normally a reachable provider
/// is already present; only when none carries a credential do we inject the
/// built-in gateway as a last resort.
fn ensure_local_pi_provider(agent_dir: &Path, settings_path: &Path) -> std::io::Result<()> {
    let models_path = agent_dir.join("models.json");
    let mut models = std::fs::read_to_string(&models_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    if !models.is_object() {
        models = json!({});
    }
    if let Some(root) = models.as_object_mut() {
        if !root.get("providers").is_some_and(Value::is_object) {
            root.insert("providers".to_owned(), json!({}));
        }
    }

    // Pick the provider we will default to: the local gateway if present, else
    // any provider that carries a credential, together with its first model.
    let mut chosen: Option<(String, Option<String>)> = None;
    if let Some(providers) = models.get("providers").and_then(Value::as_object) {
        let mut names: Vec<&String> = providers.keys().collect();
        names.sort_by_key(|name| u8::from(name.as_str() != LOCAL_PROVIDER_ID));
        for name in names {
            let entry = &providers[name];
            let has_key = entry
                .get("apiKey")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty());
            if has_key {
                let first_model = entry
                    .get("models")
                    .and_then(Value::as_array)
                    .and_then(|list| list.first())
                    .and_then(|model| model.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                chosen = Some((name.clone(), first_model));
                break;
            }
        }
    }

    if chosen.is_none() {
        if let Some(providers) = models.get_mut("providers").and_then(Value::as_object_mut) {
            providers.insert(LOCAL_PROVIDER_ID.to_owned(), local_provider_entry());
        }
        write_json_file(&models_path, &models)?;
        chosen = Some((
            LOCAL_PROVIDER_ID.to_owned(),
            Some(PI_DEFAULT_MODEL.to_owned()),
        ));
    }

    let Some((provider, model)) = chosen else {
        return Ok(());
    };

    let mut settings = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    if !settings.is_object() {
        settings = json!({});
    }

    // Names Pi treats as known providers, so a cc-switch default we cannot
    // resolve gets corrected while a valid one is left untouched.
    let mut known: Vec<String> = Vec::new();
    if let Some(providers) = models.get("providers").and_then(Value::as_object) {
        known.extend(providers.keys().cloned());
    }
    for name in ["auth.json", "models-store.json"] {
        if let Some(value) = read_json(&agent_dir.join(name)) {
            if let Some(object) = value.as_object() {
                known.extend(object.keys().cloned());
            }
        }
    }

    let configured_provider = settings
        .get("defaultProvider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let configured_model = settings
        .get("defaultModel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut changed = false;
    if !known.iter().any(|name| name == &configured_provider) {
        if let Some(object) = settings.as_object_mut() {
            object.insert("defaultProvider".to_owned(), json!(provider));
            if let Some(model) = &model {
                object.insert("defaultModel".to_owned(), json!(model));
            }
        }
        changed = true;
    } else if configured_model.is_empty() {
        if let (Some(model), Some(object)) = (&model, settings.as_object_mut()) {
            object.insert("defaultModel".to_owned(), json!(model));
            changed = true;
        }
    }
    if changed {
        write_json_file(settings_path, &settings)?;
    }
    Ok(())
}

fn local_provider_entry() -> Value {
    json!({
        "name": LOCAL_PROVIDER_ID,
        "baseUrl": LOCAL_PROVIDER_BASE_URL,
        "api": "openai-completions",
        "apiKey": LOCAL_PROVIDER_API_KEY,
        "models": [
            {
                "id": PI_DEFAULT_MODEL,
                "name": "DeepSeek V4.1 Flash",
                "reasoning": true,
                "input": ["text", "image"],
                "contextWindow": 1_048_576,
                "maxTokens": 384_000
            },
            {
                "id": "auto/glm-5.3-flash",
                "name": "GLM-5.3 Flash",
                "reasoning": true,
                "input": ["text"],
                "contextWindow": 200_000,
                "maxTokens": 32_000
            }
        ]
    })
}

fn write_json_file(path: &Path, value: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
    std::fs::write(path, text)
}

/// Read the real error Pi recorded for a session, because ACP itself only
/// reports `end_turn`. Pi writes the assistant message (with `stopReason` and
/// `errorMessage`) to its native transcript; the ACP bridge maps the session id
/// to that file in `session-map.json`. Returns the error message of the most
/// recent assistant message when that turn ended in an error, else `None`.
fn pi_error_message(agent_dir: Option<&Path>, session_id: &str) -> Option<String> {
    let agent_dir = agent_dir?;
    let acp_dir = agent_dir.parent()?.join("pi-acp");
    let map_text = std::fs::read_to_string(acp_dir.join("session-map.json")).ok()?;
    let map: Value = serde_json::from_str(&map_text).ok()?;
    let session_file = map
        .get("sessions")?
        .get(session_id)?
        .get("sessionFile")?
        .as_str()?;
    let transcript = std::fs::read_to_string(session_file).ok()?;
    for line in transcript.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Only the most recent assistant message reflects this turn; stop there
        // rather than reporting an error left over from an earlier turn.
        if message.get("stopReason").and_then(Value::as_str) != Some("error") {
            return None;
        }
        let detail = message
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        return Some(if detail.is_empty() {
            "unknown error".to_owned()
        } else {
            detail.to_owned()
        });
    }
    None
}

async fn drive_inner(
    launch: Launch,
    request: RunRequest,
    controls: crate::RunControls,
    events: &EventSender,
    harness_id: HarnessId,
    interrupt_grace: Duration,
    kill_grace: Duration,
) -> Result<(), HarnessError> {
    let cwd = resolve_cwd(&request.cwd)?;

    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Keep the child out of our console window.
        command.creation_flags(0x0800_0000);
    }
    if !launch.path_prepend.is_empty() {
        let mut paths = launch.path_prepend.clone();
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        command.env("PATH", std::env::join_paths(paths).unwrap_or_default());
    }
    // pi-acp resolves the `pi` executable itself; give it an absolute path so it
    // works when `pi` lives in our bundled node_modules instead of the machine.
    if harness_id == HarnessId::Pi
        && let Some(parent) = launch.program.parent()
    {
        let bin = parent.join("node_modules").join(".bin");
        let candidate = if cfg!(windows) {
            bin.join("pi.cmd")
        } else {
            bin.join("pi")
        };
        if candidate.is_file() {
            command.env("PI_ACP_PI_COMMAND", candidate);
        }
    }
    // `pi` and `pi-acp` persist session maps, trust state and credentials under
    // `$HOME/.pi`. Point them at a private home inside Wu's own data directory so
    // they never write into (or read plugin state from) the machine-wide install,
    // and so the sandboxed app never has to touch the user's real home.
    let pi_home = if harness_id == HarnessId::Pi {
        prepare_pi_home(&launch.program, &launch.agent_dir)
    } else {
        None
    };
    if let Some((home, agent_dir)) = &pi_home {
        command
            .env("USERPROFILE", home.as_os_str())
            .env("HOME", home.as_os_str())
            .env("PI_CODING_AGENT_DIR", agent_dir.as_os_str());
    }
    let mut child: Child = command.spawn().map_err(|error| {
        HarnessError::NotInstalled(format!("{} ({error})", launch.program.display()))
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("agent stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("agent stdout unavailable".into()))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!(target: "agent_harness::acp", "{line}");
            }
        });
    }

    let (client, mut incoming) = RpcClient::new(stdin, stdout);

    let init = client
        .request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {"name": "IDE", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await?;
    tracing::debug!(target: "agent_harness::acp", "initialize: {init}");

    let session = match request.resume.as_deref() {
        Some(previous) => match client
            .request(
                "session/load",
                json!({
                    "sessionId": previous,
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": &request.mcp_servers
                }),
            )
            .await
        {
            Ok(loaded) => loaded,
            Err(error) => {
                log::debug!(
                    target: "agent_harness::acp",
                    "session/load failed, starting a new session: {error}"
                );
                client
                    .request(
                        "session/new",
                        json!({ "cwd": cwd.to_string_lossy(), "mcpServers": &request.mcp_servers }),
                    )
                    .await?
            }
        },
        None => {
            client
                .request(
                    "session/new",
                    json!({ "cwd": cwd.to_string_lossy(), "mcpServers": &request.mcp_servers }),
                )
                .await?
        }
    };
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| request.resume.clone())
        .ok_or_else(|| HarnessError::Protocol("agent did not return a sessionId".into()))?;

    // ACP `session/load` replays the stored conversation as `session/update`
    // notifications so a fresh client can rebuild its view. Wu keeps its own
    // transcript and renders it separately, so anything queued before we send
    // the prompt is history replay, not this turn's output. Drop it, otherwise
    // every resumed turn shows the previous answer again. Answer any request the
    // agent made while replaying so it is not left waiting.
    while let Ok(message) = incoming.try_recv() {
        if let Incoming::Request {
            id,
            method,
            params,
        } = message
        {
            handle_request(&client, &id, &method, &params, &controls.permission);
        }
    }

    if harness_id == HarnessId::Pi {
        apply_pi_selection(&client, &session_id, request.model.as_deref(), request.reasoning)
            .await;
    }

    let assistant_message_id = uuid::Uuid::new_v4().to_string();
    send(
        events,
        AgentEvent::SessionStarted {
            harness: harness_id,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: cwd.to_string_lossy().into_owned(),
            session_id: session_id.clone(),
            assistant_message_id,
        },
    )
    .await;

    let prompt = prompt_blocks(&request);
    let prompt_future = client.request_now(
        "session/prompt",
        json!({ "sessionId": session_id, "prompt": prompt }),
    );
    let (done_tx, mut done_rx) = mpsc::channel::<Result<Value, HarnessError>>(1);
    tokio::spawn(async move {
        let result = prompt_future.await;
        let _ = done_tx.send(result).await;
    });

    let _steering = controls.steering;
    let interrupt = controls.interrupt;
    let permission = controls.permission;

    let mut final_status = DoneStatus::Completed;
    let mut final_error: Option<String> = None;
    let mut saw_output = false;

    loop {
        tokio::select! {
            _ = interrupt.cancelled() => {
                client.notify("session/cancel", Some(json!({ "sessionId": session_id })));
                final_status = DoneStatus::Interrupted;
                break;
            }
            result = done_rx.recv() => {
                match result {
                    Some(Ok(value)) => {
                        let stop = value.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                        match stop {
                            "cancelled" => final_status = DoneStatus::Interrupted,
                            "refusal" => {
                                final_status = DoneStatus::Errored;
                                final_error = Some("The agent refused the request.".to_owned());
                            }
                            _ => final_status = DoneStatus::Completed,
                        }
                    }
                    Some(Err(error)) => {
                        final_status = DoneStatus::Errored;
                        final_error = Some(error.to_string());
                    }
                    None => break,
                }
                break;
            }
            message = incoming.recv() => {
                match message {
                    Some(Incoming::Notification { method, params }) => {
                        if method == "session/update" && handle_update(&params, events).await {
                            saw_output = true;
                        }
                    }
                    Some(Incoming::Request { id, method, params }) => {
                        handle_request(&client, &id, &method, &params, &permission);
                    }
                    Some(Incoming::Eof) | None => {
                        final_status = DoneStatus::Errored;
                        final_error = Some("The agent exited before finishing.".to_owned());
                        break;
                    }
                }
            }
        }
    }

    // Give the child a moment, then stop it.
    let _ = tokio::time::timeout(interrupt_grace, child.wait()).await;
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(kill_grace, child.wait()).await;
    }

    // Pi reports a failed model call as `end_turn` over ACP and never forwards
    // the error text (it may even stream retry notices as normal text), so the
    // turn would otherwise look like a successful reply. The native transcript
    // still records the truth, so consult it and surface the real cause.
    if matches!(final_status, DoneStatus::Completed) {
        if let Some(detail) = pi_error_message(
            pi_home.as_ref().map(|(_, agent_dir)| agent_dir.as_path()),
            &session_id,
        ) {
            final_status = DoneStatus::Errored;
            final_error = Some(format!("The model request failed: {detail}"));
        } else if !saw_output {
            final_status = DoneStatus::Errored;
            final_error = Some(
                "The agent finished without returning any output. The model request may have failed (invalid API key, expired key, or network error).".to_owned(),
            );
        }
    }

    send(
        events,
        AgentEvent::Done {
            status: final_status,
            result: None,
            error: final_error,
            session_id: Some(session_id),
        },
    )
    .await;

    Ok(())
}

fn resolve_cwd(cwd: &str) -> Result<PathBuf, HarnessError> {
    if cwd.is_empty() {
        return std::env::current_dir().map_err(HarnessError::Io);
    }
    let path = PathBuf::from(cwd);
    if !path.is_dir() {
        return Err(HarnessError::Protocol(format!(
            "project folder not found: {cwd}"
        )));
    }
    Ok(path)
}

/// The folders a run works in, searched for a bundled agent runtime. A turn
/// knows its project even when the process does not: an executable copied out
/// of its bundle can be started anywhere, but the project it opens may be a
/// checkout that carries `crates/wu/resources/agents`.
fn session_roots(cwd: &Path) -> Vec<PathBuf> {
    if cwd.as_os_str().is_empty() {
        Vec::new()
    } else {
        vec![cwd.to_path_buf()]
    }
}

fn prompt_blocks(request: &RunRequest) -> Vec<Value> {
    vec![json!({ "type": "text", "text": request.prompt })]
}

fn handle_request(
    client: &RpcClient,
    id: &Value,
    method: &str,
    params: &Value,
    permission: &tokio::sync::watch::Receiver<PermissionMode>,
) {
    match method {
        "session/request_permission" => {
            let mode = *permission.borrow();
            let response = permission_response(mode, params);
            client.respond(id, response);
        }
        "fs/read_text_file" | "fs/write_text_file" | "terminal/create" | "terminal/output"
        | "terminal/wait_for_exit" | "terminal/kill" | "terminal/release" => {
            client.respond_error(id, -32601, "This client does not provide filesystem or terminal delegation.");
        }
        _ => {
            client.respond_error(id, -32601, "method not supported by this client");
        }
    }
}

fn permission_response(permission: PermissionMode, params: &Value) -> Value {
    let auto_allow = matches!(
        permission,
        PermissionMode::FullAccess
            | PermissionMode::Auto
            | PermissionMode::AcceptEdits
            | PermissionMode::DontAsk
            | PermissionMode::ApproveForMe
    );
    let options = params.get("options").and_then(Value::as_array);

    let option_id = |prefixes: &[&str]| -> Option<String> {
        options.and_then(|opts| {
            opts.iter().find_map(|option| {
                let kind = option.get("kind").and_then(Value::as_str).unwrap_or("");
                if prefixes.iter().any(|prefix| kind.starts_with(prefix)) {
                    option
                        .get("optionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                } else {
                    None
                }
            })
        })
    };

    let chosen = if auto_allow {
        option_id(&["allow"]).or_else(|| {
            options.and_then(|opts| opts.first()).and_then(|option| {
                option
                    .get("optionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        })
    } else {
        option_id(&["reject"])
    };

    match chosen {
        Some(option_id) => json!({
            "outcome": { "outcome": "selected", "optionId": option_id }
        }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

async fn handle_update(params: &Value, events: &EventSender) -> bool {
    let Some(update) = params.get("update") else {
        return false;
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut produced = false;
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = content_text(update.get("content")) {
                if !text.trim().is_empty() {
                    produced = true;
                }
                send(events, AgentEvent::TextDelta { text }).await;
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = content_text(update.get("content")) {
                send(events, AgentEvent::ReasoningDelta { text }).await;
            }
        }
        "tool_call" => {
            if let Some(id) = update.get("toolCallId").and_then(Value::as_str) {
                produced = true;
                let call = map_tool_call(update);
                send(
                    events,
                    AgentEvent::ToolCall {
                        id: id.to_owned(),
                        call,
                    },
                )
                .await;
            }
        }
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str);
            if matches!(status, Some("completed") | Some("failed"))
                && let Some(id) = update.get("toolCallId").and_then(Value::as_str)
            {
                produced = true;
                let is_error = status == Some("failed");
                let output = content_text(update.get("content"));
                send(
                    events,
                    AgentEvent::ToolResult {
                        id: id.to_owned(),
                        is_error,
                        output,
                        diff: None,
                    },
                )
                .await;
            }
        }
        "plan" => {
            produced = true;
            let items = map_plan(update);
            send(
                events,
                AgentEvent::ToolCall {
                    id: LIVE_PLAN_TOOL_ID.to_owned(),
                    call: ToolCall::Todo { items },
                },
            )
            .await;
        }
        "available_commands_update" => {
            let commands = map_commands(update);
            if !commands.is_empty() {
                send(events, AgentEvent::AvailableCommands { commands }).await;
            }
        }
        "usage_update" => {
            let tokens = update.get("used").and_then(Value::as_u64);
            let window = update.get("size").and_then(Value::as_u64);
            send(events, AgentEvent::ContextUsage { tokens, window }).await;
        }
        _ => {}
    }
    produced
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    if let Some(array) = content.as_array() {
        let joined: String = array
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    content
        .as_str()
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

fn location_path(update: &Value) -> Option<String> {
    update
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|locations| locations.first())
        .and_then(|location| location.get("path"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn input_string(update: &Value, key: &str) -> Option<String> {
    update
        .get("rawInput")
        .and_then(|input| input.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn map_tool_call(update: &Value) -> ToolCall {
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let kind = update.get("kind").and_then(Value::as_str).unwrap_or("other");
    let path = location_path(update);
    match kind {
        "execute" => ToolCall::Exec {
            command: input_string(update, "command").unwrap_or_else(|| title.clone()),
        },
        "read" => ToolCall::ReadFile {
            path: path.unwrap_or_default(),
        },
        "edit" => ToolCall::EditFile {
            path: path.unwrap_or_default(),
            old_string: None,
            new_string: None,
        },
        "write" | "delete" | "move" => ToolCall::WriteFile {
            path: path.unwrap_or_default(),
            content: None,
        },
        "search" => ToolCall::Search {
            pattern: title,
            path,
        },
        "fetch" => ToolCall::WebFetch {
            url: input_string(update, "url").unwrap_or(title),
            prompt: None,
        },
        _ => ToolCall::Unknown {
            name: title,
            input: update.get("rawInput").cloned(),
        },
    }
}

fn map_plan(update: &Value) -> Vec<TodoItem> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let text = entry.get("content").and_then(Value::as_str)?;
                    let status = entry
                        .get("status")
                        .and_then(Value::as_str)
                        .map(TodoStatus::parse)
                        .unwrap_or_default();
                    Some(TodoItem::new(text, status))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_commands(update: &Value) -> Vec<SlashCommand> {
    update
        .get("availableCommands")
        .and_then(Value::as_array)
        .map(|commands| {
            commands
                .iter()
                .filter_map(|command| {
                    let name = command.get("name").and_then(Value::as_str)?;
                    Some(SlashCommand {
                        name: name.to_owned(),
                        description: command
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input_hint: command
                            .get("input")
                            .and_then(|input| input.get("hint"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        is_skill: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let file_names = program_file_names(name);
    for dir in dirs {
        for file in &file_names {
            let candidate = dir.join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // `pi-acp/node_modules/.bin/pi-acp` style installs.
        let nested = dir.join(name).join("bin").join(name);
        if nested.is_file() {
            return Some(nested);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for file in &file_names {
                let candidate = dir.join(file);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn program_file_names(name: &str) -> Vec<String> {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut names = vec![name.to_owned()];
    #[cfg(windows)]
    {
        names.push(format!("{name}.exe"));
        names.push(format!("{name}.cmd"));
        names.push(format!("{name}.bat"));
    }
    names
}

/// Every place a bundled agent runtime can live, in priority order.
///
/// `project_roots` are the folders the current session works in. They come
/// after the release locations, so a bundle shipped next to the executable
/// always wins, but before nothing: an executable copied out of its bundle has
/// no `agents/` beside it, and the project it was pointed at may be a checkout
/// that carries the runtime.
fn bundled_dirs(project_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("WU_AGENT_BIN_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    let mut anchors = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("agents"));
        dirs.push(parent.join("resources").join("agents"));
        if let Some(install) = parent.parent() {
            // Windows installers keep the executable in `bin\` next to `agents\`.
            dirs.push(install.join("agents"));
        }
        // macOS app bundle layout: Contents/MacOS/wu -> Contents/Resources/agents
        if let Some(contents) = parent.parent() {
            dirs.push(contents.join("Resources").join("agents"));
        }
        dirs.push(parent.to_path_buf());
        anchors.push(parent.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        anchors.push(cwd);
    }
    anchors.extend(project_roots.iter().cloned());
    // Appended after every release location, so a directory shipped with the
    // executable always wins.
    dirs.extend(checkout_agent_dirs(&anchors));
    dirs
}

/// The agent runtime of a source checkout, for `cargo build` binaries.
///
/// `script/bundle-*` copies `crates/wu/resources/agents` next to the executable
/// for a release, but `cargo build` leaves the executable in `target/` with no
/// `agents` directory beside it, so Pi looks uninstalled in a development build.
/// The checkout is found at runtime by walking up from the executable, the
/// working directory and the folders the session works in: embedding a
/// build-time path such as `CARGO_MANIFEST_DIR` would put an absolute path in
/// the binary, which breaks as soon as the checkout moves or the build happens
/// somewhere else. Walking up only matches a machine with a checkout, and it is
/// consulted after every path a release uses, so shipped builds are unaffected.
fn checkout_agent_dirs(anchors: &[PathBuf]) -> Vec<PathBuf> {
    const CHECKOUT_LAYOUTS: &[&str] = &["crates/wu/resources/agents", "agents"];
    let mut dirs: Vec<PathBuf> = Vec::new();
    for anchor in anchors {
        for ancestor in anchor.ancestors() {
            for layout in CHECKOUT_LAYOUTS {
                let candidate = ancestor.join(layout);
                if candidate.is_dir() && !dirs.contains(&candidate) {
                    dirs.push(candidate);
                }
            }
        }
    }
    dirs
}

/// What a failed lookup looked for and the folders it checked, for the error
/// message. Without it the user only learns that the agent is missing, not
/// where a copied-out executable expects the bundled `agents` folder to be.
fn searched(missing: &str, dirs: &[PathBuf]) -> String {
    const SHOWN: usize = 8;
    let mut description = missing.to_owned();
    for dir in dirs.iter().take(SHOWN) {
        description.push_str("\n  ");
        description.push_str(&dir.display().to_string());
    }
    if dirs.len() > SHOWN {
        description.push_str("\n  …");
    }
    description
}

/// A launch that already resolved in this process.
///
/// Resolution reads the executable's location and the process working
/// directory, both fixed for the life of the process but easy to get wrong (a
/// shortcut with a different "Start in", an executable started by a shell in
/// another folder). A runtime found once must stay found, so later turns never
/// depend on that ambient state matching the current session.
static REMEMBERED_LAUNCHES: Mutex<Vec<(HarnessId, Launch)>> = Mutex::new(Vec::new());

fn remembered_launch(id: HarnessId) -> Option<Launch> {
    REMEMBERED_LAUNCHES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|(cached, _)| *cached == id)
        .map(|(_, launch)| launch.clone())
}

fn remember_launch(id: HarnessId, launch: &Launch) {
    let mut launches = REMEMBERED_LAUNCHES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !launches.iter().any(|(cached, _)| *cached == id) {
        launches.push((id, launch.clone()));
    }
}

/// The file name a bundled runtime uses for `name`.
fn bundled_program_file(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pi_home_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("pi-home").join(".pi").join("agent");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("auth.json");
        std::fs::write(&file, "{}").unwrap();

        restrict_to_owner(&nested).unwrap();
        restrict_to_owner(&file).unwrap();

        let dir_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "session transcripts would be world-readable");
        assert_eq!(file_mode, 0o600, "credentials would be world-readable");
    }

    #[test]
    fn permission_response_allows_under_full_access() {
        let params = json!({
            "options": [
                { "optionId": "reject", "kind": "reject_once" },
                { "optionId": "allow", "kind": "allow_once" }
            ]
        });
        let response = permission_response(PermissionMode::FullAccess, &params);
        assert_eq!(response["outcome"]["optionId"], "allow");
    }

    #[test]
    fn permission_response_rejects_when_asking() {
        let params = json!({
            "options": [
                { "optionId": "allow", "kind": "allow_once" },
                { "optionId": "reject", "kind": "reject_once" }
            ]
        });
        let response = permission_response(PermissionMode::Ask, &params);
        assert_eq!(response["outcome"]["optionId"], "reject");
    }

    #[test]
    fn maps_an_execute_tool_call() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run tests",
            "kind": "execute",
            "rawInput": { "command": "cargo test" }
        });
        assert_eq!(
            map_tool_call(&update),
            ToolCall::Exec {
                command: "cargo test".into()
            }
        );
    }

    #[test]
    fn mirrored_settings_drop_global_extensions() {
        let source = json!({
            "defaultProvider": "magpie",
            "defaultModel": "auto/deepseek-v4.1-flash",
            "packages": ["npm:pi-extension"],
            "enabledModels": ["pi"]
        });
        let settings = isolated_pi_settings(&source);
        assert_eq!(settings["defaultProvider"], "magpie");
        assert_eq!(settings["defaultModel"], "auto/deepseek-v4.1-flash");
        assert!(settings.get("packages").is_none());
        assert!(settings.get("enabledModels").is_none());
    }

    #[test]
    fn content_text_reads_blocks() {
        let content = json!([{ "type": "text", "text": "hello " }, { "type": "text", "text": "world" }]);
        assert_eq!(content_text(Some(&content)).as_deref(), Some("hello world"));
    }

    #[test]
    fn addendum_leads_with_its_version_marker() {
        assert_eq!(
            pi_addendum(7).lines().next(),
            Some("<!-- wu:pi-addendum:v7 -->")
        );
    }

    #[test]
    fn addendum_tells_pi_where_project_context_lives() {
        let text = pi_addendum(PI_ADDENDUM_VERSION);
        for needle in [
            "Wu (wuIDE)",
            "working directory is the project root",
            "`AGENTS.md`",
            "`repo_map`",
            "`diagnostics`",
            "Reply in the language the user writes in",
        ] {
            assert!(text.contains(needle), "addendum lost {needle:?}");
        }
        // A file shared by every project must not carry an absolute path.
        assert!(!text.contains('\\'), "addendum must not embed platform paths");
    }

    #[test]
    fn addendum_path_sits_in_the_agent_dir() {
        let agent_dir = Path::new("pi-home").join(".pi").join("agent");
        assert_eq!(
            pi_addendum_path(&agent_dir),
            agent_dir.join("APPEND_SYSTEM.md")
        );
    }

    #[test]
    fn writing_the_addendum_twice_leaves_a_single_copy() {
        let dir = tempfile::tempdir().unwrap();
        write_pi_addendum(dir.path());
        write_pi_addendum(dir.path());

        let written = std::fs::read_to_string(pi_addendum_path(dir.path())).unwrap();
        assert_eq!(written, pi_addendum(PI_ADDENDUM_VERSION));
        assert_eq!(written.matches("<!-- wu:pi-addendum:").count(), 1);
    }

    #[test]
    fn writing_the_addendum_keeps_the_users_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "user rules\n").unwrap();

        write_pi_addendum(dir.path());

        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "user rules\n"
        );
    }

    fn server_entry() -> Value {
        pi_mcp_server_entry(
            Path::new("agents"),
            Path::new("agents/node"),
            Path::new("pi-home/.pi/agent"),
        )
    }

    #[test]
    fn mcp_entry_exposes_tools_directly() {
        let entry = server_entry();
        let script = pi_mcp_server_script(Path::new("agents"));
        // Pi's default exposure is `codemode`, which hides the tools from the model.
        assert_eq!(entry["exposure"], "direct");
        assert_eq!(entry["command"], "agents/node");
        assert_eq!(entry["args"][0].as_str(), script.to_str());
        assert_eq!(entry["env"]["PI_CODING_AGENT_DIR"], "pi-home/.pi/agent");
    }

    #[test]
    fn mcp_config_keeps_the_servers_the_user_configured() {
        let existing = r#"{
  "mcpServers": {
    "github": { "url": "https://example.com/mcp", "exposure": "deferred" }
  }
}"#;
        let merged = merge_mcp_config(Some(existing), server_entry()).unwrap();
        let config: Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(
            config["mcpServers"]["github"]["url"],
            "https://example.com/mcp"
        );
        assert_eq!(config["mcpServers"]["github"]["exposure"], "deferred");
        assert_eq!(config["mcpServers"]["wu"]["exposure"], "direct");
    }

    #[test]
    fn mcp_config_creates_the_file_when_there_is_none() {
        let merged = merge_mcp_config(None, server_entry()).unwrap();
        let config: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(config["mcpServers"]["wu"]["exposure"], "direct");
    }

    #[test]
    fn mcp_config_replaces_only_wus_entry() {
        let existing = r#"{"mcpServers":{"wu":{"exposure":"codemode"},"other":{"command":"x"}}}"#;
        let merged = merge_mcp_config(Some(existing), server_entry()).unwrap();
        let config: Value = serde_json::from_str(&merged).unwrap();

        assert_eq!(config["mcpServers"]["wu"]["exposure"], "direct");
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn mcp_config_is_left_alone_when_it_cannot_be_parsed() {
        // Overwriting would silently delete every server the user configured.
        assert!(merge_mcp_config(Some("{ not json"), server_entry()).is_none());
        assert!(merge_mcp_config(Some("[]"), server_entry()).is_none());
        assert!(merge_mcp_config(Some(r#"{"mcpServers":[]}"#), server_entry()).is_none());
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the MCP server is a separate Node process"
    )]
    fn bundled_mcp_server_lists_its_tools_and_survives_a_missing_snapshot() {
        use std::io::{Read as _, Write as _};

        let agents = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates directory")
            .join("wu/resources/agents");
        let script = agents.join("mcp/wu-repo-map/server.js");
        assert!(script.is_file(), "the bundled MCP server is missing");

        let bundled = agents.join(if cfg!(windows) { "node.exe" } else { "node" });
        let node = if bundled.is_file() {
            bundled
        } else {
            PathBuf::from("node")
        };

        let empty_home = tempfile::tempdir().unwrap();
        let mut child = match std::process::Command::new(&node)
            .arg(&script)
            .env("PI_CODING_AGENT_DIR", empty_home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skipping the MCP server test: cannot run {node:?}: {error}");
                return;
            }
        };

        let mut stdin = child.stdin.take().unwrap();
        for request in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "repo_map", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "diagnostics", "arguments": {}}}),
        ] {
            writeln!(stdin, "{request}").unwrap();
        }
        drop(stdin);

        let mut stdout = String::new();
        child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
        child.wait().unwrap();

        let responses: Vec<Value> = stdout
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        let tools = responses
            .iter()
            .find(|response| response["id"] == 2)
            .expect("tools/list answered")["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(tools, ["repo_map", "symbols", "diagnostics"]);

        // No snapshot in a fresh agent directory: the model must be told what to
        // do, not handed a protocol error it cannot act on.
        let missing = responses
            .iter()
            .find(|response| response["id"] == 3)
            .expect("tools/call answered");
        assert!(missing.get("error").is_none(), "a missing snapshot is not a failure");
        let text = missing["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Repository Map panel"), "unhelpful message: {text}");

        let missing = responses
            .iter()
            .find(|response| response["id"] == 4)
            .expect("diagnostics answered");
        assert!(missing.get("error").is_none(), "a missing snapshot is not a failure");
        let text = missing["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Problems panel"), "unhelpful message: {text}");
    }

    #[test]
    fn a_checkout_supplies_the_agent_runtime_a_dev_build_lacks() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("crates/wu/resources/agents");
        std::fs::create_dir_all(agents.join("node_modules")).unwrap();
        let exe_dir = root.path().join("target/release-fast");
        std::fs::create_dir_all(&exe_dir).unwrap();

        // `cargo build` puts the executable in `target/` with no `agents` beside
        // it; the checkout above it is what makes Pi available again.
        let found = checkout_agent_dirs(&[exe_dir, root.path().to_path_buf()]);
        assert_eq!(found.first(), Some(&agents));
        assert_eq!(found.iter().filter(|dir| *dir == &agents).count(), 1);

        let plain = tempfile::tempdir().unwrap();
        let nested = plain.path().join("target/release-fast");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            !checkout_agent_dirs(&[nested])
                .iter()
                .any(|dir| dir.starts_with(plain.path())),
            "a tree without a checkout layout must not supply agents"
        );
    }

    #[test]
    fn a_projects_checkout_supplies_the_runtime_a_copied_exe_lacks() {
        let project = tempfile::tempdir().unwrap();
        let agents = project.path().join("crates/wu/resources/agents");
        std::fs::create_dir_all(agents.join("node_modules")).unwrap();
        let nested = project.path().join("crates/wu/src");
        std::fs::create_dir_all(&nested).unwrap();

        // The executable sits in a folder with nothing beside it, which is what
        // copying it out of a release produces; only the session's project can
        // still reach the runtime.
        let dirs = bundled_dirs(&[nested]);
        assert!(
            dirs.contains(&agents),
            "the project the session works in must be searched: {dirs:?}"
        );
    }

    #[test]
    fn a_missing_agent_error_names_the_folders_it_checked() {
        let dirs = [
            PathBuf::from("C:/app/agents"),
            PathBuf::from("C:/project/crates/wu/resources/agents"),
        ];
        let error = HarnessError::agent_missing(
            HarnessId::Pi,
            &searched("node_modules/pi-acp/dist/index.js", &dirs),
        );

        let message = error.to_string();
        for needle in [
            "node_modules/pi-acp/dist/index.js",
            "C:/app/agents",
            "C:/project/crates/wu/resources/agents",
        ] {
            assert!(message.contains(needle), "{message}");
        }
    }

    #[test]
    fn the_first_launch_resolved_for_a_harness_sticks() {
        let launch = |program: &str| Launch {
            program: program.into(),
            args: Vec::new(),
            agent_dir: PathBuf::from("agents"),
            path_prepend: Vec::new(),
        };

        // No other test resolves DeepSeek, so the process-wide cache starts
        // empty for it. A later resolution from another folder must not replace
        // a runtime the session already started.
        remember_launch(HarnessId::Dsh, &launch("first"));
        remember_launch(HarnessId::Dsh, &launch("second"));

        let remembered = remembered_launch(HarnessId::Dsh).expect("the launch is kept");
        assert_eq!(remembered.program, PathBuf::from("first"));
    }
}
