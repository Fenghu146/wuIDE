//! A fake Claude Code CLI for the agent harness tests.
//!
//! Reads the first stream-json user line from stdin, picks a scenario from the
//! prompt text, and plays a scripted stream-json transcript on stdout,
//! including control-channel round-trips read back from stdin. Frame shapes
//! mirror live captures from CLI 2.1.228. Every scenario matches the branch of
//! the same name in `crates/agent_harness/tests/fixtures/fake-claude.sh`, which
//! stays around as the reference.

use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

/// How long a wedged scenario stays alive until the driver tears it down.
const WEDGE: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut session = Session::new();
    let Some(first) = session.read_line() else {
        exit(1);
    };

    // The shell fixture matched with `case "$first" in *pattern*)`, so the order
    // of these checks is load-bearing: `scenario:fork` also matches
    // `scenario:fork-fallback`.
    //
    // Two chat tests need a fake that answers every turn instead of playing one
    // scenario, and the session restarts the process between turns, so they mark
    // the working directory rather than the prompt.
    let code = if Path::new(".args-fixture").is_file() {
        args_turns(&args, &mut session)
    } else if Path::new(".compact-fixture").is_file() {
        compact_turns(&args, &first, &mut session)
    } else if first.contains("scenario:fork-fallback") {
        fork_fallback(&args, &first, &mut session)
    } else if first.contains("scenario:fork") {
        fork(&args, &first, &mut session)
    } else if first.contains("scenario:command-echo") {
        command_echo(&first, &mut session)
    } else if first.contains("scenario:title") {
        title(&args, &mut session)
    } else if first.contains("scenario:happy") {
        happy(&mut session)
    } else if first.contains("scenario:wake") {
        wake(&mut session)
    } else if first.contains("scenario:askuser") {
        ask_user(&mut session)
    } else if first.contains("scenario:mode-change") {
        mode_change(&mut session)
    } else if first.contains("scenario:steer") {
        steer(&mut session)
    } else if first.contains("scenario:images") {
        images(&first, &mut session)
    } else if first.contains("scenario:superseded-steers") {
        superseded_steers(&mut session)
    } else if first.contains("scenario:absorbed-steer") {
        absorbed_steer(&mut session)
    } else if first.contains("scenario:drop") {
        drop_prompt(&args, &first, &mut session)
    } else if first.contains("scenario:interrupt") {
        interrupt(&mut session)
    } else if first.contains("scenario:replay-streaming") {
        replay("live-2.1.291-text-streaming.jsonl", &mut session)
    } else if first.contains("scenario:replay-background") {
        replay("live-2.1.228-background-subagent.jsonl", &mut session)
    } else if first.contains("\"subtype\":\"initialize\"") {
        initialize(&first, &mut session)
    } else if first.contains("scenario:error") {
        error(&mut session)
    } else {
        unknown(&mut session)
    };
    exit(code);
}

struct Session {
    output: BufWriter<io::Stdout>,
    input: BufReader<io::Stdin>,
}

impl Session {
    fn new() -> Self {
        Self {
            output: BufWriter::new(io::stdout()),
            input: BufReader::new(io::stdin()),
        }
    }

    /// Writes one frame and flushes it, so the driver sees the same pacing the
    /// shell fixture's `printf` produced.
    fn emit(&mut self, frame: &str) {
        if writeln!(self.output, "{frame}").is_err() || self.output.flush().is_err() {
            exit(1);
        }
    }

    /// Writes text verbatim, for the captured transcripts the replay modes play.
    fn write_raw(&mut self, text: &str) {
        if self.output.write_all(text.as_bytes()).is_err() || self.output.flush().is_err() {
            exit(1);
        }
    }

    /// One stdin line without its terminator; `None` at end of input.
    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
            Err(error) => {
                eprintln!("fake-claude: could not read stdin: {error}");
                None
            }
        }
    }

    /// The next stdin line that is not a permission-mode change, which can
    /// arrive between any two replies.
    fn read_reply(&mut self) -> Option<String> {
        loop {
            let line = self.read_line()?;
            if !line.contains("\"subtype\":\"set_permission_mode\"") {
                return Some(line);
            }
        }
    }
}

fn emit_init(session: &mut Session, tools: &str, session_id: &str) {
    session.emit(&format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fable-5\",\"tools\":{tools},\"cwd\":\"/tmp\",\"session_id\":\"{session_id}\"}}"
    ));
}

fn emit_text(session: &mut Session, text: &str) {
    session.emit(&format!(
        "{{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}}}"
    ));
}

/// The title turn answers outside a session transcript, so its frames carry no
/// parent tool use id at all.
fn emit_untagged_text(session: &mut Session, text: &str) {
    session.emit(&format!(
        "{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}}}"
    ));
}

/// Mirrors `sed 's/.*"key":"\([^"]*\)".*/\1/'`: the value after the last
/// `"key":"`, or the whole line when the pattern does not appear.
fn field(line: &str, key: &str) -> String {
    let marker = format!("\"{key}\":\"");
    let Some(start) = line.rfind(&marker) else {
        return line.to_string();
    };
    let rest = &line[start + marker.len()..];
    match rest.find('"') {
        Some(end) => rest[..end].to_string(),
        None => line.to_string(),
    }
}

fn fork_fallback(args: &[String], first: &str, session: &mut Session) -> i32 {
    if args.iter().any(|arg| arg == "--fork-session") {
        return 1;
    }
    if !first.contains("<conversation>") {
        return 1;
    }
    emit_init(session, "[]", "sess-fallback");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"from the history\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-fallback\"}");
    0
}

fn fork(args: &[String], first: &str, session: &mut Session) -> i32 {
    let forked = args.iter().any(|arg| arg == "--fork-session");
    let resumed = args.iter().any(|arg| arg == "--resume=sess-1");
    if !forked || !resumed || first.contains("<conversation>") {
        return 1;
    }
    emit_init(session, "[]", "sess-forked");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"forked\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-forked\"}");
    0
}

fn command_echo(first: &str, session: &mut Session) -> i32 {
    let content = field(first, "content");
    session.emit(&format!(
        "{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{content}\"}}}}}}"
    ));
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"echoed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-command-echo\"}");
    0
}

fn title(args: &[String], session: &mut Session) -> i32 {
    let mut tools_off = false;
    let mut system_set = false;
    let mut mcp_off = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--tools" => {
                index += 1;
                if args.get(index).is_none_or(String::is_empty) {
                    tools_off = true;
                }
            }
            "--system-prompt" => {
                index += 1;
                if args
                    .get(index)
                    .is_some_and(|prompt| prompt.starts_with("You generate session titles."))
                {
                    system_set = true;
                }
            }
            "--strict-mcp-config" => mcp_off = true,
            "--dangerously-skip-permissions" => return 1,
            _ => {}
        }
        index += 1;
    }
    if !tools_off || !system_set || !mcp_off {
        return 1;
    }
    session.emit("{\"type\":\"control_request\",\"request_id\":\"title-tool\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{\"command\":\"touch should-not-exist\"}}}");
    let Some(response) = session.read_line() else {
        return 1;
    };
    if !response.contains("\"behavior\":\"deny\"") {
        return 1;
    }
    emit_untagged_text(session, "Fix Login Flow");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Fix Login Flow\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-title\"}");
    0
}

fn happy(session: &mut Session) -> i32 {
    let frames = [
        "{\"type\":\"system\",\"subtype\":\"thinking_tokens\",\"tokens\":12}",
        "{\"type\":\"system\",\"subtype\":\"session_state_changed\",\"state\":\"running\"}",
        "{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"pondering\"}}}",
        "{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}}",
        "{\"type\":\"stream_event\",\"parent_tool_use_id\":\"sub-1\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"SUBAGENT\"}}}",
        "{\"type\":\"assistant\",\"parent_tool_use_id\":\"sub-1\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"sub-tool\",\"name\":\"Bash\",\"input\":{\"command\":\"echo sub\"}}]}}",
        "{\"type\":\"user\",\"parent_tool_use_id\":\"sub-1\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"sub-tool\",\"is_error\":false}]}}",
        "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello\"},{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls -la\"}},{\"type\":\"tool_use\",\"id\":\"tool-2\",\"name\":\"mcp__linear__search\",\"input\":{\"q\":\"bug\"}}]}}",
        "{\"type\":\"user\",\"parent_tool_use_id\":null,\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"is_error\":false},{\"type\":\"tool_result\",\"tool_use_id\":\"tool-2\",\"is_error\":true}]}}",
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\"}}",
        "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done!\",\"errors\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":20},\"session_id\":\"sess-1\",\"total_cost_usd\":0.01}",
    ];
    // A re-emitted init mid-run (background-task wakeup) must be deduped.
    emit_init(session, "[\"Bash\",\"Read\"]", "sess-1");
    emit_init(session, "[\"Bash\",\"Read\"]", "sess-1");
    for frame in frames {
        session.emit(frame);
    }
    0
}

fn wake(session: &mut Session) -> i32 {
    // Eager-done + wake, the live-verified 2.1.228 background-subagent shape:
    // the parent turn settles with result #1 while the subagent still runs;
    // tagged subagent traffic continues; the CLI then wakes with a second init
    // (same session id) and settles the wake turn with result #2.
    let frames = [
        "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_agent\",\"name\":\"Agent\",\"input\":{\"description\":\"background task\",\"run_in_background\":true}}]}}",
        "{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"LAUNCHED\"}}}",
        "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"LAUNCHED\",\"errors\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":5},\"session_id\":\"sess-wake\"}",
        "{\"type\":\"stream_event\",\"parent_tool_use_id\":\"toolu_agent\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"sub working\"}}}",
        "{\"type\":\"assistant\",\"parent_tool_use_id\":\"toolu_agent\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"sub-t1\",\"name\":\"Bash\",\"input\":{\"command\":\"sleep 1\"}}]}}",
        "{\"type\":\"user\",\"parent_tool_use_id\":\"toolu_agent\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"sub-t1\",\"is_error\":false}]}}",
    ];
    emit_init(session, "[\"Bash\"]", "sess-wake");
    for frame in frames {
        session.emit(frame);
    }
    emit_init(session, "[\"Bash\"]", "sess-wake");
    emit_text(session, "subagent finished");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"wrapped up\",\"errors\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":3},\"session_id\":\"sess-wake\"}");
    0
}

fn ask_user(session: &mut Session) -> i32 {
    emit_init(session, "[\"Bash\"]", "sess-ask");
    // A plain tool permission request: must be auto-allowed.
    session.emit("{\"type\":\"control_request\",\"request_id\":\"cr-0\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{\"command\":\"ls\"}}}");
    let Some(reply) = session.read_reply() else {
        return 1;
    };
    if !(reply.contains("\"request_id\":\"cr-0\"") && reply.contains("\"behavior\":\"allow\"")) {
        session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"bash tool was not allowed\"],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-ask\"}");
        return 0;
    }
    // AskUserQuestion: must be intercepted and answered via updatedInput.answers.
    session.emit("{\"type\":\"control_request\",\"request_id\":\"cr-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"AskUserQuestion\",\"input\":{\"questions\":[{\"header\":\"Choice\",\"question\":\"Pick one\",\"options\":[\"A\",\"B\"],\"multiSelect\":false}]}}}");
    let Some(reply) = session.read_reply() else {
        return 1;
    };
    if !reply.contains("\"behavior\":\"allow\"") {
        session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"AskUserQuestion was denied\"],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-ask\"}");
    } else if reply.contains("\"Pick one\":\"B\"") {
        session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"answered\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-ask\"}");
    } else {
        session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"answers missing from updatedInput\"],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-ask\"}");
    }
    0
}

fn mode_change(session: &mut Session) -> i32 {
    emit_init(session, "[\"Bash\"]", "sess-mode");
    let Some(change) = session.read_line() else {
        return 1;
    };
    if !(change.contains("\"subtype\":\"set_permission_mode\"")
        && change.contains("\"mode\":\"default\""))
    {
        session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"no mode change\"],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-mode\"}");
        return 0;
    }
    session.emit("{\"type\":\"control_request\",\"request_id\":\"cr-0\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{\"command\":\"ls\"}}}");
    let Some(reply) = session.read_reply() else {
        return 1;
    };
    if reply.contains("\"request_id\":\"cr-0\"") && reply.contains("\"behavior\":\"allow\"") {
        session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"allowed\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-mode\"}");
    } else {
        session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"bash tool was not allowed\"],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-mode\"}");
    }
    0
}

fn steer(session: &mut Session) -> i32 {
    emit_init(session, "[]", "sess-steer");
    emit_text(session, "first");
    // The queued steering user line, applied at "the step boundary" (here: now).
    let Some(steer) = session.read_line() else {
        return 1;
    };
    if !steer.contains("\"priority\":\"now\"") {
        return 9;
    }
    // Old output continues until Claude actually consumes the input.
    emit_text(session, "-still-first");
    session.emit(&steer);
    let content = field(&steer, "content");
    emit_text(session, &format!("steered:{content}"));
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"steered\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-steer\"}");
    0
}

fn images(first: &str, session: &mut Session) -> i32 {
    emit_init(session, "[]", "sess-img");
    if first.matches("\"type\":\"image\"").count() != 1 {
        return 9;
    }
    for want in [
        "\"type\":\"base64\"",
        "\"media_type\":\"image/png\"",
        "\"data\":\"aGVsbG8=\"",
        "\"type\":\"text\"",
    ] {
        if !first.contains(want) {
            return 9;
        }
    }
    emit_text(session, "first-image-ok");
    let Some(steer) = session.read_line() else {
        return 1;
    };
    for want in [
        "\"type\":\"image\"",
        "\"media_type\":\"image/jpeg\"",
        "\"data\":\"d29ybGQ=\"",
        "\"type\":\"text\"",
    ] {
        if !steer.contains(want) {
            return 9;
        }
    }
    session.emit(&steer);
    emit_text(session, "steer-image-ok");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"images\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-img\"}");
    0
}

fn superseded_steers(session: &mut Session) -> i32 {
    // CLI 2.1.280 with rapid `now` steers: the second interrupts the turn the
    // first started before it is replayed; only the last steer is replayed.
    emit_init(session, "[]", "sess-sup");
    emit_text(session, "story");
    let Some(_steer1) = session.read_line() else {
        return 1;
    };
    let Some(steer2) = session.read_line() else {
        return 1;
    };
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"story\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-sup\"}");
    emit_init(session, "[]", "sess-sup");
    session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-sup\"}");
    emit_init(session, "[]", "sess-sup");
    session.emit(&steer2);
    emit_text(session, "answered-both");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"answered-both\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-sup\"}");
    0
}

fn absorbed_steer(session: &mut Session) -> i32 {
    // A steer whose replay never comes: the turn end must not be held forever.
    emit_init(session, "[]", "sess-abs");
    let Some(_steer) = session.read_line() else {
        return 1;
    };
    emit_text(session, "absorbed");
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"absorbed\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-abs\"}");
    // Stay alive so the driver, not the turn end, decides when the process goes.
    sleep(WEDGE);
    0
}

fn drop_prompt(args: &[String], first: &str, session: &mut Session) -> i32 {
    let uuid = field(first, "uuid");
    let mut resumed = String::from("fresh");
    for arg in args {
        if let Some(point) = arg.strip_prefix("--resume-session-at=") {
            resumed = format!("at {point}");
        } else if arg.starts_with("--resume=") && resumed == "fresh" {
            resumed = String::from("whole");
        }
    }
    emit_init(session, "[]", "sess-drop");
    session.emit(&format!(
        "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"parent_tool_use_id\":null,\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"scenario:drop\"}}]}}}}"
    ));
    session.emit(&format!(
        "{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{resumed}\"}}}}}}"
    ));
    session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-drop\"}");
    0
}

fn interrupt(session: &mut Session) -> i32 {
    emit_init(session, "[]", "sess-int");
    // Wedge without reading stdin to force the SIGTERM escalation path.
    sleep(WEDGE);
    0
}

fn initialize(first: &str, session: &mut Session) -> i32 {
    let request_id = field(first, "request_id");
    let fixture = Path::new(".command-fixture");
    if fixture.is_file() {
        let name = match fs::read_to_string(fixture) {
            Ok(contents) => contents.trim_end_matches(['\r', '\n']).to_string(),
            Err(error) => {
                eprintln!("fake-claude: could not read {}: {error}", fixture.display());
                return 1;
            }
        };
        session.emit(&format!(
            "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"{request_id}\",\"response\":{{\"commands\":[{{\"name\":\"{name}\"}}]}}}}}}"
        ));
        sleep(WEDGE);
        return 0;
    }
    // Command discovery: the initialize control request arrives as the FIRST
    // stdin line (no user message ever follows). Shape mirrors 2.1.228's
    // control_response: commands under response.response.
    session.emit(&format!(
        "{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"{request_id}\",\"response\":{{\"commands\":[{{\"name\":\"review\",\"description\":\"Review a pull request\",\"argumentHint\":\"[pr number]\"}},{{\"name\":\"compact\",\"description\":\"Compact the conversation\",\"argumentHint\":\"\"}},{{\"name\":\"usage\",\"description\":\"Show plan usage\"}},{{\"name\":\"model\",\"description\":\"Set the AI model\"}},{{\"name\":\"\",\"description\":\"nameless: dropped\"}}],\"output_style\":\"default\"}}}}}}"
    ));
    // Stay alive until the driver tears us down, like the real CLI would.
    sleep(WEDGE);
    0
}

fn error(session: &mut Session) -> i32 {
    let frames = [
        // Terse assistant-level error code with no content.
        "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[]},\"error\":\"rate_limit\"}",
        // Hard-rejected claude.ai usage window.
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"five_hour\"}}",
        // Result error with an EMPTY errors array: needs fallback wording.
        "{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2},\"session_id\":\"sess-err\"}",
    ];
    emit_init(session, "[]", "sess-err");
    for frame in frames {
        session.emit(frame);
    }
    0
}

/// Replacement for the `echo-args-claude` script one chat test used to write
/// into its temporary directory: every turn answers with the argv the CLI was
/// launched with.
fn args_turns(args: &[String], session: &mut Session) -> i32 {
    let arguments = args.join(" ");
    session.emit("{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m\",\"tools\":[],\"cwd\":\"/tmp\",\"session_id\":\"sess-args\"}");
    // The first prompt was already read by `main`, so each pass answers one turn.
    loop {
        emit_text(session, &arguments);
        session.emit(
            "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[]}}",
        );
        session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-args\"}");
        if session.read_line().is_none() {
            return 0;
        }
    }
}

/// Replacement for the `steer-aware-claude` script one chat test used to write
/// into its temporary directory: it reports compaction frames and whether the
/// turn started fresh or resumed.
fn compact_turns(args: &[String], first: &str, session: &mut Session) -> i32 {
    let run = if args.iter().any(|arg| arg.contains("--resume")) {
        "resumed"
    } else {
        "new"
    };
    session.emit("{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m\",\"tools\":[],\"cwd\":\"/tmp\",\"session_id\":\"sess-commands\"}");
    let mut line = first.to_string();
    loop {
        let kind = if line.contains("priority") {
            "steer"
        } else {
            "first"
        };
        if line.contains("\"/compact") {
            session.emit("{\"type\":\"system\",\"subtype\":\"status\",\"status\":\"compacting\",\"session_id\":\"sess-commands\"}");
            session.emit("{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"session_id\":\"sess-commands\",\"compact_metadata\":{\"trigger\":\"manual\",\"pre_tokens\":900,\"post_tokens\":120}}");
            session.emit("{\"type\":\"system\",\"subtype\":\"status\",\"status\":null,\"compact_result\":\"success\",\"session_id\":\"sess-commands\"}");
        } else if line.contains("\"long task") {
            session.emit("{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"session_id\":\"sess-commands\",\"compact_metadata\":{\"trigger\":\"auto\",\"pre_tokens\":900,\"post_tokens\":150}}");
        } else if line.contains("\"doomed") {
            session.emit("{\"type\":\"system\",\"subtype\":\"status\",\"status\":\"compacting\",\"session_id\":\"sess-commands\"}");
            session.emit("{\"type\":\"system\",\"subtype\":\"status\",\"status\":null,\"compact_result\":\"failed\",\"compact_error\":\"prompt_too_long\",\"session_id\":\"sess-commands\"}");
        }
        emit_text(session, &format!("{run} {kind}"));
        session.emit(
            "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[]}}",
        );
        session.emit("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-commands\"}");
        let Some(next) = session.read_line() else {
            return 0;
        };
        line = next;
    }
}

/// Plays a captured live transcript, the job of the `cat` replayer scripts the
/// harness tests used to write.
fn replay(name: &str, session: &mut Session) -> i32 {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agent_harness/tests/fixtures/claude")
        .join(name);
    match fs::read_to_string(&path) {
        Ok(frames) => {
            session.write_raw(&frames);
            0
        }
        Err(error) => {
            eprintln!("fake-claude: could not read {}: {error}", path.display());
            1
        }
    }
}

fn unknown(session: &mut Session) -> i32 {
    session.emit("{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"errors\":[\"unknown scenario\"],\"usage\":{\"input_tokens\":0,\"output_tokens\":0},\"session_id\":\"sess-x\"}");
    0
}
