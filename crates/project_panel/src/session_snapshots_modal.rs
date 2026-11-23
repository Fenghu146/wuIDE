use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fs::Fs;
use gpui::{
    App, Context, DismissEvent, ElementId, EventEmitter, FocusHandle, Focusable, PromptButton,
    PromptLevel, SharedString, Task, WeakEntity, Window, prelude::*,
};
use git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff};
use i18n::t;
use session_snapshots::{
    SKIP_BINARY, SKIP_DELETED, SKIP_NOT_A_FILE, SKIP_TOO_LARGE, SessionSnapshotDb,
    SnapshotFileSummary, SnapshotKind, SnapshotSummary, read_file_text, restore_file,
    restore_snapshot,
};
use time::{OffsetDateTime, UtcOffset};
use time_format::TimestampFormat;
use ui::{
    Button, ButtonStyle, Color, ElevationIndex, Headline, HeadlineSize, Icon, IconButton, IconName,
    IconSize, Label, LabelSize, Modal, ModalFooter, ModalHeader, Section, SectionHeader,
    Tooltip, prelude::*,
};
use util::size::format_file_size;
use workspace::{ModalView, Workspace};

const SNAPSHOT_PAGE_SIZE: i64 = 30;

enum LoadState {
    Loading,
    Ready,
    Failed(SharedString),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiskState {
    /// The file on disk has the snapshot's size and mtime.
    Same,
    Differs,
    Missing,
}

struct FileRow {
    file: SnapshotFileSummary,
    disk: DiskState,
}

/// Lists the snapshots taken around agent sessions in one worktree, shows what
/// changed since, and puts files back.
pub struct SessionSnapshotsModal {
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    snapshots: Vec<SnapshotSummary>,
    all_snapshots_loaded: bool,
    selected_snapshot: Option<i64>,
    snapshot_state: LoadState,
    files: Vec<FileRow>,
    file_state: LoadState,
    message: Option<(bool, SharedString)>,
    load_task: Option<Task<()>>,
    files_task: Option<Task<()>>,
    restore_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
}

impl EventEmitter<DismissEvent> for SessionSnapshotsModal {}
impl ModalView for SessionSnapshotsModal {}

impl Focusable for SessionSnapshotsModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SessionSnapshotsModal {
    /// The action handler shared by the project panel's menu and the command palette.
    pub fn open(
        workspace: &mut Workspace,
        action: &session_snapshots::OpenSessionSnapshots,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let root = action.root.clone().map(PathBuf::from).or_else(|| {
            project
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        });
        let Some(root) = root else {
            return;
        };
        // Sessions record the worktree root, so a session started in a
        // subdirectory still lists the snapshots taken from it.
        let root = session_snapshots::worktree_root_for(&root);
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |_, cx| {
            Self::new(root, workspace_handle, cx)
        });
    }

    fn new(root: PathBuf, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            root,
            workspace,
            focus_handle: cx.focus_handle(),
            snapshots: Vec::new(),
            all_snapshots_loaded: false,
            selected_snapshot: None,
            snapshot_state: LoadState::Loading,
            files: Vec::new(),
            file_state: LoadState::Ready,
            message: None,
            load_task: None,
            files_task: None,
            restore_task: None,
            diff_task: None,
        };
        this.load_snapshots(None, cx);
        this
    }

    fn load_snapshots(&mut self, before: Option<i64>, cx: &mut Context<Self>) {
        let db = SessionSnapshotDb::global(cx);
        let root = self.root.to_string_lossy().into_owned();
        self.snapshot_state = LoadState::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    match before {
                        Some(before) => db.list_snapshots_before(root, before, SNAPSHOT_PAGE_SIZE),
                        None => db.list_snapshots(root, SNAPSHOT_PAGE_SIZE),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(snapshots) => {
                        this.all_snapshots_loaded = (snapshots.len() as i64) < SNAPSHOT_PAGE_SIZE;
                        let first = snapshots.first().map(|snapshot| snapshot.id);
                        this.snapshots.extend(snapshots);
                        this.snapshot_state = LoadState::Ready;
                        if this.selected_snapshot.is_none()
                            && let Some(first) = first
                        {
                            this.select_snapshot(first, cx);
                        }
                    }
                    Err(error) => {
                        this.snapshot_state = LoadState::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_snapshot(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected_snapshot = Some(id);
        self.files.clear();
        self.file_state = LoadState::Loading;
        let db = SessionSnapshotDb::global(cx);
        let root = self.root.clone();
        self.files_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let files = db.list_files(id)?;
                    anyhow::Ok(
                        files
                            .into_iter()
                            .map(|file| FileRow {
                                disk: disk_state(&root, &file),
                                file,
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(files) => {
                        this.files = files;
                        this.file_state = LoadState::Ready;
                    }
                    Err(error) => {
                        this.file_state = LoadState::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn restore_one(
        &mut self,
        file: SnapshotFileSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot_id) = self.selected_snapshot else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!(
                "Put back {path} as it was when this snapshot was taken?",
                path = &file.path
            ),
            None,
            &[
                PromptButton::new(t!("Restore")),
                PromptButton::cancel(t!("Cancel")),
            ],
            cx,
        );
        let db = SessionSnapshotDb::global(cx);
        let fs = <dyn Fs>::global(cx);
        let root = self.root.clone();
        let this = cx.entity();
        self.restore_task = Some(cx.spawn_in(window, async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let path = file.path.clone();
            let restored =
                cx.background_spawn(async move { restore_file(&db, fs, &root, snapshot_id, &file).await })
                    .await;
            this.update(cx, |this, cx| {
                this.message = Some(match restored {
                    Ok(true) => (false, t!("Put back {path}", path = path)),
                    Ok(false) => (true, t!("This snapshot has no copy of {path}", path = path)),
                    Err(error) => (true, format!("{error:#}").into()),
                });
                if let Some(snapshot_id) = this.selected_snapshot {
                    this.select_snapshot(snapshot_id, cx);
                }
                cx.notify();
            });
        }));
    }

    fn restore_everything(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self
            .selected_snapshot
            .and_then(|id| self.snapshots.iter().find(|snapshot| snapshot.id == id))
            .cloned()
        else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("Put every file of this snapshot back, replacing what is on disk now?"),
            None,
            &[
                PromptButton::new(t!("Restore All")),
                PromptButton::cancel(t!("Cancel")),
            ],
            cx,
        );
        let db = SessionSnapshotDb::global(cx);
        let fs = <dyn Fs>::global(cx);
        let this = cx.entity();
        self.restore_task = Some(cx.spawn_in(window, async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let restored = cx
                .background_spawn(async move { restore_snapshot(&db, fs, &snapshot).await })
                .await;
            this.update(cx, |this, cx| {
                this.message = Some(match restored {
                    Ok(report) if report.failed.is_empty() => (
                        false,
                        t!("Put back {count} files", count = report.restored),
                    ),
                    Ok(report) => {
                        let errors = report
                            .failed
                            .iter()
                            .map(|(path, error)| format!("{path}: {error}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        (
                            true,
                            t!(
                                "Put back {count} files, {failed} failed:\n{errors}",
                                count = report.restored,
                                failed = report.failed.len(),
                                errors = errors
                            ),
                        )
                    }
                    Err(error) => (true, format!("{error:#}").into()),
                });
                if let Some(snapshot_id) = this.selected_snapshot {
                    this.select_snapshot(snapshot_id, cx);
                }
                cx.notify();
            });
        }));
    }

    fn open_diff(
        &mut self,
        file: SnapshotFileSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot_id) = self.selected_snapshot else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let db = SessionSnapshotDb::global(cx);
        let absolute_path = self.root.join(disk_path(&file.path));
        let this = cx.entity();
        self.diff_task = Some(cx.spawn_in(window, async move |_, cx| {
            let (base, current) = cx
                .background_spawn({
                    let snapshot_path = file.path.clone();
                    async move {
                        let base = read_file_text(&db, snapshot_id, &snapshot_path)
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                        let current = std::fs::read(&absolute_path)
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                            .unwrap_or_default();
                        (base, current)
                    }
                })
                .await;
            let buffers = cx.update(|_, cx| {
                let project = workspace.read(cx).project().clone();
                let new_buffer = project.update(cx, |project, cx| {
                    project.create_local_buffer(&current, None, false, cx)
                });
                let language = new_buffer.read(cx).language().cloned();
                let old_buffer = project.update(cx, |project, cx| {
                    project.create_local_buffer(&base, language, false, cx)
                });
                (old_buffer, new_buffer, project)
            });
            let (old_buffer, new_buffer, project) = match buffers {
                Ok(buffers) => buffers,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.message = Some((true, format!("{error:#}").into()));
                        cx.notify();
                    });
                    return;
                }
            };
            let diff = build_buffer_diff(&old_buffer, &new_buffer, cx).await;
            let diff = match diff {
                Ok(diff) => diff,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.message = Some((true, format!("{error:#}").into()));
                        cx.notify();
                    });
                    return;
                }
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    let diff_view = cx.new(|cx| {
                        FileDiffView::new(
                            old_buffer,
                            new_buffer,
                            diff,
                            project,
                            workspace_entity,
                            window,
                            cx,
                        )
                    });
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(diff_view), true, true, None, window, cx);
                    });
                })
                .ok();
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn close(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_snapshots(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().id("snapshot-list").gap_0p5();
        match &self.snapshot_state {
            LoadState::Loading if self.snapshots.is_empty() => {
                list = list.child(Label::new(t!("Loading…")).color(Color::Muted));
            }
            LoadState::Failed(error) => {
                list = list.child(Label::new(error.clone()).color(Color::Error));
            }
            LoadState::Loading | LoadState::Ready if self.snapshots.is_empty() => {
                list = list.child(
                    Label::new(t!(
                        "No snapshots yet. They are taken when an agent session starts and ends."
                    ))
                    .color(Color::Muted),
                );
            }
            LoadState::Loading | LoadState::Ready => {}
        }
        for snapshot in &self.snapshots {
            let id = snapshot.id;
            let title = if snapshot.session_title.is_empty() {
                t!("Untitled chat")
            } else {
                SharedString::from(snapshot.session_title.clone())
            };
            let kind = match snapshot.kind {
                SnapshotKind::BeforeSession => t!("before the session"),
                SnapshotKind::AfterSession => t!("after the session"),
            };
            let mut details = vec![
                format_time(snapshot.created_at).to_string(),
                kind.to_string(),
                file_count_text(snapshot.file_count).to_string(),
                format_file_size(snapshot.total_bytes.max(0) as u64, true),
            ];
            if snapshot.skipped_count > 0 {
                details.push(t!("{count} skipped", count = snapshot.skipped_count).to_string());
            }
            let selected = self.selected_snapshot == Some(id);
            list = list.child(
                div()
                    .id(ElementId::named_usize("snapshot", id as usize))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(cx.theme().colors().element_selected))
                    .hover(|row| row.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.select_snapshot(id, cx)))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(Label::new(title))
                            .child(
                                Label::new(details.join(" · "))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            );
        }
        if !self.all_snapshots_loaded
            && let Some(oldest) = self.snapshots.last().map(|snapshot| snapshot.id)
        {
            list = list.child(
                Button::new("load-more-snapshots", t!("Load Older Snapshots"))
                    .style(ButtonStyle::Subtle)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.load_snapshots(Some(oldest), cx)),
                    ),
            );
        }
        list
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().id("snapshot-file-list").gap_0p5();
        if self.selected_snapshot.is_none() {
            return list
                .child(Label::new(t!("Pick a snapshot to see its files.")).color(Color::Muted));
        }
        match &self.file_state {
            LoadState::Loading => {
                list = list.child(Label::new(t!("Loading…")).color(Color::Muted));
            }
            LoadState::Failed(error) => {
                list = list.child(Label::new(error.clone()).color(Color::Error));
            }
            LoadState::Ready if self.files.is_empty() => {
                list = list.child(Label::new(t!("No files changed.")).color(Color::Muted));
            }
            LoadState::Ready => {}
        }
        for (index, row) in self.files.iter().enumerate() {
            let file = row.file.clone();
            let stored = file.is_stored();
            let restore_target = file.clone();
            let diff_target = file.clone();
            let mut details = vec![
                format_file_size(file.size.max(0) as u64, true),
                git_status_label(&file.git_status).to_string(),
                disk_state_label(row.disk).to_string(),
            ];
            if let Some(reason) = &file.skip_reason {
                details.push(skip_reason_label(reason).to_string());
            }
            list = list.child(
                div()
                    .id(ElementId::named_usize("snapshot-file", index))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .hover(|row| row.bg(cx.theme().colors().element_hover))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Label::new(SharedString::from(file.path.clone())))
                                    .child(
                                        Label::new(details.join(" · "))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                IconButton::new(
                                    ElementId::named_usize("snapshot-file-diff", index),
                                    IconName::FileDiff,
                                )
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(t!("Compare with the File on Disk")))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_diff(diff_target.clone(), window, cx)
                                })),
                            )
                            .when(stored, |row| {
                                row.child(
                                    IconButton::new(
                                        ElementId::named_usize("snapshot-file-restore", index),
                                        IconName::RotateCcw,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(t!("Put This File Back")))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.restore_one(restore_target.clone(), window, cx)
                                    })),
                                )
                            }),
                    ),
            );
        }
        list
    }
}

impl Render for SessionSnapshotsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_restore_all = self.files.iter().any(|row| row.file.is_stored());
        let mut footer = h_flex().gap_2();
        if let Some((is_error, message)) = &self.message {
            let color = if *is_error { Color::Error } else { Color::Success };
            footer = footer.child(Label::new(message.clone()).color(color).size(LabelSize::Small));
        }
        footer = footer
            .child(
                Button::new("restore-all-snapshots", t!("Restore All Files"))
                    .style(ButtonStyle::Subtle)
                    .disabled(!can_restore_all)
                    .on_click(cx.listener(|this, _, window, cx| this.restore_everything(window, cx))),
            )
            .child(
                Button::new("close-session-snapshots", t!("Close"))
                    .style(ButtonStyle::Filled)
                    .layer(ElevationIndex::ModalSurface)
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            );

        div()
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .occlude()
            .w(rems(52.))
            .child(
                Modal::new("session-snapshots", None)
                    .header(
                        ModalHeader::new()
                            .show_dismiss_button(true)
                            .icon(Icon::new(IconName::HistoryRerun).color(Color::Muted))
                            .child(Headline::new(t!("Session Snapshots")).size(HeadlineSize::Small)),
                    )
                    .section(
                        Section::new()
                            .header(SectionHeader::new(t!("Snapshots")))
                            .meta(self.root.to_string_lossy().into_owned())
                            .child(
                                v_flex()
                                    .id("snapshot-list-scroll")
                                    .max_h(rems(14.))
                                    .overflow_y_scroll()
                                    .child(self.render_snapshots(cx)),
                            ),
                    )
                    .section(
                        Section::new()
                            .header(SectionHeader::new(t!("Files")))
                            .child(
                                v_flex()
                                    .id("snapshot-file-list-scroll")
                                    .max_h(rems(20.))
                                    .overflow_y_scroll()
                                    .child(self.render_files(cx)),
                            ),
                    )
                    .footer(ModalFooter::new().end_slot(footer)),
            )
    }
}

fn disk_path(path: &str) -> PathBuf {
    PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn disk_state(root: &Path, file: &SnapshotFileSummary) -> DiskState {
    let Ok(metadata) = std::fs::metadata(root.join(disk_path(&file.path))) else {
        return DiskState::Missing;
    };
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs() as i64);
    if metadata.len() as i64 == file.size && mtime == file.mtime {
        DiskState::Same
    } else {
        DiskState::Differs
    }
}

fn format_time(created_at: i64) -> SharedString {
    let Ok(created_at) = OffsetDateTime::from_unix_timestamp(created_at) else {
        return SharedString::default();
    };
    let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    time_format::format_localized_timestamp(
        created_at,
        OffsetDateTime::from(SystemTime::now()),
        timezone,
        TimestampFormat::EnhancedAbsolute,
    )
    .into()
}

fn file_count_text(count: i64) -> SharedString {
    if count == 1 {
        t!("1 file")
    } else {
        t!("{count} files", count = count)
    }
}

fn git_status_label(status: &str) -> SharedString {
    match status {
        "untracked" => t!("untracked"),
        "modified" => t!("modified"),
        "added" => t!("added"),
        "deleted" => t!("deleted"),
        "conflict" => t!("conflict"),
        _ => t!("changed"),
    }
}

fn disk_state_label(state: DiskState) -> SharedString {
    match state {
        DiskState::Same => t!("same as disk"),
        DiskState::Differs => t!("differs from disk"),
        DiskState::Missing => t!("missing from disk"),
    }
}

fn skip_reason_label(reason: &str) -> SharedString {
    match reason {
        SKIP_TOO_LARGE => t!("not copied: larger than 2 MB"),
        SKIP_BINARY => t!("not copied: binary"),
        SKIP_DELETED => t!("not copied: deleted"),
        SKIP_NOT_A_FILE => t!("not copied: not a file"),
        _ => t!("not copied"),
    }
}
