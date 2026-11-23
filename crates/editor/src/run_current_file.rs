//! Running and debugging the file in this editor.
//!
//! The commands come from `code_runner.executor_map` and `code_runner.debug_map`
//! in the settings, merged over the built-in defaults in the [`code_runner`]
//! crate. Running goes through the terminal panel, so the command and its output
//! land in a terminal that is reused for the same file; debugging builds first
//! and then starts a session with the configured adapter.
//!
//! Runs are immediate: the command is visible in the terminal, and "Run Current
//! File…" exists for the times it has to be edited first. A command edited that
//! way sticks to the editor until it is edited again.

use std::{path::PathBuf, sync::Arc};

use code_runner::{CodeRunnerSettings, CommandSource, DebugPlan, RunCommand, RunRequest};
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, WeakEntity, Window,
};
use i18n::t;
use menu::{Cancel, Confirm};
use project::{TaskSourceKind, WorktreeId};
use settings::{Settings, SettingsLocation};
use task::{DebugScenario, SaveStrategy, SharedTaskContext, Shell, TaskContext, TaskTemplate};
use ui::{AlertModal, Button, Color, Icon, IconName, Label, LabelSize, prelude::*};
use ui_input::InputField;
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    Item as _, ModalView, Toast, item::SaveOptions, notifications::NotificationId,
    tasks::ScheduledTaskResult,
};
use wu_actions::{DebugCurrentFile, RunCurrentFile, RunCurrentFileWithCommand};

use crate::Editor;

/// What the editor knows about the file it would run or debug.
struct CodeRunTarget {
    file: PathBuf,
    worktree_root: PathBuf,
    worktree_id: WorktreeId,
    rel_path: Arc<RelPath>,
    /// Closest directory above the file that holds a `CMakeLists.txt`.
    cmake_root: Option<PathBuf>,
}

impl CodeRunTarget {
    fn request(&self) -> RunRequest<'_> {
        RunRequest {
            file: &self.file,
            worktree_root: &self.worktree_root,
            cmake_root: self.cmake_root.as_deref(),
        }
    }
}

impl Editor {
    /// Runs the file in this editor, reusing a command edited earlier.
    pub fn run_current_file(
        &mut self,
        _: &RunCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.code_run_target(cx) else {
            self.show_no_run_command(cx);
            return;
        };
        let Some(command) = self
            .last_run_for(&target)
            .or_else(|| self.resolve_code_run(&target, cx))
        else {
            self.show_no_run_command(cx);
            return;
        };

        self.save_before_running(window, cx, move |editor, _window, cx| {
            editor.last_run = Some((target.file, command.clone()));
            editor.schedule_command(
                command,
                |result, _cx| {
                    if result == ScheduledTaskResult::SpawnFailed {
                        log::error!("Failed to run the command of the code runner");
                    }
                },
                cx,
            );
        });
    }

    /// Opens the run dialog, so the command can be edited before it runs.
    pub fn run_current_file_with_command(
        &mut self,
        _: &RunCurrentFileWithCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.code_run_target(cx) else {
            self.show_no_run_command(cx);
            return;
        };
        let Some(command) = self
            .last_run_for(&target)
            .or_else(|| self.resolve_code_run(&target, cx))
        else {
            self.show_no_run_command(cx);
            return;
        };

        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        let editor = cx.entity().downgrade();
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    RunCommandModal::new(editor, command, window, cx)
                });
            })
            .ok();
    }

    /// Builds the file with debug symbols and starts a debug session.
    pub fn debug_current_file(
        &mut self,
        _: &DebugCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.code_run_target(cx) else {
            self.show_no_debug_plan(cx);
            return;
        };
        let Some(plan) = self.resolve_code_debug(&target, cx) else {
            self.show_no_debug_plan(cx);
            return;
        };
        if !code_runner::debug_adapter_available(&plan.adapter) {
            let message = t!(
                "The {adapter} debug adapter needs gdb in PATH. Install gdb, add it to PATH, or point code_runner.debug_map at another adapter.",
                adapter = plan.adapter
            );
            self.show_code_runner_toast(message, cx);
            return;
        }

        let worktree_id = target.worktree_id;
        self.save_before_running(window, cx, move |editor, _window, cx| {
            editor.schedule_debug_build(plan, worktree_id, cx)
        });
    }

    /// Whether this file has a run command, which decides whether the tab
    /// shows the run button.
    pub fn can_run_current_file(&self, cx: &App) -> bool {
        self.code_run_target(cx)
            .and_then(|target| self.resolve_code_run(&target, cx).map(|_| ()))
            .is_some()
    }

    /// Whether this file has a debug plan, which decides whether the tab shows
    /// the debug button.
    pub fn can_debug_current_file(&self, cx: &App) -> bool {
        self.code_run_target(cx)
            .and_then(|target| self.resolve_code_debug(&target, cx).map(|_| ()))
            .is_some()
    }

    fn code_run_target(&self, cx: &App) -> Option<CodeRunTarget> {
        let project = self.project()?;
        let buffer = self.buffer().read(cx).as_singleton()?;
        let file = buffer.read(cx).file()?;
        let is_local = file.as_local().is_some();
        let abs_path = match file.as_local() {
            Some(local) => PathBuf::from(&*local.abs_path(cx)),
            None => file.full_path(cx),
        };

        let worktree_id = file.worktree_id(cx);
        let worktree = project.read(cx).worktree_for_id(worktree_id, cx)?;
        let worktree = worktree.read(cx);
        let worktree_root = worktree.abs_path().to_path_buf();
        let rel_path = file.path().clone();

        Some(CodeRunTarget {
            cmake_root: is_local
                .then(|| cmake_root_for(&rel_path, &worktree_root))
                .flatten(),
            file: abs_path,
            worktree_root,
            worktree_id,
            rel_path,
        })
    }

    fn resolve_code_run(&self, target: &CodeRunTarget, cx: &App) -> Option<RunCommand> {
        code_runner::resolve(&target.request(), self.code_runner_settings(target, cx))
    }

    fn resolve_code_debug(&self, target: &CodeRunTarget, cx: &App) -> Option<DebugPlan> {
        code_runner::resolve_debug(&target.request(), self.code_runner_settings(target, cx))
    }

    fn code_runner_settings<'a>(
        &self,
        target: &CodeRunTarget,
        cx: &'a App,
    ) -> &'a CodeRunnerSettings {
        CodeRunnerSettings::get(
            Some(SettingsLocation {
                worktree_id: target.worktree_id,
                path: target.rel_path.as_ref(),
            }),
            cx,
        )
    }

    /// The command used for this file last, if it is still the same file.
    fn last_run_for(&self, target: &CodeRunTarget) -> Option<RunCommand> {
        let (file, command) = self.last_run.as_ref()?;
        (file == &target.file).then(|| command.clone())
    }

    /// Saves this editor's buffers, then runs the closure. The save is awaited
    /// first so the compiler reads what is on screen even when this tab is not
    /// the active one.
    fn save_before_running(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let save = self.save(SaveOptions::default(), project, window, cx);
        cx.spawn_in(window, async move |editor, cx| {
            save.await.log_err();
            editor
                .update_in(cx, |editor, window, cx| then(editor, window, cx))
                .ok();
        })
        .detach();
    }

    fn schedule_debug_build(
        &mut self,
        plan: DebugPlan,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        let active_buffer = self.buffer().read(cx).as_singleton();
        let task_context = SharedTaskContext::from(TaskContext {
            cwd: Some(plan.build.cwd.clone()),
            ..TaskContext::default()
        });
        let program = plan.program.to_string_lossy().into_owned();
        let cwd = plan.build.cwd.to_string_lossy().into_owned();
        let label: SharedString = plan.label().into();
        let adapter: SharedString = plan.adapter.into();

        self.schedule_command(
            plan.build,
            move |result, cx| {
                if result != ScheduledTaskResult::Success {
                    return;
                }
                let scenario = DebugScenario {
                    adapter,
                    label,
                    build: None,
                    config: serde_json::json!({
                        "request": "launch",
                        "program": program,
                        "cwd": cwd,
                    }),
                    tcp_connection: None,
                };
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.start_debug_session(
                            scenario,
                            task_context,
                            active_buffer,
                            Some(worktree_id),
                            window,
                            cx,
                        );
                    })
                    .ok();
            },
            cx,
        );
    }

    /// Runs a command in the terminal panel, reusing the terminal of the same
    /// command, and reports how it ended to `on_complete`.
    fn schedule_command(
        &mut self,
        command: RunCommand,
        on_complete: impl FnOnce(ScheduledTaskResult, &mut AsyncWindowContext) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };

        // The command line is fully substituted, so the template only has to
        // carry it to the terminal. `cwd` decides where relative paths land.
        let cwd = command.cwd.clone();
        let label = command.label();
        let task_context = TaskContext {
            cwd: Some(cwd.clone()),
            ..TaskContext::default()
        };
        let template = TaskTemplate {
            label,
            command: command.command_line,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            save: SaveStrategy::None,
            use_new_terminal: false,
            allow_concurrent_runs: false,
            show_summary: true,
            show_command: true,
            shell: Shell::System,
            ..TaskTemplate::default()
        };
        let Some(resolved_task) = template.resolve_task("code_runner", &task_context) else {
            return;
        };

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.schedule_resolved_task_with_completion(
                    TaskSourceKind::UserInput,
                    resolved_task,
                    true,
                    on_complete,
                    window,
                    cx,
                );
            })
            .ok();
    }

    fn show_no_run_command(&self, cx: &mut Context<Self>) {
        self.show_code_runner_toast(
            t!("No run command for this file. Add one under code_runner.executor_map in your settings."),
            cx,
        );
    }

    fn show_no_debug_plan(&self, cx: &mut Context<Self>) {
        self.show_code_runner_toast(
            t!("No debug plan for this file. Add one under code_runner.debug_map in your settings."),
            cx,
        );
    }

    fn show_code_runner_toast(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        let message = message.into();
        workspace
            .update(cx, |workspace, cx| {
                let id = NotificationId::unique::<RunCommand>();
                workspace.show_toast(Toast::new(id, message.to_string()), cx);
            })
            .ok();
    }
}

/// Finds the closest directory that holds a `CMakeLists.txt`, from the file's
/// directory up to the worktree root.
fn cmake_root_for(file_path: &RelPath, worktree_root: &std::path::Path) -> Option<PathBuf> {
    let mut directory = file_path.parent();
    while let Some(rel_dir) = directory {
        let absolute = worktree_root.join(rel_dir.as_std_path());
        if absolute.join("CMakeLists.txt").is_file() {
            return Some(absolute);
        }
        directory = rel_dir.parent();
    }
    None
}

struct RunCommandModal {
    editor: WeakEntity<Editor>,
    command: RunCommand,
    command_input: Entity<InputField>,
}

impl RunCommandModal {
    fn new(
        editor: WeakEntity<Editor>,
        command: RunCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let command_input = cx.new(|cx| InputField::new(window, cx, "Command"));
        command_input.update(cx, |input, cx| {
            input.set_text(&command.command_line, window, cx);
            input.editor().select_all(window, cx);
        });

        Self {
            editor,
            command,
            command_input,
        }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command_line = self.command_input.read(cx).text(cx).trim().to_owned();
        if command_line.is_empty() {
            return;
        }
        let command = self.command.clone().with_edited_command_line(command_line);
        let editor = self.editor.clone();
        cx.emit(DismissEvent);

        editor
            .update(cx, |editor, cx| {
                let Some(target) = editor.code_run_target(cx) else {
                    return;
                };
                editor.save_before_running(window, cx, move |editor, _window, cx| {
                    editor.last_run = Some((target.file, command.clone()));
                    editor.schedule_command(
                        command,
                        |result, _cx| {
                            if result == ScheduledTaskResult::SpawnFailed {
                                log::error!("Failed to run the command of the code runner");
                            }
                        },
                        cx,
                    );
                });
            })
            .ok();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for RunCommandModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The modal layer focuses the modal's handle when it opens, so handing
        // it the command field's handle puts the caret in the command.
        self.command_input.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RunCommandModal {}

impl ModalView for RunCommandModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for RunCommandModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let origin: SharedString = match self.command.source {
            CommandSource::BuiltIn => t!("built-in command"),
            CommandSource::Settings => "code_runner.executor_map".into(),
            CommandSource::Edited => t!("edited for this run"),
        };
        let directory = self.command.cwd.display().to_string();

        AlertModal::new("run-current-file-modal")
            .width(rems(36.))
            .key_context("RunCurrentFileModal")
            .track_focus(&self.command_input.read(cx).focus_handle(cx))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| this.run(window, cx)))
            .on_action(cx.listener(|this, _: &Cancel, _window, cx| this.dismiss(cx)))
            .header(
                h_flex()
                    .p_3()
                    .gap_2()
                    .child(Icon::new(IconName::PlayFilled).color(Color::Accent))
                    .child(
                        v_flex()
                            .child(Label::new(t!(
                                "Run {filename}",
                                filename = self.command.filename
                            )))
                            .child(
                                Label::new(format!("{} · {origin}", self.command.language))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(t!("Command"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.command_input.clone())
                    .child(
                        Label::new(t!("Runs in {directory}", directory = directory))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .footer(
                h_flex()
                    .p_3()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("cancel", t!("Cancel"))
                            .color(Color::Muted)
                            .on_click(cx.listener(|this, _, _window, cx| this.dismiss(cx))),
                    )
                    .child(
                        Button::new("run", t!("Run"))
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    ),
            )
    }
}
