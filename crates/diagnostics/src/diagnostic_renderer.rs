use std::{
    ops::Range,
    path::Path,
    sync::Arc,
};

use editor::{
    Anchor, Editor, EditorSnapshot, ToOffset,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle},
    hover_popover::diagnostics_markdown_style,
};
use gpui::{AppContext, Entity, Focusable, WeakEntity};
use i18n::t;
use language::{BufferId, Diagnostic, DiagnosticEntryRef, LanguageRegistry};
use lsp::DiagnosticSeverity;
use markdown::{CopyButtonVisibility, Markdown, MarkdownElement};
use project::project_settings::ProjectSettings;
use settings::Settings;
use text::Point;
use theme_settings::ThemeSettings;
use ui::{Checkbox, CopyButton, IconButton, IconName, IconSize, ToggleState, Tooltip, prelude::*};
use util::maybe;
use wu_actions::{AddDiagnosticsToChat, DiagnosticCitation};

use crate::toolbar_controls::DiagnosticsToolbarEditor;
use crate::translation::{self, TranslationChunk, TranslationId};

pub struct DiagnosticRenderer;

impl DiagnosticRenderer {
    pub fn diagnostic_blocks_for_group(
        diagnostic_group: Vec<DiagnosticEntryRef<'_, Point>>,
        buffer_id: BufferId,
        citation_path: Option<&Path>,
        diagnostics_editor: Option<Arc<dyn DiagnosticsToolbarEditor>>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Vec<DiagnosticBlock> {
        let Some(primary_ix) = diagnostic_group
            .iter()
            .position(|d| d.diagnostic.is_primary)
        else {
            return Vec::new();
        };
        let primary = &diagnostic_group[primary_ix];
        let group_id = primary.diagnostic.group_id;
        let mut results = vec![];
        for entry in diagnostic_group.iter() {
            let message = Self::markdown(&entry.diagnostic);
            let mut rest = String::new();
            let translation = if entry.diagnostic.is_primary {
                let diagnostic = &primary.diagnostic;
                append_source_and_code(&mut rest, diagnostic);

                for (ix, entry) in diagnostic_group.iter().enumerate() {
                    if entry.range.start.row.abs_diff(primary.range.start.row) >= 5 {
                        rest.push_str(&format!("\n- {}: [", t!("hint")));
                        rest.push_str(&Markdown::escape(entry.diagnostic.message.as_str()));
                        rest.push_str(&format!(
                            "](file://#diagnostic-{buffer_id}-{group_id}-{ix})\n",
                        ))
                    }
                }

                Some(TranslationId {
                    buffer_id,
                    group_id,
                    index: primary_ix,
                })
            } else {
                append_source_and_code(&mut rest, entry.diagnostic);

                rest.push_str(&format!(
                    " ([{}](file://#diagnostic-{buffer_id}-{group_id}-{primary_ix}))",
                    t!("back")
                ));
                None
            };

            let chunks = vec![TranslationChunk {
                message,
                rest,
                diagnostic: translation,
            }];
            results.push(DiagnosticBlock {
                initial_range: entry.range.clone(),
                severity: entry.diagnostic.severity,
                count: 1,
                diagnostics_editor: diagnostics_editor.clone(),
                copy_message: entry.diagnostic.message.as_shared_string().clone(),
                citation: citation_path
                    .map(|path| citation_for(path, entry.range.start, &entry.diagnostic)),
                markdown: Self::markdown_entity(chunks.clone(), language_registry.clone(), cx),
                chunks,
            });
        }

        results
    }

    /// The markdown of one diagnostic, with its translation state already
    /// applied, registered so that a translation arriving later can rewrite
    /// it in place instead of waiting for the block to be rebuilt.
    fn markdown_entity(
        chunks: Vec<TranslationChunk>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Entity<Markdown> {
        let translations = translation::translations(cx);
        let enabled = ProjectSettings::get_global(cx)
            .diagnostics
            .translate_with_model;
        let source = match &translations {
            Some(translations) => translations.read(cx).source(&chunks, enabled),
            None => chunks
                .iter()
                .map(|chunk| format!("{}{}", chunk.message, chunk.rest))
                .collect::<Vec<_>>()
                .join("\n\n"),
        };
        let markdown = cx.new(|cx| Markdown::new(source.into(), language_registry, None, cx));
        if let Some(translations) = translations {
            translations.update(cx, |translations, _| {
                translations.register_view(&markdown, chunks)
            });
        }
        markdown
    }

    fn markdown(diagnostic: &Diagnostic) -> String {
        let mut markdown = String::new();

        if let Some(message_markdown) = diagnostic.message.markdown() {
            markdown.push_str(message_markdown);
        } else {
            markdown.push_str(&Markdown::escape(diagnostic.message.as_str()));
        };
        markdown
    }

    /// Merges blocks whose diagnostics start at the same position into one row
    /// that lists every message. The display map anchors a block to a position,
    /// so blocks there end up on the same display row and paint over each
    /// other; which one stays visible then depends on paint order.
    pub(crate) fn merge_blocks_at_same_position(
        mut blocks: Vec<DiagnosticBlock>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Vec<DiagnosticBlock> {
        if blocks.len() < 2 {
            return blocks;
        }
        // The blocks come out of a `HashMap`, so their order is arbitrary; the
        // merged text must not be. The worst severity leads the row.
        blocks.sort_by(|left, right| {
            (left.initial_range.start, left.severity, left.initial_range.end)
                .cmp(&(
                    right.initial_range.start,
                    right.severity,
                    right.initial_range.end,
                ))
                .then_with(|| left.copy_message.cmp(&right.copy_message))
        });

        let mut merged: Vec<DiagnosticBlock> = Vec::with_capacity(blocks.len());
        for block in blocks {
            let Some(existing) = merged
                .iter_mut()
                .find(|existing| existing.initial_range.start == block.initial_range.start)
            else {
                merged.push(block);
                continue;
            };
            let mut chunks = existing.chunks.clone();
            chunks.extend(block.chunks);
            existing.markdown =
                Self::markdown_entity(chunks.clone(), language_registry.clone(), cx);
            existing.chunks = chunks;
            existing.count += block.count;
        }
        merged
    }
}

/// The name the chat uses for a diagnostic severity.
pub(crate) fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::ERROR => "error",
        DiagnosticSeverity::WARNING => "warning",
        DiagnosticSeverity::INFORMATION => "information",
        DiagnosticSeverity::HINT => "hint",
        _ => "information",
    }
}

/// A diagnostic as the chat cites it: one-based position and the message that
/// stays out of the composer.
pub(crate) fn citation_for(
    path: &Path,
    position: Point,
    diagnostic: &Diagnostic,
) -> DiagnosticCitation {
    DiagnosticCitation {
        path: path.to_path_buf(),
        line: position.row + 1,
        column: position.column + 1,
        severity: severity_name(diagnostic.severity).to_string(),
        code: diagnostic.code.as_ref().map(|code| code.to_string()),
        message: diagnostic.message.as_str().to_string(),
    }
}

/// Highest severity the panel currently shows, which is also the highest one
/// worth citing.
pub(crate) fn max_shown_severity(include_warnings: bool) -> DiagnosticSeverity {
    if include_warnings {
        DiagnosticSeverity::WARNING
    } else {
        DiagnosticSeverity::ERROR
    }
}

fn append_source_and_code(markdown: &mut String, diagnostic: &Diagnostic) {
    if diagnostic.source.is_none() && diagnostic.code.is_none() {
        return;
    }
    let is_lsp_markdown = diagnostic
        .message
        .lsp_markup()
        .is_some_and(|(kind, _)| kind == &lsp::MarkupKind::Markdown);
    if is_lsp_markdown {
        markdown.push_str("\n\n(");
    } else {
        markdown.push_str(" (");
    }
    if let Some(source) = diagnostic.source.as_ref() {
        markdown.push_str(&Markdown::escape(source));
    }
    if diagnostic.source.is_some() && diagnostic.code.is_some() {
        markdown.push(' ');
    }
    if let Some(code) = diagnostic.code.as_ref() {
        if let Some(description) = diagnostic.code_description.as_ref() {
            markdown.push('[');
            markdown.push_str(&Markdown::escape(&code.to_string()));
            markdown.push_str("](");
            markdown.push_str(&Markdown::escape(description.as_ref()));
            markdown.push(')');
        } else {
            markdown.push_str(&Markdown::escape(&code.to_string()));
        }
    }
    markdown.push(')');
}

impl editor::DiagnosticRenderer for DiagnosticRenderer {
    fn render_group(
        &self,
        diagnostic_group: Vec<DiagnosticEntryRef<'_, Point>>,
        buffer_id: BufferId,
        snapshot: EditorSnapshot,
        editor: WeakEntity<Editor>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Vec<BlockProperties<Anchor>> {
        // The block is rendered inside the file's own editor, so the buffer it
        // came from is the file to cite. Without it the block has no cite
        // button at all, which is why the entry used to exist only in the
        // Problems panel.
        let citation_path = snapshot
            .buffer_snapshot()
            .as_singleton()
            .and_then(|buffer| buffer.file())
            .map(|file| file.path().as_std_path().to_path_buf());
        let blocks = Self::diagnostic_blocks_for_group(
            diagnostic_group,
            buffer_id,
            citation_path.as_deref(),
            None,
            language_registry,
            cx,
        );

        blocks
            .into_iter()
            .map(|block| {
                let editor = editor.clone();
                BlockProperties {
                    placement: BlockPlacement::Near(
                        snapshot
                            .buffer_snapshot()
                            .anchor_after(block.initial_range.start),
                    ),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |bcx| block.render_block(editor.clone(), bcx)),
                    priority: 1,
                }
            })
            .collect()
    }

    fn render_hover(
        &self,
        diagnostic_group: Vec<DiagnosticEntryRef<'_, Point>>,
        range: Range<Point>,
        buffer_id: BufferId,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> Option<Entity<Markdown>> {
        let blocks = Self::diagnostic_blocks_for_group(
            diagnostic_group,
            buffer_id,
            None,
            None,
            language_registry,
            cx,
        );
        blocks
            .into_iter()
            .find_map(|block| (block.initial_range == range).then(|| block.markdown))
    }

    fn open_link(
        &self,
        editor: &mut Editor,
        link: SharedString,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        DiagnosticBlock::open_link(editor, &None, link, window, cx);
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticBlock {
    pub(crate) initial_range: Range<Point>,
    pub(crate) severity: DiagnosticSeverity,
    /// How many diagnostics this row stands for. Diagnostics that start at the
    /// same position share one display row, so they are merged and counted here
    /// instead of silently disappearing behind each other.
    pub(crate) count: usize,
    pub(crate) markdown: Entity<Markdown>,
    /// The markdown is built from these, so a translation can be applied to
    /// it again without rebuilding the block.
    pub(crate) chunks: Vec<TranslationChunk>,
    pub(crate) diagnostics_editor: Option<Arc<dyn DiagnosticsToolbarEditor>>,
    pub(crate) copy_message: SharedString,
    /// Present when the diagnostic can be cited in a chat, which needs the file
    /// it belongs to.
    pub(crate) citation: Option<DiagnosticCitation>,
}

impl DiagnosticBlock {
    pub fn render_block(&self, editor: WeakEntity<Editor>, bcx: &BlockContext) -> AnyElement {
        let cx = &bcx.app;
        let status_colors = cx.theme().status();

        let max_width = bcx.em_width * 120.;

        let (background_color, border_color) = match self.severity {
            DiagnosticSeverity::ERROR => (status_colors.error_background, status_colors.error),
            DiagnosticSeverity::WARNING => {
                (status_colors.warning_background, status_colors.warning)
            }
            DiagnosticSeverity::INFORMATION => (status_colors.info_background, status_colors.info),
            DiagnosticSeverity::HINT => (status_colors.hint_background, status_colors.hint),
            _ => (status_colors.ignored_background, status_colors.ignored),
        };
        let settings = ThemeSettings::get_global(cx);
        let editor_line_height = (settings.line_height() * settings.buffer_font_size(cx)).round();
        let line_height = editor_line_height;
        let diagnostics_editor = self.diagnostics_editor.clone();

        let copy_button_id = format!(
            "copy-diagnostic-{}-{}-{}-{}",
            self.initial_range.start.row,
            self.initial_range.start.column,
            self.initial_range.end.row,
            self.initial_range.end.column
        );
        let chat_button_id = format!(
            "chat-diagnostic-{}-{}-{}-{}",
            self.initial_range.start.row,
            self.initial_range.start.column,
            self.initial_range.end.row,
            self.initial_range.end.column
        );
        let checkbox = self
            .diagnostics_editor
            .clone()
            .filter(|diagnostics_editor| diagnostics_editor.supports_diagnostic_selection())
            .zip(self.citation.clone())
            .map(|(diagnostics_editor, citation)| {
                let checkbox_id = format!(
                    "select-diagnostic-{}-{}-{}-{}",
                    citation.path.display(),
                    citation.line,
                    citation.column,
                    citation.code.as_deref().unwrap_or("-")
                );
                let toggle_state = if diagnostics_editor.is_diagnostic_selected(&citation, cx) {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                };
                div()
                    .flex_none()
                    .debug_selector(|| "diagnostic-row-checkbox".into())
                    .child(
                        Checkbox::new(checkbox_id, toggle_state).on_click(move |_, _, cx| {
                            diagnostics_editor.toggle_diagnostic_selection(citation.clone(), cx)
                        }),
                    )
                    .into_any_element()
            });

        h_flex()
            .max_w(max_width)
            .pl_1p5()
            .pr_0p5()
            .items_start()
            .gap_1()
            .border_l_2()
            .line_height(line_height)
            .bg(background_color)
            .border_color(border_color)
            .children(checkbox)
            .when(self.count > 1, |this| {
                this.child(
                    div()
                        .flex_none()
                        .mt_0p5()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(border_color)
                        .debug_selector(|| "diagnostic-count-badge".into())
                        .child(Label::new(format!("×{}", self.count)).size(LabelSize::Small)),
                )
            })
            .child(
                div().flex_1().min_w_0().child(
                    MarkdownElement::new(
                        self.markdown.clone(),
                        diagnostics_markdown_style(bcx.window, cx),
                    )
                    .code_block_renderer(markdown::CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                        border: false,
                    })
                    .on_url_click({
                        move |link, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    Self::open_link(editor, &diagnostics_editor, link, window, cx)
                                })
                                .ok();
                        }
                    }),
                ),
            )
            .when_some(self.citation.clone(), |this, citation| {
                this.child(
                    div()
                        .flex_none()
                        .debug_selector(|| "diagnostic-row-chat".into())
                        .child(
                            IconButton::new(chat_button_id, IconName::AgentChatRoundLine)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(t!("Add Diagnostic to Chat")))
                                .aria_label(t!("Add Diagnostic to Chat"))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(AddDiagnosticsToChat {
                                            citations: vec![citation.clone()],
                                            announce: false,
                                        }),
                                        cx,
                                    );
                                }),
                        ),
                )
            })
            .child(
                CopyButton::new(copy_button_id, self.copy_message.clone())
                    .tooltip_label(t!("Copy Diagnostic")),
            )
            .into_any_element()
    }

    pub fn open_link(
        editor: &mut Editor,
        diagnostics_editor: &Option<Arc<dyn DiagnosticsToolbarEditor>>,
        link: SharedString,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        if let Some((action, id)) = translation::parse_link(&link) {
            translation::handle_link(editor, action, id, cx);
            return;
        }
        let Some(diagnostic_link) = link.strip_prefix("file://#diagnostic-") else {
            let workspace = editor.workspace();
            editor::hover_popover::open_markdown_url(workspace, link, window, cx);
            return;
        };
        let Some((buffer_id, group_id, ix)) = maybe!({
            let mut parts = diagnostic_link.split('-');
            let buffer_id: u64 = parts.next()?.parse().ok()?;
            let group_id: usize = parts.next()?.parse().ok()?;
            let ix: usize = parts.next()?.parse().ok()?;
            Some((BufferId::new(buffer_id).ok()?, group_id, ix))
        }) else {
            return;
        };

        if let Some(diagnostics_editor) = diagnostics_editor {
            if let Some(diagnostic) = diagnostics_editor
                .get_diagnostics_for_buffer(buffer_id, cx)
                .into_iter()
                .filter(|d| d.diagnostic.group_id == group_id)
                .nth(ix)
            {
                let multibuffer = editor.buffer().read(cx);
                if let Some(anchor_range) = multibuffer
                    .snapshot(cx)
                    .buffer_anchor_range_to_anchor_range(diagnostic.range)
                {
                    Self::jump_to(editor, anchor_range, window, cx);
                    return;
                }
            }
        } else if let Some(diagnostic) = editor
            .snapshot(window, cx)
            .buffer_snapshot()
            .diagnostic_group(buffer_id, group_id)
            .nth(ix)
        {
            Self::jump_to(editor, diagnostic.range, window, cx)
        };
    }

    fn jump_to<I: ToOffset>(
        editor: &mut Editor,
        range: Range<I>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let snapshot = &editor.buffer().read(cx).snapshot(cx);
        let range = range.start.to_offset(snapshot)..range.end.to_offset(snapshot);

        editor.unfold_ranges(&[range.start..range.end], true, false, cx);
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([range.start..range.start]);
        });
        window.focus(&editor.focus_handle(cx), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use language::DiagnosticMessage;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_adapter_markdown_keeps_source_and_code_inline() {
        let diagnostic = Diagnostic {
            message: DiagnosticMessage::plain_with_adapter_markdown(
                "mismatched types\nexpected `usize`, found `char`",
                Some("mismatched types\n\nexpected `usize`, found `char`".into()),
            ),
            source: Some("rust-analyzer".to_string()),
            code: Some(lsp::NumberOrString::String("E0308".to_string())),
            ..Diagnostic::default()
        };

        let mut markdown = DiagnosticRenderer::markdown(&diagnostic);
        append_source_and_code(&mut markdown, &diagnostic);

        assert_eq!(
            markdown,
            "mismatched types\n\nexpected `usize`, found `char` (rust\\-analyzer E0308)"
        );
    }
}
