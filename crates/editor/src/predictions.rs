//! Inline edit predictions ("ghost text") requested from an OpenAI-compatible
//! endpoint configured through the `edit_predictions` settings.
//!
//! A prediction is requested shortly after typing stops and rendered as an
//! inlay at the cursor. Pressing Tab inserts it; moving the cursor or editing
//! the buffer dismisses it.

use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::AsyncReadExt as _;
use futures::future::{self, Either};
use gpui::{App, AppContext as _, Context, Task};
use http_client::{AsyncBody, HttpClient, Method, Request};
use multi_buffer::{MultiBufferRow, MultiBufferSnapshot};
use serde_json::json;
use settings::Settings as _;
use text::{Point, ToOffset as _};

use crate::{Editor, EditorSettings, MultiBufferOffset, inlays::Inlay};

/// Maximum number of characters of buffer context sent on each side of the
/// cursor. Kept small so requests stay fast and cheap.
const MAX_CONTEXT_CHARS: usize = 4_000;

/// Total time a prediction request may take, connection and body included.
///
/// The HTTP client's own connect timeout only covers the handshake: a hosted
/// endpoint can accept the request and then stream for much longer than that.
/// 20 s is long enough for a cold model or a large context and short enough
/// that a stuck request cannot hold a background task for minutes. A prediction
/// that arrives later is stale anyway, so timing out costs nothing.
const PREDICTION_TIMEOUT: Duration = Duration::from_secs(20);

/// Delay before the first retry after a failed request; every further failure
/// doubles it, up to [`MAX_RETRY_DELAY`].
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Ceiling for the retry delay.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How long to wait after `consecutive_failures` failed requests in a row:
/// 1 s, 2 s, 4 s, and so on, capped at [`MAX_RETRY_DELAY`].
fn retry_delay(consecutive_failures: u32) -> Duration {
    // Doubling past 32 s is pointless because it is capped at 30 s, and the
    // shift is bounded so a long outage cannot overflow it.
    let doublings = consecutive_failures.saturating_sub(1).min(5);
    INITIAL_RETRY_DELAY
        .saturating_mul(1 << doublings)
        .min(MAX_RETRY_DELAY)
}

/// Consecutive prediction failures and the earliest time the next request may
/// start.
///
/// A failing endpoint must not be asked again on every keystroke, so prompting
/// stays silent during the backoff window. Any answered request, including one
/// whose completion is empty, clears it.
#[derive(Default)]
struct PredictionRetry {
    consecutive_failures: u32,
    next_attempt: Option<Instant>,
}

impl PredictionRetry {
    fn ready(&self, now: Instant) -> bool {
        self.next_attempt
            .is_none_or(|next_attempt| now >= next_attempt)
    }

    fn record_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.next_attempt = Some(now + retry_delay(self.consecutive_failures));
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.next_attempt = None;
    }
}

/// Cursor context kept while a request is in flight; a response is only
/// rendered when the editor still sits at exactly this position.
struct PredictionRequest {
    cursor_offset: MultiBufferOffset,
}

struct ActivePrediction {
    id: usize,
    cursor_offset: MultiBufferOffset,
    text: String,
}

pub struct EditPredictionData {
    active: Option<ActivePrediction>,
    refresh_task: Task<()>,
    retry: PredictionRetry,
    next_id: usize,
}

impl EditPredictionData {
    pub fn new() -> Self {
        Self {
            active: None,
            refresh_task: Task::ready(()),
            retry: PredictionRetry::default(),
            next_id: 0,
        }
    }
}

impl Editor {
    /// Clears the active prediction when the cursor has moved away from it.
    pub(crate) fn dismiss_stale_edit_prediction(&mut self, cx: &mut Context<Self>) {
        let Some(prediction) = self.edit_predictions.active.as_ref() else {
            return;
        };
        let display_map = self.display_snapshot(cx);
        if prediction_cursor(self, &display_map) != Some(prediction.cursor_offset) {
            self.clear_edit_prediction(cx);
        }
    }

    /// Dismisses the current prediction and schedules a new request after the
    /// configured debounce.
    pub(crate) fn refresh_edit_predictions(&mut self, cx: &mut Context<Self>) {
        self.clear_edit_prediction(cx);

        let settings = EditorSettings::get_global(cx).edit_predictions.clone();
        if !settings.enabled || settings.endpoint.is_empty() || self.read_only(cx) {
            return;
        }

        // A failing endpoint is left alone until its backoff expires, so typing
        // through an outage does not fire a request on every keystroke.
        if !self.edit_predictions.retry.ready(Instant::now()) {
            return;
        }

        // Predictions only target regular singleton editors; skip multi-buffers
        // like project search results.
        let Some(buffer) = self.buffer().read(cx).as_singleton() else {
            return;
        };

        let display_map = self.display_snapshot(cx);
        let cursor_offset = match prediction_cursor(self, &display_map) {
            Some(offset) => offset,
            None => return,
        };

        // Only predict while typing at the end of a line, where an inline
        // continuation is meaningful and rendering stays simple.
        let multi_buffer_snapshot = self.buffer().read(cx).snapshot(cx);
        if !is_at_line_end(&multi_buffer_snapshot, cursor_offset) {
            return;
        }

        let buffer_ref = buffer.read(cx);
        let buffer_snapshot = buffer_ref.snapshot();
        let Some((buffer_cursor, _)) = multi_buffer_snapshot
            .anchor_to_buffer_anchor(multi_buffer_snapshot.anchor_before(cursor_offset))
        else {
            return;
        };
        let buffer_cursor_offset = buffer_cursor.to_offset(&buffer_snapshot);
        let prefix = excerpt_before(&buffer_snapshot, buffer_cursor_offset, MAX_CONTEXT_CHARS);
        let suffix = excerpt_after(&buffer_snapshot, buffer_cursor_offset, MAX_CONTEXT_CHARS);
        if prefix.trim().is_empty() && suffix.trim().is_empty() {
            return;
        }

        let request = PredictionRequest {
            cursor_offset,
        };
        let http_client = cx.http_client();
        let endpoint = settings.endpoint.clone();
        let api_key = settings.api_key.clone();
        let model = settings.model.clone();
        let max_tokens = settings.max_tokens;
        let debounce = settings.debounce.0;
        let language = buffer_ref
            .language()
            .map(|language| language.name().to_string());
        let file_name = buffer_ref
            .file()
            .map(|file| file.file_name(cx).to_string());

        self.edit_predictions.refresh_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(debounce))
                .await;

            let mut response_task = None;
            editor
                .update(cx, |editor, cx| {
                    if cursor_matches(editor, &request, cx) {
                        // The timeout covers the whole exchange: a server that
                        // accepts the connection and then stalls must not keep
                        // this task alive.
                        let timeout = cx.background_executor().timer(PREDICTION_TIMEOUT);
                        response_task = Some(cx.background_spawn(async move {
                            request_prediction(
                                http_client,
                                timeout,
                                endpoint.to_string(),
                                api_key.to_string(),
                                model.to_string(),
                                max_tokens,
                                language.as_deref(),
                                file_name.as_deref(),
                                &prefix,
                                &suffix,
                            )
                            .await
                        }));
                    }
                })
                .ok();

            let Some(response_task) = response_task else {
                return;
            };
            let outcome = response_task.await;
            editor
                .update(cx, |editor, cx| {
                    match &outcome {
                        Ok(_) => editor.edit_predictions.retry.record_success(),
                        Err(_) => editor.edit_predictions.retry.record_failure(Instant::now()),
                    }
                    if !cursor_matches(editor, &request, cx) {
                        return;
                    }
                    if let Ok(Some(text)) = outcome
                        && !text.trim().is_empty()
                    {
                        editor.show_edit_prediction(request, text, cx);
                    }
                })
                .ok();
        });
    }

    /// Renders the prediction as an inlay at the request position.
    fn show_edit_prediction(
        &mut self,
        request: PredictionRequest,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.clear_edit_prediction(cx);
        let snapshot = self.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_after(request.cursor_offset);
        let id = post_inc(&mut self.edit_predictions.next_id);
        self.edit_predictions.active = Some(ActivePrediction {
            id,
            cursor_offset: request.cursor_offset,
            text: text.clone(),
        });
        let inlay = Inlay::prediction(id, anchor, text);
        self.splice_inlays(&[], vec![inlay], cx);
    }

    /// Removes the rendered prediction without inserting it.
    pub(crate) fn clear_edit_prediction(&mut self, cx: &mut Context<Self>) {
        if let Some(prediction) = self.edit_predictions.active.take() {
            self.splice_inlays(&[project::InlayId::Prediction(prediction.id)], vec![], cx);
        }
    }

    /// Inserts the predicted text at the cursor. Returns whether a
    /// prediction was accepted.
    pub fn accept_edit_prediction(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(prediction) = self.edit_predictions.active.take() else {
            return false;
        };
        self.splice_inlays(&[project::InlayId::Prediction(prediction.id)], vec![], cx);
        let anchor = self
            .buffer()
            .read(cx)
            .snapshot(cx)
            .anchor_after(prediction.cursor_offset);
        self.buffer().update(cx, |multi_buffer, cx| {
            multi_buffer.edit([(anchor..anchor, prediction.text)], None, cx);
        });
        true
    }
}

fn post_inc(counter: &mut usize) -> usize {
    let id = *counter;
    *counter += 1;
    id
}

fn prediction_cursor(
    editor: &Editor,
    display_map: &crate::DisplaySnapshot,
) -> Option<MultiBufferOffset> {
    editor
        .selections
        .all::<MultiBufferOffset>(display_map)
        .first()
        .map(|selection| selection.head())
}

fn cursor_matches(editor: &Editor, request: &PredictionRequest, cx: &mut App) -> bool {
    let display_map = editor.display_snapshot(cx);
    prediction_cursor(editor, &display_map) == Some(request.cursor_offset)
}

fn is_at_line_end(snapshot: &MultiBufferSnapshot, offset: MultiBufferOffset) -> bool {
    let point = snapshot.offset_to_point(offset);
    let line_end = Point {
        row: point.row,
        column: snapshot.line_len(MultiBufferRow(point.row)),
    };
    snapshot.point_to_offset(line_end) == offset
}

fn excerpt_before(snapshot: &text::BufferSnapshot, offset: usize, max_chars: usize) -> String {
    let start = offset.saturating_sub(max_chars);
    snapshot
        .text_for_range(start..offset)
        .collect::<String>()
}

fn excerpt_after(snapshot: &text::BufferSnapshot, offset: usize, max_chars: usize) -> String {
    let end = (offset + max_chars).min(snapshot.len());
    snapshot.text_for_range(offset..end).collect::<String>()
}

fn prediction_prompt(
    language: Option<&str>,
    file_name: Option<&str>,
    prefix: &str,
    suffix: &str,
) -> String {
    let mut prompt = String::from("Continue the code at the cursor position. ");
    if let Some(language) = language {
        prompt.push_str(&format!("The file is written in {language}. "));
    }
    if let Some(file_name) = file_name {
        prompt.push_str(&format!("The file is named `{file_name}`. "));
    }
    prompt.push_str(
        "Reply with only the raw continuation text for the cursor position: no explanations, \
        no markdown code fences, and do not repeat the given code.",
    );
    prompt.push_str("\n\n<prefix>\n");
    prompt.push_str(prefix);
    prompt.push_str("\n</prefix>\n\n<cursor/>\n\n<suffix>\n");
    prompt.push_str(suffix);
    prompt.push_str("\n</suffix>");
    prompt
}

fn prediction_request_body(model: &str, max_tokens: u32, prompt: &str) -> String {
    json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a code completion engine. You continue code exactly at a cursor position, replying with raw continuation text only."
            },
            {
                "role": "user",
                "content": prompt,
            }
        ],
        "max_tokens": max_tokens,
        "temperature": 0.2,
        "stream": false,
    })
    .to_string()
}

fn build_prediction_request(
    endpoint: &str,
    api_key: &str,
    model: &str,
    max_tokens: u32,
    prompt: &str,
) -> Result<Request<AsyncBody>> {
    let body = prediction_request_body(model, max_tokens, prompt);

    let mut request = Request::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header("Content-Type", "application/json");
    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }
    Ok(request.body(AsyncBody::from(body))?)
}

/// Why a prediction request produced no text. Logged as a coarse category,
/// because the transport error can quote the endpoint, which may carry an API
/// key in its query.
#[derive(Debug)]
enum PredictionFailure {
    /// The endpoint setting is not a usable URL.
    InvalidEndpoint,
    /// The request could not be sent or the connection failed.
    Transport,
    /// The endpoint answered, but not with a success status.
    Status(u16),
    /// The response body could not be read or was not an answer.
    Body,
    /// No answer arrived within [`PREDICTION_TIMEOUT`].
    Timeout,
}

impl std::fmt::Display for PredictionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpoint => f.write_str("invalid endpoint"),
            Self::Transport => f.write_str("transport error"),
            Self::Status(status) => write!(f, "HTTP status {status}"),
            Self::Body => f.write_str("unreadable response body"),
            Self::Timeout => f.write_str("timed out"),
        }
    }
}

/// The endpoint's host, the only part that is safe to log: the URL may carry
/// an API key in its query, and its path is none of the log's business.
fn endpoint_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unparsed endpoint>".to_owned())
}

/// Requests a prediction on the background thread, giving up after `timeout`.
/// Returns `None` when the endpoint answered without any text to insert.
///
/// Failures are only logged, never surfaced in the editor: a missing prediction
/// is invisible by design, and this task runs while the user keeps typing.
async fn request_prediction(
    http_client: Arc<dyn HttpClient>,
    timeout: Task<()>,
    endpoint: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    language: Option<&str>,
    file_name: Option<&str>,
    prefix: &str,
    suffix: &str,
) -> Result<Option<String>> {
    let host = endpoint_host(&endpoint);
    let attempt = async {
        let prompt = prediction_prompt(language, file_name, prefix, suffix);
        let request = build_prediction_request(&endpoint, &api_key, &model, max_tokens, &prompt)
            .map_err(|_| PredictionFailure::InvalidEndpoint)?;
        let mut response = http_client
            .send(request)
            .await
            .map_err(|_| PredictionFailure::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(PredictionFailure::Status(status.as_u16()));
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .map_err(|_| PredictionFailure::Body)?;
        Ok(parse_prediction_response(&body))
    };

    let attempt = pin!(attempt);
    let timeout = pin!(timeout);
    let outcome = match future::select(attempt, timeout).await {
        Either::Left((outcome, _)) => outcome,
        Either::Right(((), _)) => Err(PredictionFailure::Timeout),
    };
    match outcome {
        Ok(prediction) => Ok(prediction),
        Err(failure) => {
            log::warn!("edit prediction request to {host} failed: {failure}");
            Err(anyhow::anyhow!("{failure}"))
        }
    }
}

/// Extracts the completion text from an OpenAI-compatible chat response.
fn parse_prediction_response(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let text = value
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?
        .to_string();
    let text = strip_markdown_fence(&text);
    let text = text.trim_start_matches('\n');
    let text = text.trim_end();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Models sometimes wrap their answer in a markdown fence despite the
/// instructions; unwrap it so the text can be inserted verbatim.
fn strip_markdown_fence(text: &str) -> String {
    let trimmed = text.trim_start();
    let Some(after_opening) = trimmed.strip_prefix("```") else {
        return text.to_string();
    };
    // Skip the optional language tag on the opening fence.
    let Some(opening_line_end) = after_opening.find('\n').map(|index| index + 1) else {
        // A fence with no content after it is not a continuation.
        return String::new();
    };
    let content = &after_opening[opening_line_end..];
    match content.strip_suffix("```") {
        Some(stripped) => stripped.to_string(),
        None => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content: &str) -> Vec<u8> {
        serde_json::json!({
            "choices": [{ "message": { "content": content } }]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn parses_plain_continuation() {
        assert_eq!(
            parse_prediction_response(&response("let x = 1;")),
            Some("let x = 1;".to_string())
        );
    }

    #[test]
    fn strips_fenced_continuation() {
        assert_eq!(
            parse_prediction_response(&response("```rust\nlet x = 1;\n```")),
            Some("let x = 1;".to_string())
        );
    }

    #[test]
    fn rejects_empty_predictions() {
        assert_eq!(parse_prediction_response(&response("  \n")), None);
        assert_eq!(parse_prediction_response(&response("```")), None);
    }

    #[test]
    fn keeps_prediction_with_leading_indentation() {
        assert_eq!(
            parse_prediction_response(&response("    .map(|x| x + 1)")),
            Some("    .map(|x| x + 1)".to_string())
        );
    }

    #[test]
    fn builds_request_with_auth_header() {
        let request = build_prediction_request(
            "https://api.deepseek.com/chat/completions",
            "sk-test",
            "deepseek-chat",
            176,
            "continue",
        )
        .unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(
            request.headers().get("Authorization").unwrap(),
            "Bearer sk-test"
        );
        let body: serde_json::Value =
            serde_json::from_str(&prediction_request_body("deepseek-chat", 176, "continue"))
                .unwrap();
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["max_tokens"], 176);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn builds_request_without_auth_header_for_local_servers() {
        let request = build_prediction_request(
            "http://localhost:11434/v1/chat/completions",
            "",
            "",
            96,
            "c",
        )
        .unwrap();
        assert!(request.headers().get("Authorization").is_none());
    }

    #[test]
    fn prompt_contains_marker_delimiters() {
        let prompt = prediction_prompt(Some("Rust"), Some("main.rs"), "fn main() {", "}");
        assert!(prompt.contains("written in Rust"));
        assert!(prompt.contains("named `main.rs`"));
        assert!(prompt.contains("<prefix>\nfn main() {"));
        assert!(prompt.contains("<cursor/>"));
        assert!(prompt.ends_with("</suffix>"));
    }

    #[test]
    fn retry_delays_double_until_the_thirty_second_cap() {
        let delays: Vec<u64> = (1..=8)
            .map(|failures| retry_delay(failures).as_secs())
            .collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn backoff_holds_requests_until_it_expires_and_success_resets_it() {
        let start = Instant::now();
        let mut retry = PredictionRetry::default();
        assert!(retry.ready(start), "the first request is never delayed");

        retry.record_failure(start);
        assert!(!retry.ready(start + Duration::from_millis(999)));
        let second_attempt = start + INITIAL_RETRY_DELAY;
        assert!(retry.ready(second_attempt));

        retry.record_failure(second_attempt);
        assert!(
            !retry.ready(second_attempt + Duration::from_millis(1_999)),
            "the second failure waits twice as long"
        );
        assert!(retry.ready(second_attempt + Duration::from_secs(2)));

        retry.record_success();
        assert!(retry.ready(second_attempt), "success clears the backoff");
        retry.record_failure(second_attempt);
        assert_eq!(
            retry.next_attempt,
            Some(second_attempt + INITIAL_RETRY_DELAY),
            "success restarts the doubling from 1 s"
        );
    }

    #[test]
    fn logged_endpoint_is_only_the_host() {
        assert_eq!(
            endpoint_host("https://api.deepseek.com/v1/chat/completions?api_key=sk-secret"),
            "api.deepseek.com"
        );
        assert_eq!(
            endpoint_host("http://localhost:11434/v1/chat/completions"),
            "localhost"
        );
        assert_eq!(endpoint_host("not a url"), "<unparsed endpoint>");
    }

    /// Answers nothing, standing in for an endpoint that accepted the request
    /// and then never replied.
    struct HangingHttpClient;

    impl HttpClient for HangingHttpClient {
        fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&http_client::Url> {
            None
        }

        fn send(
            &self,
            _request: Request<AsyncBody>,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<http_client::Response<AsyncBody>>>
        {
            Box::pin(future::pending())
        }
    }

    #[gpui::test]
    async fn a_hanging_prediction_request_times_out(cx: &mut gpui::TestAppContext) {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let did_finish = finished.clone();
        let timeout = cx.background_executor.timer(PREDICTION_TIMEOUT);
        let task = cx.background_executor.spawn(async move {
            let outcome = request_prediction(
                Arc::new(HangingHttpClient),
                timeout,
                "http://localhost:11434/v1/chat/completions".to_string(),
                String::new(),
                "local-model".to_string(),
                176,
                None,
                None,
                "fn main() {",
                "}",
            )
            .await;
            did_finish.store(true, std::sync::atomic::Ordering::SeqCst);
            outcome
        });

        cx.background_executor.run_until_parked();
        cx.background_executor
            .advance_clock(PREDICTION_TIMEOUT - Duration::from_secs(1));
        cx.background_executor.run_until_parked();
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "a request must not be abandoned before the configured timeout"
        );

        cx.background_executor.advance_clock(Duration::from_secs(1));
        cx.background_executor.run_until_parked();
        assert!(
            task.await.is_err(),
            "a stalled endpoint must end as an ordinary failure"
        );
    }
}
