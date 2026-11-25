//! The run controls in the editor's top-right toolbar.
//!
//! Run, stop and debug sit next to the editor's other quick actions, away from
//! the file tab, so they are where the eye looks for them and they keep their
//! place while the tab bar scrolls. Debug stays visible but disabled when the
//! debug adapter is missing, and its tooltip says what is missing.

use editor::Editor;
use gpui::{
    App, Context, EventEmitter, Focusable, Render, Subscription, WeakEntity, Window,
    prelude::*,
};
use i18n::t;
use ui::{ButtonStyle, IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{
    ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace, item::ItemHandle,
};
use wu_actions::{CompileCurrentFile, DebugCurrentFile, RunCurrentFile, StopCurrentFile};
pub struct RunToolbar {
    active_item: Option<Box<dyn ItemHandle>>,
    /// Redraws the toolbar when the active editor's run state changes, so Stop
    /// appears and disappears with the program.
    _editor_observation: Option<Subscription>,
    _workspace: WeakEntity<Workspace>,
}

impl RunToolbar {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            active_item: None,
            _editor_observation: None,
            _workspace: workspace.weak_handle(),
        }
    }
}

impl Render for RunToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self
            .active_item
            .as_deref()
            .and_then(|item| item.downcast::<Editor>())
        else {
            return div().into_any_element();
        };

        let (can_run, can_compile, can_debug, is_running, debug_blocked) =
            editor.update(cx, |editor, cx| {
                (
                    editor.can_run_current_file(cx),
                    editor.can_compile_current_file(cx),
                    editor.can_debug_current_file(cx),
                    editor.code_run_is_active(),
                    editor.code_debug_unavailable_reason(cx),
                )
            });
        let focus_handle = editor.read(cx).focus_handle(cx);

        // A right-aligned toolbar lays its children out in reverse, so the
        // debug button is listed first to end up rightmost, and compile comes
        // after run to sit on its left.
        let mut controls = Vec::with_capacity(4);

        if can_debug {
            let reason = debug_blocked;
            controls.push(
                IconButton::new("run-toolbar-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .disabled(reason.is_some())
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |window, cx| match reason.clone() {
                            Some(reason) => Tooltip::text(reason)(window, cx),
                            None => Tooltip::for_action_in(
                                t!("Debug Current File"),
                                &DebugCurrentFile,
                                &focus_handle,
                                cx,
                            ),
                        }
                    })
                    .on_click({
                        let editor = editor.downgrade();
                        move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.debug_current_file(&DebugCurrentFile, window, cx)
                                })
                                .ok();
                        }
                    })
                    .into_any_element(),
            );
        }

        if is_running {
            controls.push(
                IconButton::new("run-toolbar-stop", IconName::Stop)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                t!("Stop Running Code"),
                                &StopCurrentFile,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.downgrade();
                        move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.stop_current_file(&StopCurrentFile, window, cx)
                                })
                                .ok();
                        }
                    })
                    .into_any_element(),
            );
        }

        if can_run {
            controls.push(
                IconButton::new("run-toolbar-run", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_window, cx| {
                            Tooltip::for_action_in(
                                t!("Run Current File"),
                                &RunCurrentFile,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click({
                        let editor = editor.downgrade();
                        move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.run_current_file(&RunCurrentFile, window, cx)
                                })
                                .ok();
                        }
                    })
                    .into_any_element(),
            );
        }

        if can_compile {
            controls.push(
                IconButton::new("run-toolbar-compile", IconName::Build)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .tooltip(move |_window, cx| {
                        Tooltip::for_action_in(
                            t!("Compile Current File"),
                            &CompileCurrentFile,
                            &focus_handle,
                            cx,
                        )
                    })
                    .on_click({
                        let editor = editor.downgrade();
                        move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.compile_current_file(&CompileCurrentFile, window, cx)
                                })
                                .ok();
                        }
                    })
                    .into_any_element(),
            );
        }

        h_flex()
            .id("run toolbar")
            .gap(DynamicSpacing::Base01.rems(cx))
            .children(controls)
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for RunToolbar {}

impl ToolbarItemView for RunToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let location = match active_pane_item {
            Some(item) if item.downcast::<Editor>().is_some() => ToolbarItemLocation::PrimaryRight,
            _ => ToolbarItemLocation::Hidden,
        };
        self._editor_observation = None;
        self.active_item = active_pane_item.map(ItemHandle::boxed_clone);
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self._editor_observation = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
        }
        location
    }
}

/// The run entry point in the status bar, so the run state stays visible even
/// when the toolbar is scrolled out of the way.
pub struct RunStatusItem {
    /// Weak, because the entry outlives the editor it was pointed at: a closed
    /// file must not stay alive just because the status bar remembers it.
    active_editor: Option<WeakEntity<Editor>>,
    _observation: Option<Subscription>,
}

impl Default for RunStatusItem {
    fn default() -> Self {
        Self::new()
    }
}

impl RunStatusItem {
    pub fn new() -> Self {
        Self {
            active_editor: None,
            _observation: None,
        }
    }
}

impl Render for RunStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self
            .active_editor
            .as_ref()
            .and_then(|editor| editor.upgrade())
        else {
            return div().into_any_element();
        };
        let (can_run, is_running) = editor.update(cx, |editor, cx| {
            (editor.can_run_current_file(cx), editor.code_run_is_active())
        });
        if !can_run && !is_running {
            return div().into_any_element();
        }

        let (label, icon) = if is_running {
            (t!("Stop Running Code"), IconName::Stop)
        } else {
            (t!("Run Current File"), IconName::PlayFilled)
        };

        div()
            .child(
                IconButton::new("run-status", icon)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(label))
                    .on_click({
                        let editor = editor.downgrade();
                        move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    if editor.code_run_is_active() {
                                        editor.stop_current_file(&StopCurrentFile, window, cx);
                                    } else {
                                        editor.run_current_file(&RunCurrentFile, window, cx);
                                    }
                                })
                                .ok();
                        }
                    }),
            )
            .into_any_element()
    }
}

impl workspace::StatusItemView for RunStatusItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The entry stays on the editor that was last active instead of
        // disappearing when the terminal a run just opened takes the focus:
        // that is exactly when Stop has to be within reach.
        let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) else {
            return;
        };
        if self
            .active_editor
            .as_ref()
            .is_some_and(|active| active.entity_id() == editor.entity_id())
        {
            return;
        }
        self._observation = Some(cx.observe_in(&editor, window, |_, _, _, cx| cx.notify()));
        self.active_editor = Some(editor.downgrade());
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
