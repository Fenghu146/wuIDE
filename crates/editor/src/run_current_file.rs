//! Running and debugging the file in this editor.
//!
//! The commands come from `code_runner.executor_map` and `code_runner.debug_map`
//! in the settings, merged over the built-in defaults in the [`code_runner`]
//! crate. A run is a list of steps: the build and the program are separate
//! spawns, so each has its own terminal, exit code and output, and nothing
//! depends on a shell `&&` that PowerShell 5.1 does not have.
//!
//! Every step goes through the terminal panel, which is also what makes a run
//! stoppable. Nothing here returns silently: a file that cannot run says why.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use code_runner::{
    CodeRunnerSettings, CommandSource, DebugPlan, Program, RunError, RunPlan, RunRequest, RunStep,
    scan_directory,
};
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
use wu_actions::{DebugCurrentFile, RunCurrentFile, RunCurrentFileWithCommand, StopCurrentFile};

use crate::Editor;

/// How long a check for the debug adapter is trusted before `PATH` is read
/// again. Toolbars ask on every frame, and the answer only changes when the
/// user installs a tool.
const ADAPTER_CHECK_TTL: Duration = Duration::from_secs(2);

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
    fn request(&self, language: &str) -> RunRequest<'_> {
        let directory = self
            .file
            .parent()
            .unwrap_or(&self.worktree_root)
            .to_path_buf();
        RunRequest {
            file: &self.file,
            worktree_root: &self.worktree_root,
            cmake_root: self.cmake_root.as_deref(),
            directory: scan_directory(&directory, language),
            cmake_available: code_runner::tool_available("cmake"),
        }
    }
}

/// A run that is in progress: the terminal labels Stop has to reach.
#[derive(Clone)]
pub struct ActiveCodeRun {
    labels: Vec<String>,
}

/// What the last step of a plan is for.
enum PlanGoal {
    Run,
    Debug {
        adapter: SharedString,
        label: SharedString,
        task_context: SharedTaskContext,
        buffer: Option<Entity<language::Buffer>>,
        program: Program,
    },
}

/// The plan a step chain belongs to, so the chain can continue after each
/// spawn finishes.
struct PlanInFlight {
    plan: RunPlan,
    next_step: usize,
    worktree_id: WorktreeId,
    goal: PlanGoal,
}

/// What a run started from the toolbar, the menu or a keybinding is for.
#[derive(Clone, Copy)]
enum CodePlanGoal {
    Run,
    Debug,
}

impl Editor {
    /// Runs the file in this editor: the build first, then the program.
    pub fn run_current_file(
        &mut self,
        _: &RunCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_code_plan(CodePlanGoal::Run, window, cx);
    }

    /// Builds with debug symbols and starts a debug session.
    pub fn debug_current_file(
        &mut self,
        _: &DebugCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_code_plan(CodePlanGoal::Debug, window, cx);
    }

    /// Stops the program the code runner started.
    pub fn stop_current_file(
        &mut self,
        _: &StopCurrentFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = self.code_run.take() else {
            return;
        };
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        if workspace
            .update(cx, |workspace, cx| {
                for label in &running.labels {
                    workspace.kill_task_terminal(label, cx);
                }
            })
            .is_err()
        {
            log::info!("code runner: the workspace is gone, so there is nothing to stop");
        }
        cx.notify();
    }

    fn start_code_plan(
        &mut self,
        goal: CodePlanGoal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let debug = matches!(goal, CodePlanGoal::Debug);
        let Some(target) = self.code_run_target(cx) else {
            self.show_code_runner_message(
                t!("This file is not in a project, so there is nothing to run. Open the folder that holds it."),
                cx,
            );
            return;
        };
        if debug {
            if let Some(language) = code_runner::language_id_for_path(&target.file)
                && let Some(entry) = self
                    .code_runner_settings(&target, cx)
                    .debug_entry(&language)
                && !self.debug_adapter_available(&entry.adapter)
            {
                self.show_code_runner_message(
                    t!("Debugging needs gdb in PATH. Install gdb, add it to PATH, or point code_runner.debug_map at another adapter."),
                    cx,
                );
                return;
            }
        }

        // A plan edited in the run dialog sticks to the file until it is
        // edited again; everything else is resolved after saving, so a `main`
        // that was just typed is part of the scan.
        let reused = (!debug)
            .then(|| self.last_run_for(&target))
            .flatten()
            .filter(|plan| plan.source == CommandSource::Edited);

        let worktree_id = target.worktree_id;
        self.save_before_running(window, cx, move |editor, window, cx| {
            let (plan, debug_plan) = match resolve_for(editor, &target, debug, reused, cx) {
                Ok(plan) => plan,
                Err(message) => {
                    editor.show_code_runner_message(message, cx);
                    return;
                }
            };
            let goal = match debug_plan {
                Some(DebugPlan {
                    adapter, program, ..
                }) => PlanGoal::Debug {
                    adapter: adapter.into(),
                    label: plan.filename.clone().into(),
                    task_context: SharedTaskContext::from(TaskContext {
                        cwd: Some(plan.root.clone()),
                        ..TaskContext::default()
                    }),
                    buffer: editor.buffer().read(cx).as_singleton(),
                    program,
                },
                None => PlanGoal::Run,
            };
            editor.last_run = Some((target.file.clone(), plan.clone()));
            editor.start_plan(plan, worktree_id, goal, window, cx);
        });
    }

    /// Runs the steps of a plan in order, each as its own terminal task, and
    /// stops at the first failure so a broken build never runs a stale binary.
    fn start_plan(
        &mut self,
        plan: RunPlan,
        worktree_id: WorktreeId,
        goal: PlanGoal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.code_run = Some(ActiveCodeRun {
            labels: plan.steps.iter().map(|step| step.label.clone()).collect(),
        });
        cx.notify();
        self.advance_plan(
            PlanInFlight {
                plan,
                next_step: 0,
                worktree_id,
                goal,
            },
            window,
            cx,
        );
    }

    fn advance_plan(
        &mut self,
        mut in_flight: PlanInFlight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(step) = in_flight.plan.steps.get(in_flight.next_step).cloned() else {
            self.finish_plan(in_flight, window, cx);
            return;
        };
        in_flight.next_step += 1;

        let editor = cx.entity().downgrade();
        self.run_step(
            step,
            move |result, cx| {
                editor
                    .update_in(cx, |editor, window, cx| {
                        if result == ScheduledTaskResult::Success {
                            editor.advance_plan(in_flight, window, cx);
                        } else {
                            editor.code_run = None;
                            cx.notify();
                        }
                    })
                    .ok();
            },
            window,
            cx,
        );
    }

    fn finish_plan(
        &mut self,
        mut in_flight: PlanInFlight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A CMake project's binary only appears once the build has run.
        if let Some(binary) = in_flight.plan.pending_binary.clone() {
            let is_windows = self.settings_for_active_file(cx).is_windows();
            match code_runner::cmake_run_step(&in_flight.plan.root, &binary, is_windows) {
                Ok(step) => {
                    if let Some(code_run) = self.code_run.as_mut() {
                        code_run.labels.push(step.label.clone());
                    }
                    in_flight.plan.steps.push(step);
                    in_flight.plan.pending_binary = None;
                    self.advance_plan(in_flight, window, cx);
                }
                Err(error) => {
                    self.code_run = None;
                    cx.notify();
                    self.show_code_runner_message(run_error_message(&error), cx);
                }
            }
            return;
        }

        self.code_run = None;
        cx.notify();

        let PlanGoal::Debug {
            adapter,
            label,
            task_context,
            buffer,
            program,
        } = in_flight.goal
        else {
            return;
        };

        let program = match program {
            Program::Path(path) => Some(path),
            Program::Cmake(binary) => {
                code_runner::find_cmake_binary(&binary.build_dir, binary.target.as_deref())
            }
        };
        let Some(program) = program else {
            self.show_code_runner_message(
                t!("The build finished, but its executable was not found. Point code_runner.debug_map at the binary."),
                cx,
            );
            return;
        };

        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        let scenario = DebugScenario {
            adapter,
            label,
            build: None,
            config: serde_json::json!({
                "request": "launch",
                "program": program.to_string_lossy(),
                "cwd": in_flight.plan.root.to_string_lossy(),
            }),
            tcp_connection: None,
        };
        let worktree_id = in_flight.worktree_id;
        // Same reason as in `run_step`: `update_in` cannot reach a window from
        // inside a window update, and the build's completion callback runs in
        // one. The window this editor is drawing into is the one to use.
        let started = workspace.update(cx, |workspace, cx| {
            workspace.start_debug_session(
                scenario,
                task_context,
                buffer,
                Some(worktree_id),
                window,
                cx,
            );
        });
        if started.is_err() {
            log::info!("code runner: the workspace closed before the debug session started");
        }
    }

    /// Whether a code run is in flight, which is what the stop button shows.
    pub fn code_run_is_active(&self) -> bool {
        self.code_run.is_some()
    }

    /// Why the debug button is disabled for this file even though it has a
    /// debug plan, such as a missing `gdb`.
    pub fn code_debug_unavailable_reason(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<SharedString> {
        let target = self.code_run_target(cx)?;
        let language = code_runner::language_id_for_path(&target.file)?;
        let adapter = self
            .code_runner_settings(&target, cx)
            .debug_entry(&language)?
            .adapter
            .clone();
        if self.debug_adapter_available(&adapter) {
            return None;
        }
        Some(t!(
            "Debugging needs gdb in PATH. Install gdb, add it to PATH, or point code_runner.debug_map at another adapter."
        ))
    }

    /// Whether the adapter can start, cached for a moment because toolbars ask
    /// on every frame.
    pub fn debug_adapter_available(&mut self, adapter: &str) -> bool {
        let now = Instant::now();
        if let Some((checked, available)) = self.debug_adapter_check
            && now.duration_since(checked) < ADAPTER_CHECK_TTL
        {
            return available;
        }
        let available = code_runner::debug_adapter_available(adapter);
        self.debug_adapter_check = Some((now, available));
        available
    }

    /// Opens the run dialog, so the steps can be edited before they run.
    pub fn run_current_file_with_command(
        &mut self,
        _: &RunCurrentFileWithCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.code_run_target(cx) else {
            self.show_code_runner_message(
                t!("This file is not in a project, so there is nothing to run. Open the folder that holds it."),
                cx,
            );
            return;
        };
        let plan = self.last_run_for(&target).or_else(|| {
            let language = code_runner::language_id_for_path(&target.file)?;
            let request = target.request(&language);
            code_runner::resolve(&request, self.code_runner_settings(&target, cx))
                .inspect_err(|error| log::info!("code runner: {error:?}"))
                .ok()
        });
        let Some(plan) = plan else {
            self.show_code_runner_message(
                t!("There is no run command for this file type."),
                cx,
            );
            return;
        };

        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        let editor = cx.entity().downgrade();
        if workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    RunCommandModal::new(editor, plan, window, cx)
                });
            })
            .is_err()
        {
            log::info!("code runner: the workspace closed before the run dialog opened");
        }
    }

    /// Whether this file type has a run command, which decides whether the
    /// run button shows.
    pub fn can_run_current_file(&self, cx: &App) -> bool {
        self.code_run_target(cx)
            .and_then(|target| code_runner::language_id_for_path(&target.file))
            .is_some_and(|language| {
                self.settings_for_active_file(cx)
                    .run_case(&language)
                    .is_some()
            })
    }

    /// Whether this file type has a debug plan, which decides whether the
    /// debug button shows.
    pub fn can_debug_current_file(&self, cx: &App) -> bool {
        self.code_run_target(cx)
            .and_then(|target| code_runner::language_id_for_path(&target.file))
            .is_some_and(|language| {
                self.settings_for_active_file(cx)
                    .debug_entry(&language)
                    .is_some()
            })
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
            cmake_root: is_local.then(|| cmake_root_for(&abs_path, &worktree_root)).flatten(),
            file: abs_path,
            worktree_root,
            worktree_id,
            rel_path,
        })
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

    /// The settings that apply in the directory of the active file, which is
    /// where a plan's Windows-only and per-worktree choices come from.
    fn settings_for_active_file<'a>(&self, cx: &'a App) -> &'a CodeRunnerSettings {
        match self.code_run_target(cx) {
            Some(target) => self.code_runner_settings(&target, cx),
            None => CodeRunnerSettings::get_global(cx),
        }
    }

    /// The plan used for this file last, if it is still the same file.
    fn last_run_for(&self, target: &CodeRunTarget) -> Option<RunPlan> {
        let (file, plan) = self.last_run.as_ref()?;
        (file == &target.file).then(|| plan.clone())
    }

    /// Saves this editor's buffers, then runs the closure. The save is awaited
    /// first so the compiler and the source scan see what is on screen even
    /// when this tab is not the active one.
    fn save_before_running(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(project) = self.project.clone() else {
            self.show_code_runner_message(
                t!("This file has no project, so there is nothing to run."),
                cx,
            );
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

    /// Runs one step in the terminal panel and reports how it ended.
    fn run_step(
        &mut self,
        step: RunStep,
        on_complete: impl FnOnce(ScheduledTaskResult, &mut AsyncWindowContext) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace, _)) = self.workspace.clone() else {
            self.show_code_runner_message(
                t!("This file is not in a workspace, so there is nowhere to run it."),
                cx,
            );
            return;
        };

        // The command line is fully substituted, so the template only has to
        // carry it to the terminal. `cwd` decides where relative paths land.
        let cwd = step.cwd.clone();
        let task_context = TaskContext {
            cwd: Some(cwd.clone()),
            ..TaskContext::default()
        };
        let template = TaskTemplate {
            label: step.label,
            command: step.command_line,
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
            self.show_code_runner_message(
                t!("The run command could not be built from its template. Check code_runner.executor_map."),
                cx,
            );
            return;
        };

        // `update_in` looks the window up by entity id, and an action handler
        // already runs inside that window's update, so it would fail with
        // "entity has no current window". The action's own window goes through
        // instead.
        let scheduled = workspace.update(cx, |workspace, cx| {
            workspace.schedule_resolved_task_with_completion(
                TaskSourceKind::UserInput,
                resolved_task,
                true,
                on_complete,
                window,
                cx,
            );
        });
        if scheduled.is_err() {
            self.show_code_runner_message(
                t!("The workspace closed before the command could start."),
                cx,
            );
        }
    }

    fn show_code_runner_message(
        &self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        log::info!("code runner: {message}");
        let Some((workspace, _)) = self.workspace.clone() else {
            return;
        };
        workspace
            .update(cx, |workspace, cx| {
                let id = NotificationId::unique::<CodeRunNotification>();
                workspace.show_toast(Toast::new(id, message.to_string()), cx);
            })
            .ok();
    }
}

/// Marks the notification slot of the code runner, so its messages replace
/// each other instead of stacking up.
struct CodeRunNotification;

/// Resolves what to run for a target: the plan plus, when debugging, the
/// session that follows its build.
fn resolve_for(
    editor: &Editor,
    target: &CodeRunTarget,
    debug: bool,
    reused: Option<RunPlan>,
    cx: &App,
) -> Result<(RunPlan, Option<DebugPlan>), SharedString> {
    if let Some(plan) = reused {
        return Ok((plan, None));
    }
    let Some(language) = code_runner::language_id_for_path(&target.file) else {
        return Err(t!("There is no run command for this file type."));
    };
    let settings = editor.code_runner_settings(target, cx);
    let request = target.request(&language);
    if debug {
        code_runner::resolve_debug(&request, settings)
            .map(|plan| (plan.build.clone(), Some(plan)))
            .map_err(|error| run_error_message(&error))
    } else {
        code_runner::resolve(&request, settings)
            .map(|plan| (plan, None))
            .map_err(|error| run_error_message(&error))
    }
}

fn run_error_message(error: &RunError) -> SharedString {
    match error {
        RunError::NoCommand { language } => t!(
            "No run command for {language}. Add one under code_runner.executor_map in your settings.",
            language = language.clone()
        ),
        RunError::NoMain { directory } => t!(
            "No main function found in {directory}. Use a CMake project, or add a run command under code_runner.executor_map.",
            directory = directory.display().to_string()
        ),
        RunError::MissingTool { tool } => t!(
            "{tool} was not found in PATH. Install {tool} or add it to PATH.",
            tool = tool.clone()
        ),
        RunError::UnresolvedVariable => {
            t!("The run command uses a variable that has no value for this file.")
        }
        RunError::NoFile => t!("This editor has no file on disk to run."),
        RunError::NoProjectDirectory => {
            t!("No project directory to run in. Open a folder to use the code runner.")
        }
        RunError::EmptyCommand => t!("The run command is empty."),
    }
}

/// Finds the closest directory that holds a `CMakeLists.txt`, walking up from
/// the file to the worktree root, or further when the window holds one file.
fn cmake_root_for(file: &Path, worktree_root: &Path) -> Option<PathBuf> {
    // A window that opened a single file has that file as its worktree root,
    // so the walk cannot stop there.
    let stop_at = worktree_root.is_dir().then_some(worktree_root);
    let mut directory = file.parent();
    for _ in 0..24 {
        let current = directory?;
        if current.join("CMakeLists.txt").is_file() {
            return Some(current.to_path_buf());
        }
        if stop_at == Some(current) {
            return None;
        }
        directory = current.parent();
    }
    None
}

struct RunCommandModal {
    editor: WeakEntity<Editor>,
    plan: RunPlan,
    inputs: Vec<Entity<InputField>>,
}

impl RunCommandModal {
    fn new(
        editor: WeakEntity<Editor>,
        plan: RunPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inputs = plan
            .steps
            .iter()
            .map(|step| {
                let input = cx.new(|cx| InputField::new(window, cx, "Command"));
                input.update(cx, |input, cx| {
                    input.set_text(&step.command_line, window, cx);
                    input.editor().select_all(window, cx);
                });
                input
            })
            .collect();

        Self {
            editor,
            plan,
            inputs,
        }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let commands = self
            .inputs
            .iter()
            .map(|input| input.read(cx).text(cx).trim().to_owned())
            .collect::<Vec<_>>();
        if commands.iter().all(String::is_empty) {
            return;
        }

        let mut plan = self.plan.clone();
        for (step, command) in plan.steps.iter_mut().zip(commands) {
            step.command_line = command;
        }
        plan.source = CommandSource::Edited;

        let editor = self.editor.clone();
        cx.emit(DismissEvent);

        editor
            .update(cx, |editor, cx| {
                let Some(target) = editor.code_run_target(cx) else {
                    return;
                };
                editor.last_run = Some((target.file.clone(), plan.clone()));
                let worktree_id = target.worktree_id;
                let edited = plan.clone();
                editor.save_before_running(window, cx, move |editor, window, cx| {
                    editor.start_plan(edited.clone(), worktree_id, PlanGoal::Run, window, cx);
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
        // it the first command field's handle puts the caret there.
        match self.inputs.first() {
            Some(input) => input.read(cx).focus_handle(cx),
            None => cx.focus_handle(),
        }
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
        let origin: SharedString = match self.plan.source {
            CommandSource::BuiltIn => t!("built-in command"),
            CommandSource::Settings => "code_runner.executor_map".into(),
            CommandSource::Edited => t!("edited for this run"),
        };
        let directory = self.plan.root.display().to_string();
        let note = self.plan.note.clone();
        let language = self.plan.language.clone();

        AlertModal::new("run-current-file-modal")
            .width(rems(40.))
            .key_context("RunCurrentFileModal")
            .track_focus(&self.focus_handle(cx))
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
                                filename = self.plan.filename
                            )))
                            .child(
                                Label::new(format!("{language} · {origin}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .when_some(note, |this, note| {
                        this.child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
                    })
                    .children(
                        self.plan
                            .steps
                            .iter()
                            .zip(&self.inputs)
                            .map(|(step, input)| {
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(step.label.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(input.clone())
                            }),
                    )
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
