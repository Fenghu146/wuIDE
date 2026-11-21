//! Compact references to code selections in the chat composer.
//!
//! A selection becomes one chip in the composer instead of a pasted code
//! block. The chip is the fold placeholder the `@` file mentions already use:
//! the composer keeps a short `@path Lx-Ly` token, which stays ordinary text
//! the editor can edit and delete, and the token is displayed as a rounded
//! label. When the message is sent the token turns into a pointer that lets
//! the agent read the file itself; code is only inlined when the selection
//! came from a buffer that is not on disk.

use super::ChatView;
use crate::chat_style::{icon, ink, ui};
use editor::{
    MultiBufferOffset,
    display_map::{Crease, FoldPlaceholder},
};
use gpui::{
    App, Focusable as _, IntoElement, ParentElement, SharedString, Styled, WeakEntity, div, px,
};
use i18n::t;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{IconName, Tooltip, prelude::*};

/// Selections longer than this are never inlined, not even as a fallback, so
/// a stray `Ctrl+A` cannot put a whole file into the prompt.
pub(crate) const MAX_INLINE_SELECTION_LINES: usize = 500;
/// Character budget for the fallback excerpt, matching the old fenced snippet.
pub(crate) const MAX_INLINE_SELECTION_CHARS: usize = 8_000;
/// Longest first selected line quoted as the summary of a reference.
const MAX_SUMMARY_CHARS: usize = 100;

/// One code selection the user added to the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectionRef {
    /// Identifies the chip so a stale click cannot remove a newer reference.
    pub id: u64,
    /// Absolute path of the file, or `None` for a buffer that was never saved.
    pub path: Option<PathBuf>,
    pub start_row: u32,
    pub end_row: u32,
    pub language: Option<String>,
    /// First non-empty selected line, so the agent can confirm the region.
    pub summary: Option<String>,
    /// Selected code, kept only when the agent cannot open the file.
    pub excerpt: Option<String>,
}

impl SelectionRef {
    pub(crate) fn new(
        path: Option<PathBuf>,
        start_row: u32,
        end_row: u32,
        language: Option<String>,
        text: &str,
    ) -> Self {
        let summary = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| clip(line, MAX_SUMMARY_CHARS));
        let lines = end_row.saturating_sub(start_row) + 1;
        let excerpt =
            (path.is_none() && lines as usize <= MAX_INLINE_SELECTION_LINES).then(|| {
                clip(text, MAX_INLINE_SELECTION_CHARS)
            });
        Self {
            id: 0,
            path,
            start_row,
            end_row,
            language,
            summary,
            excerpt,
        }
    }

    fn line_count(&self) -> u32 {
        self.end_row.saturating_sub(self.start_row) + 1
    }

    /// True for a selection too large to ever be sent verbatim.
    fn is_large(&self) -> bool {
        self.line_count() as usize > MAX_INLINE_SELECTION_LINES
    }

    fn same_selection(&self, other: &Self) -> bool {
        self.path == other.path && self.start_row == other.start_row && self.end_row == other.end_row
    }

    fn file_name(&self) -> Option<String> {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// Path shown to the agent and used in the token, relative to the chat's
    /// working directory so it survives a move of the checkout.
    pub(crate) fn display_path(&self, cwd: &Path) -> Option<String> {
        let path = self.path.as_deref()?;
        let shown = path.strip_prefix(cwd).unwrap_or(path);
        Some(shown.to_string_lossy().replace('\\', "/"))
    }

    /// What the composer holds instead of the code.
    pub(crate) fn token(&self, cwd: &Path) -> String {
        match self.display_path(cwd) {
            Some(path) => format!("@{path} L{}-L{}", self.start_row, self.end_row),
            None => format!("@L{}-L{}", self.start_row, self.end_row),
        }
    }

    /// The chip reads `file.rs L11-L28`, never the code.
    fn label(&self) -> String {
        match self.file_name() {
            Some(name) => format!("{name} L{}-L{}", self.start_row, self.end_row),
            None => format!("L{}-L{}", self.start_row, self.end_row),
        }
    }

    /// Turns the reference into the text the agent receives: a pointer by
    /// default, a fenced excerpt only for a buffer that has no file on disk.
    pub(crate) fn context_text(&self, cwd: &Path) -> String {
        let rows = format!("L{}-L{}", self.start_row, self.end_row);
        let lines = self.line_count();
        let mut text = match self.display_path(cwd) {
            Some(path) => format!("选中代码：`{path}` {rows}（共 {lines} 行"),
            None => format!("选中的代码（{rows}，共 {lines} 行"),
        };
        if let Some(summary) = &self.summary {
            text.push_str(&format!("，首行 `{summary}`"));
        }
        text.push('）');
        if let Some(excerpt) = &self.excerpt {
            text.push_str("：\n\n");
            if let Some(language) = &self.language {
                text.push_str(&format!("```{language}\n"));
            }
            text.push_str(excerpt);
            if self.language.is_some() {
                text.push_str("\n```");
            }
        } else if self.path.is_some() {
            text.push_str("。请读取该文件中的这段代码。");
        } else {
            text.push_str("（选区过大，未附带代码内容）。");
        }
        text
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars).collect();
    clipped.push('…');
    clipped
}

/// Finds `token` as a whole reference: `@a.rs L1-L2` must not match the start
/// of `@a.rs L1-L20`.
fn find_token(text: &str, token: &str) -> Option<Range<usize>> {
    text.match_indices(token)
        .map(|(start, _)| start..start + token.len())
        .find(|range| {
            !text[range.end..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_digit())
        })
}

/// Replaces every reference token still present in `text` with the context the
/// agent should see. Tokens the user already deleted are skipped.
pub(crate) fn expand(text: &str, references: &[SelectionRef], cwd: &Path) -> String {
    let mut expanded = text.to_string();
    for reference in references {
        let token = reference.token(cwd);
        let Some(range) = find_token(&expanded, &token) else {
            continue;
        };
        expanded.replace_range(range, &reference.context_text(cwd));
    }
    expanded
}

fn badge(reference: &SelectionRef, chat: WeakEntity<ChatView>, cwd: &Path) -> FoldPlaceholder {
    let id = reference.id;
    let label: SharedString = reference.label().into();
    let hint = if reference.is_large() {
        t!("Large selection: only the file and line range are sent.")
    } else {
        t!("The agent reads the code from the file; the code itself is not pasted.")
    };
    let tooltip = match reference.display_path(cwd) {
        Some(path) => format!("{path}\n{hint}"),
        None => hint.to_string(),
    };
    FoldPlaceholder {
        render: Arc::new(move |fold_id, _, cx| {
            let chat = chat.clone();
            let colors = cx.theme().colors();
            let group: SharedString = format!("agent-selection-ref-{id}").into();
            div()
                .id(fold_id)
                .h_full()
                .flex()
                .items_center()
                .child(
                    h_flex()
                        .group(group.clone())
                        .h(ui(20.))
                        .gap(px(4.))
                        .pl(px(5.))
                        .pr(px(3.))
                        .rounded(px(6.))
                        .bg(ink(0.07, cx))
                        .text_size(ui(13.))
                        .text_color(colors.text)
                        .child(icon(IconName::AgentDocument, px(12.), colors.text_muted))
                        .child(label.clone())
                        .child(
                            div()
                                .id(("agent-selection-ref-remove", id as usize))
                                .size(px(14.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .visible_on_hover(group)
                                .hover(|style| style.bg(ink(0.14, cx)))
                                .tooltip(Tooltip::text(t!("Remove")))
                                .child(icon(IconName::AgentClose, px(9.), colors.text_muted))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let chat = chat.clone();
                                    let _ = chat.update(cx, |chat, cx| {
                                        chat.remove_selection_ref(id, cx)
                                    });
                                }),
                        ),
                )
                .tooltip(Tooltip::text(tooltip.clone()))
                .into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        ..FoldPlaceholder::default()
    }
}

impl ChatView {
    /// Appends a selected region to the composer as one compact chip, without
    /// sending anything. The same region is only added once.
    pub(crate) fn insert_selection_reference(
        &mut self,
        mut reference: SelectionRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.composer.focus_handle(cx), cx);
        if self
            .selection_refs
            .iter()
            .any(|existing| existing.same_selection(&reference))
        {
            return;
        }
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let token = reference.token(&cwd);
        let id = self.next_selection_ref_id;
        self.next_selection_ref_id += 1;
        reference.id = id;

        let text = self.composer.read(cx).text(cx);
        let mut insertion = String::new();
        if !text.is_empty() && !text.ends_with(char::is_whitespace) {
            insertion.push(' ');
        }
        let start = text.len() + insertion.len();
        insertion.push_str(&token);
        let end = start + token.len();
        self.composer.update(cx, |editor, cx| {
            let offset = MultiBufferOffset(text.len());
            editor.edit([(offset..offset, insertion)], cx);
            editor.change_selections(
                editor::SelectionEffects::default(),
                window,
                cx,
                |selections| selections.select_ranges([MultiBufferOffset(end)..MultiBufferOffset(end)]),
            );
        });

        let crease = Crease::simple(
            MultiBufferOffset(start)..MultiBufferOffset(end),
            badge(&reference, cx.entity().downgrade(), &cwd),
        );
        self.composer
            .update(cx, |editor, cx| editor.fold_creases(vec![crease], false, window, cx));
        self.selection_refs.push(reference);
        cx.notify();
    }

    /// Drops the reference and the token that stands for it. Other text in the
    /// composer is left alone.
    pub(super) fn remove_selection_ref(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self
            .selection_refs
            .iter()
            .position(|reference| reference.id == id)
        else {
            return;
        };
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let token = self.selection_refs[index].token(&cwd);
        self.selection_refs.remove(index);
        let text = self.composer.read(cx).text(cx);
        if let Some(range) = find_token(&text, &token) {
            let range = with_adjacent_space(&text, range);
            self.composer.update(cx, |editor, cx| {
                editor.edit(
                    [(
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        String::new(),
                    )],
                    cx,
                )
            });
        }
        cx.notify();
    }

    /// Backspace and delete remove a whole chip when the caret sits next to it.
    /// Returns false so the editor handles every other deletion normally.
    pub(super) fn delete_selection_ref_at_caret(
        &mut self,
        forward: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let caret = self.composer.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            selection.is_empty().then(|| selection.head().0)
        });
        let Some(caret) = caret else {
            return false;
        };
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let text = self.composer.read(cx).text(cx);
        let found = self.selection_refs.iter().find_map(|reference| {
            let token = reference.token(&cwd);
            let range = find_token(&text, &token)?;
            let adjacent = if forward {
                range.start == caret
            } else {
                range.end == caret
            };
            adjacent.then_some(reference.id)
        });
        let Some(id) = found else {
            return false;
        };
        self.remove_selection_ref(id, cx);
        true
    }

    /// Turns every chip still in `text` into context for the agent and forgets
    /// the references, since they belong to the message being sent.
    pub(super) fn expand_selection_refs(&mut self, text: &str, cx: &App) -> String {
        if self.selection_refs.is_empty() {
            return text.to_string();
        }
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let expanded = expand(text, &self.selection_refs, &cwd);
        self.selection_refs.clear();
        expanded
    }
}

/// Eats the separator space around a removed token so the sentence does not
/// collapse into a double space.
fn with_adjacent_space(text: &str, range: Range<usize>) -> Range<usize> {
    let mut range = range;
    if text[range.end..].starts_with(' ') && !text[..range.start].ends_with('\n') {
        range.end += 1;
    } else if text[..range.start].ends_with(' ') {
        range.start -= 1;
    }
    range
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(path: &str, start_row: u32, end_row: u32, text: &str) -> SelectionRef {
        SelectionRef::new(
            Some(PathBuf::from(path)),
            start_row,
            end_row,
            Some("rust".to_string()),
            text,
        )
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/work/repo")
    }

    #[test]
    fn a_reference_is_a_pointer_that_quotes_only_the_first_line() {
        let reference = reference(
            "/work/repo/src/panel.rs",
            11,
            28,
            "pub fn visible_chat(\n    workspace: &mut Workspace,\n) {}\n",
        );
        assert_eq!(reference.label(), "panel.rs L11-L28");
        assert_eq!(reference.token(&cwd()), "@src/panel.rs L11-L28");
        let context = reference.context_text(&cwd());
        assert_eq!(
            context,
            "选中代码：`src/panel.rs` L11-L28（共 18 行，首行 `pub fn visible_chat(`）。请读取该文件中的这段代码。"
        );
        assert!(
            !context.contains("workspace: &mut Workspace"),
            "the code body is never pasted"
        );
    }

    #[test]
    fn an_unsaved_buffer_falls_back_to_an_excerpt_in_a_fence() {
        let selection = "fn main() {}\n";
        let reference = SelectionRef::new(None, 3, 4, Some("rust".to_string()), selection);
        let context = reference.context_text(&cwd());
        assert_eq!(
            context,
            "选中的代码（L3-L4，共 2 行，首行 `fn main() {}`）：\n\n```rust\nfn main() {}\n\n```"
        );
    }

    #[test]
    fn a_huge_selection_keeps_only_the_pointer() {
        let huge = format!("fn huge() {{\n{}\n}}", "body_line\n".repeat(MAX_INLINE_SELECTION_LINES + 1));
        let saved = SelectionRef::new(
            Some(PathBuf::from("/work/repo/big.rs")),
            1,
            900,
            Some("rust".to_string()),
            &huge,
        );
        assert!(saved.excerpt.is_none());
        assert!(!saved.context_text(&cwd()).contains("body_line"));

        let unsaved = SelectionRef::new(None, 1, 900, Some("rust".to_string()), &huge);
        assert!(unsaved.excerpt.is_none());
        let context = unsaved.context_text(&cwd());
        assert!(context.contains("选区过大"));
        assert!(!context.contains("```"));
        assert!(!context.contains("body_line"));
    }

    #[test]
    fn expansion_replaces_each_token_once_and_leaves_deleted_ones_alone() {
        let first = SelectionRef::new(
            Some(PathBuf::from("/work/repo/src/a.rs")),
            1,
            2,
            Some("rust".to_string()),
            "fn a() {}",
        );
        let second = SelectionRef::new(
            Some(PathBuf::from("/work/repo/src/a.rs")),
            1,
            20,
            Some("rust".to_string()),
            "fn b() {}",
        );
        let text = format!(
            "look at {} and {} please",
            first.token(&cwd()),
            second.token(&cwd())
        );
        let expanded = expand(&text, &[first, second.clone()], &cwd());
        assert!(expanded.starts_with("look at 选中代码：`src/a.rs` L1-L2"));
        assert!(expanded.contains("`src/a.rs` L1-L20"));
        assert!(!expanded.contains("@src/a.rs"), "{expanded}");

        let deleted = expand("gone", &[second], &cwd());
        assert_eq!(deleted, "gone");
    }

    #[test]
    fn a_short_token_does_not_eat_a_longer_one() {
        let short = SelectionRef::new(
            Some(PathBuf::from("/work/repo/src/a.rs")),
            1,
            2,
            None,
            "fn a() {}",
        );
        let long = SelectionRef::new(
            Some(PathBuf::from("/work/repo/src/a.rs")),
            1,
            20,
            None,
            "fn b() {}",
        );
        let expanded = expand(&long.token(&cwd()), &[short, long], &cwd());
        assert!(expanded.contains("L1-L20"), "{expanded}");
    }
}
