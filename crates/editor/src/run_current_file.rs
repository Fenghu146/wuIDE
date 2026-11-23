//! "Run Current File": turns the file in this editor into a shell command and
//! runs it in the terminal panel.
//!
//! The command comes from `code_runner.executor_map` in the settings, merged
//! over the built-in defaults in the [`code_runner`] crate. The first run of a
//! file opens a dialog that shows the command with its origin and lets it be
//! edited; later runs of the same file rerun the command that was used last.

use std::{path::PathBuf, sync::Arc};

use code_runner::{CodeRunnerSettings, CommandSource, RunCommand, RunRequest};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, WeakEntity, Window,
};
use i18n::t;
use menu::{Cancel, Confirm};
use project::{TaskSourceKind, WorktreeId};
use settings::{Settings, SettingsLocation};
use task::{SaveStrategy, Shell, TaskContext, TaskTemplate};
use ui::{AlertModal, Button, Color, Icon, IconName, Label, LabelSize, prelude::*};
use ui_input::InputField;
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    Item as _, ModalView, Toast, Workspace, item::SaveOptions, notifications::NotificationId,
    tasks::ScheduledTaskResult,
};
use wu_actions::RunCurrentFile;

use crate::Editor;

/// What the editor knows about the file it would run.
struct CodeRunTarget {
    file: PathBuf,
    worktree_root: PathBuf,
    worktree_id: WorktreeId,
    rel_path: Arc<RelPath>,
    /// Closest directory above the file that holds a `CMakeLists.txt`.
    cmake_root: Option<PathBuf>,
}

impl Editor {
    /// Runs the file in this editor, or asks for the command the first time.
    pub fn run_current_file(
        &mut self,
        action: &RunCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.code_run_target(cx) else {
            self.show_no_run_command_toast(cx);
            return;
        };

        if !action.edit
            && let Some((file, command)) = self.last_run.clone()
            && file == target.file
        {
            self.spawn_code_run(command, window, cx);
            return;
        }

        let Some(command) = self.resolve_code_run(&target, cx) else {
            self.show_no_run_command_toast(cx);
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

    /// Whether this file has a run command, which decides whether the tab
    /// shows the run button.
    pub fn can_run_current_file(&self, cx: &App) -> bool {
        self.code_run_target(cx)
            .and_then(|target| self.resolve_code_run(&target, cx).map(|_| ()))
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
        let settings = CodeRunnerSettings::get(
            Some(SettingsLocation {
                worktree_id: target.worktree_id,
                path: target.rel_path.as_ref(),
            }),
            cx,
        );
        code_runner::resolve(
            &RunRequest {
                file: &target.file,
                worktree_root: &target.worktree_root,
                cmake_root: target.cmake_root.as_deref(),
            },
            settings,
        )
    }

    /// Saves this editor's buffers, then runs the command in the terminal
    /// panel. The save is awaited first so the compiler reads what is on
    /// screen even when this tab is not the active one.
    fn spawn_code_run(&mut self, command: RunCommand, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };

        let save = self.save(SaveOptions::default(), project, window, cx);
        cx.spawn_in(window, async move |editor, cx| {
            save.await.log_err();
            editor
                .update_in(cx, |editor, _window, cx| {
                    if let Some(target) = editor.code_run_target(cx) {
                        editor.last_run = Some((target.file, command.clone()));
                    }
                    editor.schedule_code_run_task(&workspace, command, cx);
                })
                .ok();
        })
        .detach();
    }

    fn schedule_code_run_task(
        &mut self,
        workspace: &WeakEntity<Workspace>,
        command: RunCommand,
        cx: &mut Context<Self>,
    ) {
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
                    |result, _cx| {
                        if result == ScheduledTaskResult::SpawnFailed {
                            log::error!("Failed to run the command of the code runner");
                        }
                    },
                    window,
                    cx,
                );
            })
            .ok();
    }

    fn show_no_run_command_toast(&self, cx: &mut Context<Self>) {
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        workspace
            .update(cx, |workspace, cx| {
                let id = NotificationId::unique::<RunCommand>();
                let message = t!(
                    "No run command for this file. Add one under code_runner.executor_map in your settings."
                );
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
        editor
            .update(cx, |editor, cx| editor.spawn_code_run(command, window, cx))
            .ok();
        cx.emit(DismissEvent);
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
