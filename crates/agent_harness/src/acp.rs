//! A generic [Agent Client Protocol](https://agentclientprotocol.com) harness.
//!
//! Wu ships no ACP support of its own, but a handful of agents already speak
//! ACP over newline-delimited JSON-RPC on stdio (`dsh-acp` for DeepSeek,
//! `pi-acp` for Pi). This module drives any such agent and translates its
//! `session/update` notifications into Wu's [`AgentEvent`] stream.

use std::path::{Path, PathBuf};
use std::sync::Arc;
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

struct Launch {
    program: PathBuf,
    args: Vec<String>,
    /// Directory prepended to PATH for the child (e.g. bundled node_modules/.bin).
    path_prepend: Option<PathBuf>,
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

    fn resolve_launch(&self) -> Result<Launch, HarnessError> {
        if let Some(path) = &self.program_override {
            return Ok(self.build_launch(path.clone()));
        }
        if let Some(found) = find_program(self.preset.program) {
            return Ok(self.build_launch(found));
        }
        Err(HarnessError::agent_missing(
            self.preset.id,
            self.preset.program,
        ))
    }

    fn build_launch(&self, program: PathBuf) -> Launch {
        let mut args: Vec<String> = Vec::new();
        if let Some(entry) = self.preset.js_entry {
            let base = program.parent().unwrap_or_else(|| Path::new("."));
            args.push(base.join(entry).to_string_lossy().into_owned());
        }
        args.extend(self.preset.args.iter().map(|a| (*a).to_owned()));
        self.push_data_dir(&mut args, &program);
        let path_prepend = program.parent().and_then(|parent| {
            let bin = parent.join("node_modules").join(".bin");
            bin.is_dir().then_some(bin)
        });
        Launch {
            program,
            args,
            path_prepend,
        }
    }

    /// Point the agent at an isolated data directory inside Wu's own folder so
    /// its plugins, credentials and session state never touch the machine's
    /// globally installed agent.
    fn push_data_dir(&self, args: &mut Vec<String>, program: &Path) {
        let Some(flag) = self.preset.data_dir_flag else {
            return;
        };
        let slug = match self.preset.id {
            HarnessId::Dsh => "deepseek",
            HarnessId::Pi => "pi",
            _ => "agent",
        };
        let base = agent_data_root(program);
        let dir = base.join(slug);
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
        &[]
    }

    fn installed(&self) -> bool {
        self.resolve_launch().is_ok()
    }

    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_launch().ok().map(|launch| launch.program)
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self.fallback_models())
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
        let launch = self.resolve_launch()?;
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

/// Writable per-user directory where Wu keeps each agent's isolated plugins,
/// credentials and session state. Overridable for tests via `WU_AGENT_DATA_DIR`.
fn agent_data_root(program: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("WU_AGENT_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(dir).join("Wu").join("agents");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Wu")
                .join("agents");
        }
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("wu").join("agents");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("wu")
            .join("agents");
    }
    program
        .parent()
        .map(|parent| parent.join("data"))
        .unwrap_or_else(|| PathBuf::from("data"))
}

/// Build an isolated `$HOME` for the bundled Pi agent and seed it with the
/// user's existing model/credential files (but not their globally installed
/// extensions, which would need `npm` at runtime).
///
/// The isolated home also gets a provider that is reachable on this machine:
/// the inherited `defaultProvider` is a cc-switch alias that only exists in the
/// user's global setup, so without this Pi would fall back to a model it has no
/// credentials for and answer every prompt with nothing.
fn prepare_pi_home(program: &Path) -> Option<(PathBuf, PathBuf)> {
    let base = agent_data_root(program);
    let home = base.join("pi-home");
    let agent_dir = home.join(".pi").join("agent");
    let acp_dir = home.join(".pi").join("pi-acp");
    if let Err(error) = std::fs::create_dir_all(&agent_dir)
        .and_then(|_| std::fs::create_dir_all(&acp_dir))
    {
        log::warn!("failed to prepare Pi home {home:?}: {error}");
        return None;
    }

    let settings_path = agent_dir.join("settings.json");
    if !settings_path.exists() {
        if let Some(source_home) = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
        {
            let source_agent = source_home.join(".pi").join("agent");
            for name in ["auth.json", "models.json", "models-store.json"] {
                let from = source_agent.join(name);
                let to = agent_dir.join(name);
                if from.is_file() && !to.exists() {
                    if let Err(error) = std::fs::copy(&from, &to) {
                        log::warn!("failed to seed Pi {name:?}: {error}");
                    }
                }
            }
            let source_settings = source_agent.join("settings.json");
            let mut value = std::fs::read_to_string(&source_settings)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or_else(|| json!({}));
            if let Some(object) = value.as_object_mut() {
                object.remove("packages");
                object.remove("enabledModels");
            }
            if let Err(error) = write_json_file(&settings_path, &value) {
                log::warn!("failed to write Pi settings: {error}");
            }
        } else if let Err(error) = std::fs::write(&settings_path, "{}") {
            log::warn!("failed to write Pi settings: {error}");
        }
    }

    if let Err(error) = ensure_local_pi_provider(&agent_dir, &settings_path) {
        log::warn!("failed to configure the Pi model provider: {error}");
    }

    Some((home, agent_dir))
}

/// Make sure the isolated Pi home can reach a model: merge the local gateway
/// into `models.json`, and repoint the default provider at it when the
/// configured provider cannot be resolved from the isolated files.
fn ensure_local_pi_provider(agent_dir: &Path, settings_path: &Path) -> std::io::Result<()> {
    let models_path = agent_dir.join("models.json");
    let mut models = std::fs::read_to_string(&models_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    if !models.is_object() {
        models = json!({});
    }

    let mut known: Vec<String> = Vec::new();
    let mut models_changed = false;
    if let Some(root) = models.as_object_mut() {
        if !root.get("providers").is_some_and(Value::is_object) {
            root.insert("providers".to_owned(), json!({}));
        }
        if let Some(providers) = root.get_mut("providers").and_then(Value::as_object_mut) {
            known.extend(providers.keys().cloned());
            if !providers.contains_key(LOCAL_PROVIDER_ID) {
                providers.insert(LOCAL_PROVIDER_ID.to_owned(), local_provider_entry());
                models_changed = true;
            }
        }
    }
    if models_changed {
        write_json_file(&models_path, &models)?;
    }
    known.push(LOCAL_PROVIDER_ID.to_owned());

    for name in ["auth.json", "models-store.json"] {
        let path = agent_dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(object) = value.as_object() {
            known.extend(object.keys().cloned());
        }
    }

    let mut settings = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    if !settings.is_object() {
        settings = json!({});
    }
    let configured = settings
        .get("defaultProvider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !known.iter().any(|name| name == configured) {
        if let Some(object) = settings.as_object_mut() {
            object.insert("defaultProvider".to_owned(), json!(LOCAL_PROVIDER_ID));
            object.insert("defaultModel".to_owned(), json!(PI_DEFAULT_MODEL));
        }
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
    if let Some(dir) = &launch.path_prepend {
        let mut paths = vec![dir.clone()];
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
        prepare_pi_home(&launch.program)
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
                "clientInfo": {"name": "Wu", "version": env!("CARGO_PKG_VERSION")}
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
                    "mcpServers": []
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
                        json!({ "cwd": cwd.to_string_lossy(), "mcpServers": [] }),
                    )
                    .await?
            }
        },
        None => {
            client
                .request(
                    "session/new",
                    json!({ "cwd": cwd.to_string_lossy(), "mcpServers": [] }),
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

fn find_program(name: &str) -> Option<PathBuf> {
    let file_names = program_file_names(name);
    for dir in bundled_dirs() {
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
    let mut names = vec![name.to_owned()];
    #[cfg(windows)]
    {
        names.push(format!("{name}.exe"));
        names.push(format!("{name}.cmd"));
        names.push(format!("{name}.bat"));
    }
    names
}

fn bundled_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("WU_AGENT_BIN_DIR") {
        dirs.push(PathBuf::from(dir));
    }
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
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn content_text_reads_blocks() {
        let content = json!([{ "type": "text", "text": "hello " }, { "type": "text", "text": "world" }]);
        assert_eq!(content_text(Some(&content)).as_deref(), Some("hello world"));
    }
}
