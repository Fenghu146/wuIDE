use crate::{BufferDiagnosticsEditor, ProjectDiagnosticsEditor, ToggleDiagnosticsRefresh};
use gpui::{Context, EventEmitter, ParentElement, Render, Subscription, Window};
use i18n::t;
use language::DiagnosticEntry;
use text::{Anchor, BufferId};
use ui::{IconName, Tooltip, prelude::*};
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle};
use wu_actions::{AddDiagnosticsToChat, DiagnosticCitation, buffer_search};

pub struct ToolbarControls {
    editor: Option<Box<dyn DiagnosticsToolbarEditor>>,
    /// Keeps the toolbar in step with the panel: `Add Selected (N)` has to
    /// appear and count up as the user ticks rows, which only notifies the
    /// panel itself.
    _observe_editor: Option<Subscription>,
}

pub(crate) trait DiagnosticsToolbarEditor: Send + Sync {
    /// Informs the toolbar whether warnings are included in the diagnostics.
    fn include_warnings(&self, cx: &App) -> bool;
    /// Toggles whether warning diagnostics should be displayed by the
    /// diagnostics editor.
    fn toggle_warnings(&self, window: &mut Window, cx: &mut App);
    /// Indicates whether the diagnostics editor is currently updating the
    /// diagnostics.
    fn is_updating(&self, cx: &App) -> bool;
    /// Requests that the diagnostics editor stop updating the diagnostics.
    fn stop_updating(&self, cx: &mut App);
    /// Requests that the diagnostics editor updates the displayed diagnostics
    /// with the latest information.
    fn refresh_diagnostics(&self, window: &mut Window, cx: &mut App);
    /// Returns a list of diagnostics for the provided buffer id.
    fn get_diagnostics_for_buffer(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Vec<DiagnosticEntry<Anchor>>;
    /// Returns the diagnostics the panel is showing, for the "add all" button.
    fn diagnostic_citations(&self, cx: &App) -> Vec<DiagnosticCitation>;

    /// Whether this view lets the user tick diagnostic rows. Only the
    /// project-wide panel does; the buffer view cites one diagnostic at a time.
    fn supports_diagnostic_selection(&self) -> bool {
        false
    }

    /// Whether the row showing `citation` is ticked.
    fn is_diagnostic_selected(&self, _citation: &DiagnosticCitation, _cx: &App) -> bool {
        false
    }

    /// Ticks or unticks the row showing `citation`.
    fn toggle_diagnostic_selection(&self, _citation: DiagnosticCitation, _cx: &mut App) {}

    /// The ticked diagnostics, in panel order, for the "add selected" button.
    fn selected_diagnostic_citations(&self, _cx: &App) -> Vec<DiagnosticCitation> {
        Vec::new()
    }

    /// Drops every tick after the selection became a chat reference.
    fn clear_diagnostic_selection(&self, _cx: &mut App) {}

    /// Adds the ticked rows to the chat the user is looking at, and keeps the
    /// ticks when there is no chat to add them to.
    fn add_selected_diagnostics_to_chat(&self, _window: &mut Window, _cx: &mut App) {}
}

impl Render for ToolbarControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut include_warnings = false;
        let mut is_updating = false;

        match &self.editor {
            Some(editor) => {
                include_warnings = editor.include_warnings(cx);
                is_updating = editor.is_updating(cx);
            }
            None => {}
        }
        let citations = self
            .editor
            .as_ref()
            .map(|editor| editor.diagnostic_citations(cx))
            .unwrap_or_default();
        let selected = self
            .editor
            .as_ref()
            .map(|editor| editor.selected_diagnostic_citations(cx))
            .unwrap_or_default();

        let (warning_tooltip, warning_color) = if include_warnings {
            (t!("Exclude Warnings"), Color::Warning)
        } else {
            (t!("Include Warnings"), Color::Disabled)
        };

        h_flex()
            .gap_1()
            .child({
                IconButton::new("toggle_search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        t!("Buffer Search"),
                        &buffer_search::Deploy::find(),
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(buffer_search::Deploy::find()), cx);
                    })
            })
            .map(|div| {
                if is_updating {
                    div.child(
                        IconButton::new("stop-updating", IconName::Stop)
                            .icon_color(Color::Error)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title(
                                t!("Stop Diagnostics Update"),
                                &ToggleDiagnosticsRefresh,
                            ))
                            .on_click(cx.listener(move |toolbar_controls, _, _, cx| {
                                if let Some(editor) = toolbar_controls.editor() {
                                    editor.stop_updating(cx);
                                    cx.notify();
                                }
                            })),
                    )
                } else {
                    div.child(
                        IconButton::new("refresh-diagnostics", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title(
                                t!("Refresh Diagnostics"),
                                &ToggleDiagnosticsRefresh,
                            ))
                            .on_click(cx.listener({
                                move |toolbar_controls, _, window, cx| {
                                    if let Some(editor) = toolbar_controls.editor() {
                                        editor.refresh_diagnostics(window, cx)
                                    }
                                }
                            })),
                    )
                }
            })
            .child(
                IconButton::new("toggle-warnings", IconName::Warning)
                    .icon_color(warning_color)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(warning_tooltip))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(editor) = &this.editor {
                            editor.toggle_warnings(window, cx)
                        }
                    })),
            )
            .when(!selected.is_empty(), |this| {
                let count = selected.len();
                this.child(
                    Button::new(
                        "add-selected-diagnostics-to-chat",
                        t!("Add Selected to Chat ({count})", count = count),
                    )
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(t!(
                        "The agent gets one reference chip listing the ticked diagnostics."
                    )))
                    .on_click(cx.listener(|toolbar_controls, _, window, cx| {
                        if let Some(editor) = toolbar_controls.editor() {
                            editor.add_selected_diagnostics_to_chat(window, cx);
                        }
                    })),
                )
            })
            .when(!citations.is_empty(), |this| {
                let citations = citations.clone();
                this.child(
                    IconButton::new("add-all-diagnostics-to-chat", IconName::AgentChatRoundLine)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(t!("Add All Diagnostics to Chat")))
                        .aria_label(t!("Add All Diagnostics to Chat"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(AddDiagnosticsToChat {
                                    citations: citations.clone(),
                                    announce: false,
                                }),
                                cx,
                            );
                        }),
                )
            })
    }
}

impl EventEmitter<ToolbarItemEvent> for ToolbarControls {}

impl ToolbarItemView for ToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.editor = None;
        self._observe_editor = None;
        let Some(pane_item) = active_pane_item.as_ref() else {
            return ToolbarItemLocation::Hidden;
        };
        if let Some(editor) = pane_item.downcast::<ProjectDiagnosticsEditor>() {
            self.editor = Some(Box::new(editor.downgrade()));
            self._observe_editor = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
            ToolbarItemLocation::PrimaryRight
        } else if let Some(editor) = pane_item.downcast::<BufferDiagnosticsEditor>() {
            self.editor = Some(Box::new(editor.downgrade()));
            self._observe_editor = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Default for ToolbarControls {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolbarControls {
    pub fn new() -> Self {
        ToolbarControls {
            editor: None,
            _observe_editor: None,
        }
    }

    fn editor(&self) -> Option<&dyn DiagnosticsToolbarEditor> {
        self.editor.as_deref()
    }
}
