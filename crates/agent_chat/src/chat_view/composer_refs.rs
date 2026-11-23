//! Compact references in the chat composer: code selections, single diagnostics
//! and groups of diagnostics.
//!
//! A reference becomes one chip in the composer instead of a pasted block. The
//! chip is the fold placeholder the `@` file mentions already use: the composer
//! keeps a short token, which stays ordinary text the editor can edit and
//! delete, and the token is displayed as a rounded label. When the message is
//! sent the token turns into a pointer that lets the agent read the file
//! itself; a selection is only inlined when it came from a buffer that is not
//! on disk.

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
use settings::Settings as _;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{IconName, Tooltip, prelude::*};
use util::ResultExt as _;
pub(crate) use wu_actions::DiagnosticCitation;

/// Selections longer than this are never inlined, not even as a fallback, so
/// a stray `Ctrl+A` cannot put a whole file into the prompt.
pub(crate) const MAX_INLINE_SELECTION_LINES: usize = 500;
/// Character budget for the fallback excerpt, matching the old fenced snippet.
pub(crate) const MAX_INLINE_SELECTION_CHARS: usize = 8_000;
/// Longest first selected line quoted as the summary of a reference.
const MAX_SUMMARY_CHARS: usize = 100;
/// Diagnostics spelled out when a group chip expands. A larger group points at
/// the `diagnostics` tool instead of pasting every message.
pub(crate) const MAX_LISTED_DIAGNOSTICS: usize = 20;

/// The project digest a new chat offers in its message box.
///
/// The digest is rendered once, when the chat is created, so the chip needs no
/// background lookup when it expands: the text sent to the agent is the same
/// text the transcript keeps, collapsed behind "Show more".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DigestRef {
    /// Files the digest lists, after `DEFAULT_DIGEST_FILES` capped it.
    pub files: usize,
    /// The whole `SymbolMap::to_digest` result, sent verbatim.
    pub digest: String,
}

impl DigestRef {
    fn label(&self) -> String {
        format!("项目摘要 · {} 文件", self.files)
    }

    fn token(&self) -> String {
        format!("@digest {}", self.files)
    }

    fn tooltip(&self) -> String {
        "仓库地图摘要会随这条消息一起发送给智能体；删除这个引用就不会发送。".to_string()
    }

    /// The transcript keeps the digest under the same name the chip shows, so a
    /// sent message never hides the context the agent received.
    fn context_text(&self) -> String {
        format!("{}\n\n{}", self.label(), self.digest)
    }
}

/// One code selection the user added to the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectionRef {
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
        let excerpt = (path.is_none() && lines as usize <= MAX_INLINE_SELECTION_LINES)
            .then(|| clip(text, MAX_INLINE_SELECTION_CHARS));
        Self {
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
        self.path == other.path
            && self.start_row == other.start_row
            && self.end_row == other.end_row
    }

    fn file_name(&self) -> Option<String> {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
    }

    fn token(&self, cwd: &Path) -> String {
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

    fn tooltip(&self, cwd: &Path) -> String {
        let hint = if self.is_large() {
            t!("Large selection: only the file and line range are sent.")
        } else {
            t!("The agent reads the code from the file; the code itself is not pasted.")
        };
        match self.display_path(cwd) {
            Some(path) => format!("{path}\n{hint}"),
            None => hint.to_string(),
        }
    }

    /// Turns the reference into the text the agent receives: a pointer by
    /// default, a fenced excerpt only for a buffer that has no file on disk.
    fn context_text(&self, cwd: &Path) -> String {
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

    /// Path shown to the agent and used in the token, relative to the chat's
    /// working directory so it survives a move of the checkout.
    fn display_path(&self, cwd: &Path) -> Option<String> {
        Some(display_path(self.path.as_deref()?, cwd))
    }
}

/// What a chip in the composer stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ComposerRefTarget {
    Selection(SelectionRef),
    Diagnostic(DiagnosticCitation),
    /// Several diagnostics added at once, summarised in the composer.
    Diagnostics(Vec<DiagnosticCitation>),
    /// The project digest offered to a chat that has no messages yet.
    Digest(DigestRef),
}

impl ComposerRefTarget {
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Selection(a), Self::Selection(b)) => a.same_selection(b),
            (Self::Diagnostic(a), Self::Diagnostic(b)) => same_diagnostic(a, b),
            (Self::Diagnostics(a), Self::Diagnostics(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|reference| b.iter().any(|other| same_diagnostic(reference, other)))
            }
            (Self::Digest(a), Self::Digest(b)) => a == b,
            _ => false,
        }
    }

    /// True when this chip already points the agent at `citation`.
    fn references_diagnostic(&self, citation: &DiagnosticCitation) -> bool {
        match self {
            Self::Selection(_) | Self::Digest(_) => false,
            Self::Diagnostic(existing) => same_diagnostic(existing, citation),
            Self::Diagnostics(existing) => existing
                .iter()
                .any(|other| same_diagnostic(other, citation)),
        }
    }
}

/// One chip in the composer, with the id that ties it to its token.
#[derive(Clone, Debug)]
pub(crate) struct ComposerRef {
    pub id: u64,
    pub target: ComposerRefTarget,
}

impl ComposerRef {
    pub(crate) fn new(id: u64, target: ComposerRefTarget) -> Self {
        Self { id, target }
    }

    fn token(&self, cwd: &Path) -> String {
        match &self.target {
            ComposerRefTarget::Selection(selection) => selection.token(cwd),
            ComposerRefTarget::Diagnostic(citation) => diagnostic_token(citation, cwd),
            ComposerRefTarget::Diagnostics(citations) => {
                // The id keeps two same-sized groups from sharing a token.
                format!("@diagnostics {} #{}", citations.len(), self.id)
            }
            ComposerRefTarget::Digest(digest) => digest.token(),
        }
    }

    fn label(&self) -> String {
        match &self.target {
            ComposerRefTarget::Selection(selection) => selection.label(),
            ComposerRefTarget::Diagnostic(citation) => diagnostic_label(citation),
            ComposerRefTarget::Diagnostics(citations) => {
                t!("{count} diagnostics", count = citations.len()).to_string()
            }
            ComposerRefTarget::Digest(digest) => digest.label(),
        }
    }

    fn icon(&self) -> IconName {
        match &self.target {
            ComposerRefTarget::Selection(_) => IconName::AgentDocument,
            ComposerRefTarget::Diagnostic(_) => IconName::AgentDangerTriangle,
            ComposerRefTarget::Diagnostics(_) => IconName::AgentChecklist,
            ComposerRefTarget::Digest(_) => IconName::AgentFolderWithFiles,
        }
    }

    fn tooltip(&self, cwd: &Path) -> String {
        match &self.target {
            ComposerRefTarget::Selection(selection) => selection.tooltip(cwd),
            ComposerRefTarget::Diagnostic(citation) => {
                let hint =
                    t!("The agent reads this diagnostic from the file; the message is not pasted.");
                format!("{}\n{hint}", diagnostic_header(citation))
            }
            ComposerRefTarget::Diagnostics(citations) => {
                let hint = t!(
                    "The agent gets a pointer list of these diagnostics and can read their details itself."
                );
                let listed = citations
                    .iter()
                    .take(3)
                    .map(|citation| format!("  {}", diagnostic_label(citation)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let more = citations.len().saturating_sub(3);
                if more > 0 {
                    format!("{listed}\n  … +{more}\n{hint}")
                } else {
                    format!("{listed}\n{hint}")
                }
            }
            ComposerRefTarget::Digest(digest) => digest.tooltip(),
        }
    }

    fn context_text(&self, cwd: &Path) -> String {
        match &self.target {
            ComposerRefTarget::Selection(selection) => selection.context_text(cwd),
            ComposerRefTarget::Diagnostic(citation) => {
                format!(
                    "{}。请读取该文件这一行附近的代码。",
                    diagnostic_pointer(citation, cwd)
                )
            }
            ComposerRefTarget::Diagnostics(citations) => {
                let mut text = format!("诊断（共 {} 条）：", citations.len());
                for citation in citations.iter().take(MAX_LISTED_DIAGNOSTICS) {
                    text.push_str(&format!("\n- {}", diagnostic_pointer(citation, cwd)));
                }
                if citations.len() > MAX_LISTED_DIAGNOSTICS {
                    text.push_str(&format!(
                        "\n（共 {} 条，可用 diagnostics 工具取全文）",
                        citations.len()
                    ));
                }
                text
            }
            ComposerRefTarget::Digest(digest) => digest.context_text(),
        }
    }
}

/// Two diagnostics are the same reference when they sit on the same line of the
/// same file and carry the same rule code.
fn same_diagnostic(a: &DiagnosticCitation, b: &DiagnosticCitation) -> bool {
    a.path == b.path && a.line == b.line && a.code == b.code
}

/// A pointer the agent can act on: path, position, severity, code and message.
fn diagnostic_pointer(citation: &DiagnosticCitation, cwd: &Path) -> String {
    let path = display_path(&citation.path, cwd);
    let mut pointer = format!(
        "{path}:{}:{} {}",
        citation.line, citation.column, citation.severity
    );
    if let Some(code) = &citation.code {
        pointer.push_str(&format!(" {code}"));
    }
    let message = citation.message.replace('\n', " ");
    let message = message.trim();
    if !message.is_empty() {
        pointer.push(' ');
        pointer.push_str(message);
    }
    pointer
}

/// The tooltip line for one diagnostic, with the full path so it is unique.
fn diagnostic_header(citation: &DiagnosticCitation) -> String {
    let mut header = format!(
        "{}:{}:{}",
        citation.path.display(),
        citation.line,
        citation.column
    );
    header.push_str(&format!(" [{}]", citation.severity));
    if let Some(code) = &citation.code {
        header.push(' ');
        header.push_str(code);
    }
    let message = citation.message.trim();
    if !message.is_empty() {
        header.push('\n');
        header.push_str(message);
    }
    header
}

/// `activation_manager_fh.hpp:42` is enough for the chip; the path lives in the
/// token and the tooltip.
fn diagnostic_label(citation: &DiagnosticCitation) -> String {
    let name = citation
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| citation.path.to_string_lossy().into_owned());
    format!("{name}:{}", citation.line)
}

fn diagnostic_token(citation: &DiagnosticCitation, cwd: &Path) -> String {
    let path = display_path(&citation.path, cwd);
    // A missing code gets `-` so the token of one diagnostic can never be a
    // prefix of another's.
    match &citation.code {
        Some(code) => format!("@diag {path}:{}:{} {code}", citation.line, citation.column),
        None => format!("@diag {path}:{}:{} -", citation.line, citation.column),
    }
}

fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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
/// of `@a.rs L1-L20`, and one diagnostic token must not match a longer one.
fn find_token(text: &str, token: &str) -> Option<Range<usize>> {
    text.match_indices(token)
        .map(|(start, _)| start..start + token.len())
        .find(|range| {
            !text[range.end..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_' || next == '#')
        })
}

/// Replaces every reference token still present in `text` with the context the
/// agent should see. Tokens the user already deleted are skipped.
pub(crate) fn expand(text: &str, references: &[ComposerRef], cwd: &Path) -> String {
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

fn badge(reference: &ComposerRef, chat: WeakEntity<ChatView>, cwd: &Path) -> FoldPlaceholder {
    let id = reference.id;
    let label: SharedString = reference.label().into();
    let icon_name = reference.icon();
    let tooltip = reference.tooltip(cwd);
    FoldPlaceholder {
        render: Arc::new(move |fold_id, _, cx| {
            let chat = chat.clone();
            let colors = cx.theme().colors();
            let group: SharedString = format!("agent-composer-ref-{id}").into();
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
                        .child(icon(icon_name, px(12.), colors.text_muted))
                        .child(label.clone())
                        .child(
                            div()
                                .id(("agent-composer-ref-remove", id as usize))
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
                                    let _ = chat
                                        .update(cx, |chat, cx| chat.remove_composer_ref(id, cx));
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
    /// Appends a reference to the composer as one compact chip, without sending
    /// anything. The same target is only added once, and a group leaves out
    /// diagnostics an existing chip already points at.
    pub(crate) fn insert_composer_reference(
        &mut self,
        target: ComposerRefTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.composer.focus_handle(cx), cx);
        let target = match target {
            ComposerRefTarget::Diagnostics(citations) => {
                let mut kept: Vec<DiagnosticCitation> = Vec::new();
                for citation in citations {
                    if self.references_diagnostic(&citation)
                        || kept
                            .iter()
                            .any(|existing| same_diagnostic(existing, &citation))
                    {
                        continue;
                    }
                    kept.push(citation);
                }
                if kept.is_empty() {
                    return;
                }
                ComposerRefTarget::Diagnostics(kept)
            }
            ComposerRefTarget::Diagnostic(citation) => {
                if self.references_diagnostic(&citation) {
                    return;
                }
                ComposerRefTarget::Diagnostic(citation)
            }
            selection @ ComposerRefTarget::Selection(_) => selection,
            digest @ ComposerRefTarget::Digest(_) => digest,
        };
        if self
            .composer_refs
            .iter()
            .any(|existing| existing.target.same_target(&target))
        {
            return;
        }
        let id = self.next_composer_ref_id;
        self.next_composer_ref_id += 1;
        self.add_composer_ref(ComposerRef::new(id, target), false, window, cx);
    }

    /// Inserts a reference at the start of the composer, before anything the
    /// user has already typed.
    fn add_composer_ref(
        &mut self,
        reference: ComposerRef,
        at_start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let token = reference.token(&cwd);

        let text = self.composer.read(cx).text(cx);
        let mut insertion = String::new();
        let (offset, start) = if at_start {
            insertion.push_str(&token);
            if !text.is_empty() && !text.starts_with(char::is_whitespace) {
                insertion.push(' ');
            }
            (0, 0)
        } else {
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                insertion.push(' ');
            }
            let start = text.len() + insertion.len();
            insertion.push_str(&token);
            (text.len(), start)
        };
        let end = start + token.len();
        self.composer.update(cx, |editor, cx| {
            let offset = MultiBufferOffset(offset);
            editor.edit([(offset..offset, insertion)], cx);
            editor.change_selections(
                editor::SelectionEffects::default(),
                window,
                cx,
                |selections| {
                    selections.select_ranges([MultiBufferOffset(end)..MultiBufferOffset(end)])
                },
            );
        });

        let crease = Crease::simple(
            MultiBufferOffset(start)..MultiBufferOffset(end),
            badge(&reference, cx.entity().downgrade(), &cwd),
        );
        self.composer.update(cx, |editor, cx| {
            editor.fold_creases(vec![crease], false, window, cx)
        });
        self.composer_refs.push(reference);
        cx.notify();
    }

    /// Offers the project digest to a chat that has no messages yet.
    ///
    /// The symbol map is built off the foreground thread, so this returns as
    /// soon as the build starts. A repository without definitions produces
    /// nothing: no chip, no notice.
    pub(crate) fn offer_repo_digest(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.subagent.is_some()
            || self.digest_offered
            || !crate::AgentChatSettings::get_global(cx).inject_repo_map_digest
        {
            return false;
        }
        if self
            .session
            .read(cx)
            .entries()
            .iter()
            .any(|entry| matches!(entry, crate::session::Entry::User { .. }))
        {
            return false;
        }
        let map_task = repo_wiki::symbol_map::symbol_map_task(self.project.clone(), cx);
        let root = self.session.read(cx).metadata().cwd.clone();
        let prepare = cx.background_spawn(async move {
            let map = map_task.await;
            let ranked = map.ranked().count();
            if ranked == 0 {
                return None;
            }
            let files = ranked.min(repo_wiki::symbol_map::DEFAULT_DIGEST_FILES);
            let digest = map.to_digest(
                repo_wiki::symbol_map::DEFAULT_DIGEST_FILES,
                repo_wiki::symbol_map::DEFAULT_DIGEST_SYMBOLS_PER_FILE,
                repo_wiki::symbol_map::MAX_DIGEST_CHARS,
            );
            // Pi's MCP server reads the snapshot from the project directory, so
            // a chat that never opened the repo map panel still gets one.
            if let Err(error) =
                repo_wiki::snapshot::write(&map, &root, &paths::repo_map_snapshots_dir())
            {
                log::error!("failed to write the repo map snapshot for {root:?}: {error}");
            }
            Some(DigestRef { files, digest })
        });
        self._digest_chip = cx.spawn_in(window, async move |this, cx| {
            let Some(digest) = prepare.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.insert_digest_chip(digest, window, cx);
            })
            .log_err();
        });
        true
    }

    /// Adds the digest chip once. A chat keeps the decision afterwards: a chip
    /// the user removes is never put back.
    pub(crate) fn insert_digest_chip(
        &mut self,
        digest: DigestRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.digest_offered
            || self
                .session
                .read(cx)
                .entries()
                .iter()
                .any(|entry| matches!(entry, crate::session::Entry::User { .. }))
        {
            return false;
        }
        let target = ComposerRefTarget::Digest(digest);
        if self
            .composer_refs
            .iter()
            .any(|existing| existing.target.same_target(&target))
        {
            return false;
        }
        let id = self.next_composer_ref_id;
        self.next_composer_ref_id += 1;
        self.add_composer_ref(ComposerRef::new(id, target), true, window, cx);
        self.digest_offered = true;
        true
    }

    fn references_diagnostic(&self, citation: &DiagnosticCitation) -> bool {
        self.composer_refs
            .iter()
            .any(|reference| reference.target.references_diagnostic(citation))
    }

    /// Drops the reference and the token that stands for it. Other text in the
    /// composer is left alone.
    pub(super) fn remove_composer_ref(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self
            .composer_refs
            .iter()
            .position(|reference| reference.id == id)
        else {
            return;
        };
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let token = self.composer_refs[index].token(&cwd);
        self.composer_refs.remove(index);
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
    pub(super) fn delete_composer_ref_at_caret(
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
        let found = self.composer_refs.iter().find_map(|reference| {
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
        self.remove_composer_ref(id, cx);
        true
    }

    /// Turns every chip still in `text` into context for the agent and forgets
    /// the references, since they belong to the message being sent.
    pub(super) fn expand_composer_refs(&mut self, text: &str, cx: &App) -> String {
        if self.composer_refs.is_empty() {
            return text.to_string();
        }
        let cwd = self.session.read(cx).metadata().cwd.clone();
        let expanded = expand(text, &self.composer_refs, &cwd);
        self.composer_refs.clear();
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

    fn citation(
        path: &str,
        line: u32,
        column: u32,
        code: Option<&str>,
        message: &str,
    ) -> DiagnosticCitation {
        DiagnosticCitation {
            path: PathBuf::from(path),
            line,
            column,
            severity: "error".to_string(),
            code: code.map(str::to_string),
            message: message.to_string(),
        }
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
        let huge = format!(
            "fn huge() {{\n{}\n}}",
            "body_line\n".repeat(MAX_INLINE_SELECTION_LINES + 1)
        );
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
        let first = ComposerRef::new(0, ComposerRefTarget::Selection(first));
        let second = ComposerRef::new(1, ComposerRefTarget::Selection(second));
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
        let short = ComposerRef::new(0, ComposerRefTarget::Selection(short));
        let long = ComposerRef::new(1, ComposerRefTarget::Selection(long));
        let expanded = expand(&long.token(&cwd()), &[short, long], &cwd());
        assert!(expanded.contains("L1-L20"), "{expanded}");
    }

    #[test]
    fn a_diagnostic_chip_names_the_line_and_expands_to_a_pointer() {
        let reference = ComposerRef::new(
            0,
            ComposerRefTarget::Diagnostic(citation(
                "/work/repo/src/activation_manager_fh.hpp",
                42,
                7,
                Some("unused-variable"),
                "unused variable 'count'",
            )),
        );
        assert_eq!(reference.label(), "activation_manager_fh.hpp:42");
        assert_eq!(
            reference.token(&cwd()),
            "@diag src/activation_manager_fh.hpp:42:7 unused-variable"
        );
        assert_eq!(
            reference.context_text(&cwd()),
            "src/activation_manager_fh.hpp:42:7 error unused-variable unused variable 'count'。请读取该文件这一行附近的代码。"
        );
        let tooltip = reference.tooltip(&cwd());
        assert!(tooltip.contains("/work/repo/src/activation_manager_fh.hpp:42:7"));
        assert!(tooltip.contains("unused-variable"));
        assert!(tooltip.contains("unused variable 'count'"));
    }

    #[test]
    fn a_diagnostic_group_lists_pointers_and_truncates_a_large_group() {
        let citations: Vec<_> = (1..=MAX_LISTED_DIAGNOSTICS as u32 + 5)
            .map(|line| {
                citation(
                    "/work/repo/src/worker.cpp",
                    line,
                    3,
                    Some("E0308"),
                    "mismatched types",
                )
            })
            .collect();
        let reference = ComposerRef::new(7, ComposerRefTarget::Diagnostics(citations));
        assert_eq!(
            reference.label(),
            format!("{} diagnostics", MAX_LISTED_DIAGNOSTICS + 5)
        );
        let context = reference.context_text(&cwd());
        let lines: Vec<_> = context.lines().collect();
        assert_eq!(
            lines.len(),
            MAX_LISTED_DIAGNOSTICS + 2,
            "header, the first N pointers and the summary"
        );
        assert_eq!(
            lines[1],
            "- src/worker.cpp:1:3 error E0308 mismatched types"
        );
        assert_eq!(
            *lines.last().unwrap(),
            format!(
                "（共 {} 条，可用 diagnostics 工具取全文）",
                MAX_LISTED_DIAGNOSTICS + 5
            )
        );
        assert_eq!(
            reference.token(&cwd()),
            format!("@diagnostics {} #7", MAX_LISTED_DIAGNOSTICS + 5)
        );
    }

    #[test]
    fn a_group_of_same_line_diagnostics_stays_short() {
        let citations = vec![
            citation("/work/repo/a.rs", 3, 1, Some("E1"), "one"),
            citation("/work/repo/b.rs", 9, 2, None, "two"),
        ];
        let reference = ComposerRef::new(3, ComposerRefTarget::Diagnostics(citations));
        assert_eq!(
            reference.context_text(&cwd()),
            "诊断（共 2 条）：\n- a.rs:3:1 error E1 one\n- b.rs:9:2 error two"
        );
    }

    #[test]
    fn tokens_of_neighbouring_diagnostics_do_not_swallow_each_other() {
        let coded = ComposerRef::new(
            0,
            ComposerRefTarget::Diagnostic(citation("/work/repo/a.rs", 4, 1, Some("E030"), "a")),
        );
        let longer_code = ComposerRef::new(
            1,
            ComposerRefTarget::Diagnostic(citation("/work/repo/a.rs", 4, 1, Some("E0308"), "b")),
        );
        let plain = ComposerRef::new(
            2,
            ComposerRefTarget::Diagnostic(citation("/work/repo/a.rs", 4, 1, None, "c")),
        );
        let text = format!(
            "{} / {} / {}",
            coded.token(&cwd()),
            longer_code.token(&cwd()),
            plain.token(&cwd())
        );
        let expanded = expand(&text, &[coded, longer_code, plain], &cwd());
        assert!(expanded.contains("E030 a"), "{expanded}");
        assert!(expanded.contains("E0308 b"), "{expanded}");
        assert!(expanded.contains("error c"), "{expanded}");
        assert!(!expanded.contains("@diag"), "{expanded}");
    }

    fn digest_map(files: usize) -> repo_wiki::symbol_map::SymbolMap {
        use repo_wiki::symbol_map::{FileSymbols, SymbolEntry, SymbolMap};
        SymbolMap {
            files: (0..files)
                .map(|index| FileSymbols {
                    rel: path::rel_path::RelPath::new(
                        Path::new(&format!("src/file{index}.rs")),
                        path::PathStyle::local(),
                    )
                    .expect("valid test path")
                    .into_arc(),
                    size: 120,
                    rank: 1.0 - index as f32 / 10.0,
                    lines: 12,
                    in_degree: 1,
                    out_degree: 1,
                    dep_in_degree: 0,
                    symbols: vec![SymbolEntry {
                        name: format!("item{index}").into(),
                        kind: "function".into(),
                        row: 0,
                        owner: None,
                        depth: 0,
                        access: None,
                        signature: None,
                        local: false,
                    }],
                })
                .collect(),
            ..SymbolMap::default()
        }
    }

    #[test]
    fn a_digest_chip_names_its_files_and_expands_to_the_whole_digest() {
        let digest = "仓库地图摘要（按被引用程度排序，共 2 个文件）：\n- src/main.rs";
        let reference = ComposerRef::new(
            0,
            ComposerRefTarget::Digest(DigestRef {
                files: 2,
                digest: digest.to_string(),
            }),
        );
        assert_eq!(reference.label(), "项目摘要 · 2 文件");
        assert_eq!(reference.token(&cwd()), "@digest 2");

        let text = format!("请先熟悉项目 {} 再动手", reference.token(&cwd()));
        let expanded = expand(&text, std::slice::from_ref(&reference), &cwd());
        assert_eq!(
            expanded,
            format!("请先熟悉项目 项目摘要 · 2 文件\n\n{digest} 再动手")
        );
        assert!(!expanded.contains("@digest"));
    }

    #[test]
    fn the_digest_from_the_symbol_map_travels_whole() {
        use repo_wiki::symbol_map::{
            DEFAULT_DIGEST_FILES, DEFAULT_DIGEST_SYMBOLS_PER_FILE, MAX_DIGEST_CHARS,
        };

        let digest = digest_map(3).to_digest(
            DEFAULT_DIGEST_FILES,
            DEFAULT_DIGEST_SYMBOLS_PER_FILE,
            MAX_DIGEST_CHARS,
        );
        assert!(digest.contains("仓库地图摘要"));
        assert!(digest.contains("src/file0.rs"));

        let reference = ComposerRef::new(
            0,
            ComposerRefTarget::Digest(DigestRef {
                files: 3,
                digest: digest.clone(),
            }),
        );
        let expanded = expand(&reference.token(&cwd()), &[reference], &cwd());
        assert_eq!(expanded, format!("项目摘要 · 3 文件\n\n{digest}"));
        assert!(expanded.ends_with(&digest), "the body is not truncated");
    }
}
