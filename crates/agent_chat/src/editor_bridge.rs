//! Bridges the active editor with agent chat sessions: selections and
//! diagnostics can be routed to an agent as a chat prompt.

use crate::chat_view::{ChatView, ComposerRefTarget, DiagnosticCitation, SelectionRef};
use editor::{Editor, MultiBufferOffset};
use gpui::{App, Context, Window};
use i18n::t;
use text::{Point, ToPoint as _};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// Selections are truncated to keep the prompt small; agents can always
/// re-read the file at the given location.
const MAX_SELECTION_CHARS: usize = 8_000;
/// Diagnostic code excerpts are truncated for the same reason.
const MAX_EXCERPT_CHARS: usize = 4_000;
/// Buffer rows of context included around the diagnostics target.
const DIAGNOSTIC_CONTEXT_ROWS: u32 = 8;

/// Opens an agent chat that asks about the first non-empty selection of the
/// active editor. Does nothing when no editor is focused or the selection is
/// empty.
pub fn ask_about_selection(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(prompt) = selection_prompt(workspace, cx) {
        crate::agent_panel::open_chat_with_prompt(workspace, prompt, window, cx);
    }
}

/// Opens an agent chat that asks to fix the diagnostics around the cursor of
/// the active editor. Does nothing when no editor is focused or there are no
/// diagnostics there.
pub fn fix_diagnostics(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(prompt) = diagnostics_prompt(workspace, cx) {
        crate::agent_panel::open_chat_with_prompt(workspace, prompt, window, cx);
    }
}

/// Appends the active editor's selection to the chat the user is looking at.
/// Never starts a new chat: without a visible chat it explains what to do.
pub fn add_selection_to_chat(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(reference) = selection_reference(workspace, cx) else {
        return;
    };
    let Some(chat) = crate::agent_panel::visible_chat(workspace, cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<ChatView>(),
                t!("Open a chat before adding selected code.").to_string(),
            )
            .autohide(),
            cx,
        );
        return;
    };
    chat.update(cx, |chat, cx| {
        chat.insert_composer_reference(ComposerRefTarget::Selection(reference), window, cx)
    });
}

/// Appends diagnostics chosen in the Problems panel to the chat the user is
/// looking at: one chip for a single diagnostic, one summary chip for a group.
/// Never starts a new chat; without a visible chat it explains what to do.
pub fn add_diagnostics_to_chat(
    workspace: &mut Workspace,
    mut citations: Vec<DiagnosticCitation>,
    announce: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if citations.is_empty() {
        return;
    }
    let Some(chat) = crate::agent_panel::visible_chat(workspace, cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<ChatView>(),
                t!("Open a chat before adding diagnostics.").to_string(),
            )
            .autohide(),
            cx,
        );
        return;
    };
    let count = citations.len();
    let target = if citations.len() == 1 {
        ComposerRefTarget::Diagnostic(citations.remove(0))
    } else {
        ComposerRefTarget::Diagnostics(citations)
    };
    chat.update(cx, |chat, cx| {
        chat.insert_composer_reference(target, window, cx)
    });
    if announce {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<ChatView>(),
                t!("Added {count} diagnostics to the chat", count = count).to_string(),
            )
            .autohide(),
            cx,
        );
    }
}

struct Selection {
    path: Option<std::path::PathBuf>,
    start_row: u32,
    end_row: u32,
    language: Option<String>,
    text: String,
}

fn selection(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<Selection> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    editor.update(cx, |editor, cx| {
        let display_map = editor.display_snapshot(cx);
        let selection = editor
            .selections
            .all::<MultiBufferOffset>(&display_map)
            .into_iter()
            .find(|selection| !selection.is_empty())?;
        let multibuffer = editor.buffer().read(cx).snapshot(cx);
        let text: String = multibuffer
            .text_for_range(selection.start..selection.end)
            .collect();
        if text.trim().is_empty() {
            return None;
        }

        let start_row = multibuffer.offset_to_point(selection.start).row + 1;
        let end_row = multibuffer.offset_to_point(selection.end).row + 1;
        let language = multibuffer
            .language_at(selection.head())
            .map(|language| language.name().to_string());
        let file_name = singleton_file_name(editor, cx);
        Some(Selection {
            path: file_name,
            start_row,
            end_row,
            language,
            text,
        })
    })
}

fn selection_prompt(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<String> {
    let selection = selection(workspace, cx)?;
    let mut prompt = String::from("请解释以下选中的代码");
    if let Some(path) = &selection.path {
        prompt.push_str(&format!(
            "（文件 `{}`，第 {}-{} 行）",
            path.display(),
            selection.start_row,
            selection.end_row
        ));
    }
    prompt.push_str("：\n\n");
    if let Some(language) = &selection.language {
        prompt.push_str(&format!("```{language}\n"));
    }
    prompt.push_str(&truncate(&selection.text, MAX_SELECTION_CHARS));
    if selection.language.is_some() {
        prompt.push_str("\n```");
    }
    Some(prompt)
}

/// A cited region for an existing chat: the chat turns it into a chip that
/// points the agent at the file instead of pasting the code.
fn selection_reference(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<SelectionRef> {
    let selection = selection(workspace, cx)?;
    Some(SelectionRef::new(
        selection.path,
        selection.start_row,
        selection.end_row,
        selection.language,
        &selection.text,
    ))
}

fn diagnostics_prompt(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<String> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    editor.update(cx, |editor, cx| {
        let singleton = editor.buffer().read(cx).as_singleton()?;
        let display_map = editor.display_snapshot(cx);
        let selections = editor.selections.all::<MultiBufferOffset>(&display_map);
        let head = selections.first()?.head();
        let multibuffer = editor.buffer().read(cx).snapshot(cx);
        let (buffer_anchor, _) =
            multibuffer.anchor_to_buffer_anchor(multibuffer.anchor_before(head))?;

        let buffer = singleton.read(cx);
        let snapshot = buffer.snapshot();
        let cursor_row = buffer_anchor.to_point(&snapshot).row;
        let mut row_start = cursor_row;
        let mut row_end = cursor_row;
        for selection in selections.iter().filter(|selection| !selection.is_empty()) {
            for offset in [selection.start, selection.end] {
                let Some((buffer_anchor, _)) =
                    multibuffer.anchor_to_buffer_anchor(multibuffer.anchor_before(offset))
                else {
                    continue;
                };
                let row = buffer_anchor.to_point(&snapshot).row;
                row_start = row_start.min(row);
                row_end = row_end.max(row);
            }
        }

        let mut diagnostics: Vec<(u32, _)> = buffer
            .buffer_diagnostics(None)
            .into_iter()
            .filter_map(|entry| {
                let start = entry.range.start.to_point(&snapshot);
                let end = entry.range.end.to_point(&snapshot);
                if start.row <= row_end && end.row >= row_start {
                    Some((start.row, entry))
                } else {
                    None
                }
            })
            .collect();
        if diagnostics.is_empty() {
            return None;
        }
        diagnostics.sort_by_key(|(row, _)| *row);

        let mut lines = Vec::new();
        for (row, entry) in &diagnostics {
            let diagnostic = &entry.diagnostic;
            let severity = match diagnostic.severity {
                lsp::DiagnosticSeverity::ERROR => "错误",
                lsp::DiagnosticSeverity::WARNING => "警告",
                lsp::DiagnosticSeverity::INFORMATION => "提示",
                _ => "建议",
            };
            let code = diagnostic.code.as_ref().map(|code| match code {
                lsp::NumberOrString::Number(number) => format!("（代码 {number}）"),
                lsp::NumberOrString::String(text) => format!("（{text}）"),
            });
            let source = diagnostic
                .source
                .as_deref()
                .map(|source| format!("{source}："));
            lines.push(format!(
                "- 第 {} 行 [{}] {}{}{}",
                *row + 1,
                severity,
                source.unwrap_or_default(),
                diagnostic.message.as_str(),
                code.unwrap_or_default(),
            ));
        }

        let max_row = snapshot.max_point().row;
        let excerpt_start_row = row_start
            .saturating_sub(DIAGNOSTIC_CONTEXT_ROWS)
            .min(max_row);
        let excerpt_end_row = (row_end + DIAGNOSTIC_CONTEXT_ROWS).min(max_row);
        let excerpt: String = snapshot
            .text_for_range(
                Point {
                    row: excerpt_start_row,
                    column: 0,
                }..Point {
                    row: excerpt_end_row,
                    column: snapshot.line_len(excerpt_end_row),
                },
            )
            .collect();

        let file_name = buffer.file().map(|file| file.path().to_string());
        let mut prompt = match &file_name {
            Some(file_name) => format!(
                "请修复文件 `{file_name}` 第 {} 行附近的诊断：\n\n",
                row_start + 1
            ),
            None => format!("请修复第 {} 行附近的诊断：\n\n", row_start + 1),
        };
        prompt.push_str(&lines.join("\n"));
        if !excerpt.trim().is_empty() {
            let language = buffer.language().map(|language| language.name().to_string());
            prompt.push_str("\n\n相关代码：\n\n");
            if let Some(language) = &language {
                prompt.push_str(&format!("```{language}\n"));
            }
            prompt.push_str(&truncate(&excerpt, MAX_EXCERPT_CHARS));
            if language.is_some() {
                prompt.push_str("\n```");
            }
        }
        Some(prompt)
    })
}

fn singleton_file_name(editor: &Editor, cx: &App) -> Option<std::path::PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.path().as_std_path().to_path_buf())
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str("\n…（已截断）");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_excerpt_is_truncated_with_a_marker() {
        let long = "x".repeat(MAX_SELECTION_CHARS + 100);
        let excerpt = truncate(&long, MAX_SELECTION_CHARS);
        assert_eq!(excerpt.chars().count(), MAX_SELECTION_CHARS + "\n…（已截断）".chars().count());
        assert!(excerpt.ends_with("（已截断）"));
        assert_eq!(truncate("short", MAX_SELECTION_CHARS), "short");
    }
}
