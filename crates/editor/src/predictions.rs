//! Inline edit predictions ("ghost text") requested from an OpenAI-compatible
//! endpoint configured through the `edit_predictions` settings.
//!
//! A prediction is requested shortly after typing stops and rendered as an
//! inlay at the cursor. Pressing Tab inserts it; moving the cursor or editing
//! the buffer dismisses it.

use std::sync::Arc;

use anyhow::Result;
use futures::AsyncReadExt as _;
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
    next_id: usize,
}

impl EditPredictionData {
    pub fn new() -> Self {
        Self {
            active: None,
            refresh_task: Task::ready(()),
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
                        response_task = Some(cx.background_spawn(async move {
                            request_prediction(
                                http_client,
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
            let prediction = response_task.await.ok().flatten();
            editor
                .update(cx, |editor, cx| {
                    if !cursor_matches(editor, &request, cx) {
                        return;
                    }
                    if let Some(text) = prediction
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

/// Requests a prediction on the background thread. Returns `None` on any
/// transport, status, or parsing failure.
async fn request_prediction(
    http_client: Arc<dyn HttpClient>,
    endpoint: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    language: Option<&str>,
    file_name: Option<&str>,
    prefix: &str,
    suffix: &str,
) -> Result<Option<String>> {
    let prompt = prediction_prompt(language, file_name, prefix, suffix);
    let request = build_prediction_request(&endpoint, &api_key, &model, max_tokens, &prompt)?;
    let mut response = http_client.send(request).await?;
    if !response.status().is_success() {
        anyhow::bail!("prediction request failed: {}", response.status());
    }
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    Ok(parse_prediction_response(&body))
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
}
