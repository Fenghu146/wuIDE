//! A zero-cost structure overview of the open worktrees: modules by weight,
//! the largest files, and the state of the project conventions file
//! (AGENTS.md). Built entirely from the worktree snapshots that are already
//! in memory; no indexing, no background services.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Pixels, ScrollHandle, Task, WeakEntity, Window, actions,
};
use i18n::t;
use path::{PathStyle, rel_path::RelPath};
use project::{Project, ProjectPath};
use ui::prelude::*;
use util::ResultExt;
use worktree::{EntryKind, WorktreeId};
use workspace::{
    Workspace, Toast,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::conventions::{AGENTS_FILE_NAME, AGENTS_TEMPLATE};
use crate::symbol_map::{self, SourceFile, SymbolMap};

actions!(
    repo_map_panel,
    [
        /// Toggle the repo map panel.
        ToggleFocus,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<RepoMapPanel>(window, cx);
            },
        );
    })
    .detach();
}

pub struct RepoMapPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    structures: Vec<WorktreeStructure>,
    expanded: HashSet<Arc<str>>,
    /// Symbol maps per worktree, built on demand on a background thread.
    symbol_maps: HashMap<WorktreeId, SymbolIndex>,
    /// Worktrees whose symbol build is in flight.
    building: HashSet<WorktreeId>,
    active: bool,
    _refresh_task: Task<()>,
    _symbol_task: Task<()>,
}

/// A symbol map plus a path index, so rendering a file row does not scan the
/// whole map.
struct SymbolIndex {
    map: SymbolMap,
    by_rel: HashMap<Arc<RelPath>, usize>,
}

impl SymbolIndex {
    fn new(map: SymbolMap) -> Self {
        let by_rel = map
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.rel.clone(), index))
            .collect();
        Self { map, by_rel }
    }

    fn file(&self, rel: &Arc<RelPath>) -> Option<&crate::symbol_map::FileSymbols> {
        self.by_rel
            .get(rel)
            .and_then(|index| self.map.files.get(*index))
    }
}

struct WorktreeStructure {
    worktree_id: WorktreeId,
    root_name: Arc<str>,
    root_path: PathBuf,
    root: DirNode,
    total_files: usize,
    total_size: u64,
    agents_md: Option<Arc<RelPath>>,
}

struct DirNode {
    name: Arc<str>,
    /// Path relative to the worktree root, used as the expand/collapse key.
    path: Arc<str>,
    dirs: Vec<DirNode>,
    files: Vec<FileRow>,
    total_size: u64,
    file_count: usize,
}

impl DirNode {
    fn new(name: Arc<str>, path: Arc<str>) -> Self {
        Self {
            name,
            path,
            dirs: Vec::new(),
            files: Vec::new(),
            total_size: 0,
            file_count: 0,
        }
    }
}

#[derive(Clone)]
struct FileRow {
    name: Arc<str>,
    rel: Arc<RelPath>,
    size: u64,
}

#[derive(Clone)]
enum Row {
    WorktreeHeader {
        index: usize,
    },
    Conventions {
        worktree_id: WorktreeId,
        root_path: PathBuf,
        /// The AGENTS.md entry when the file exists; `None` offers creation.
        rel: Option<Arc<RelPath>>,
    },
    Dir {
        depth: usize,
        path: Arc<str>,
        name: Arc<str>,
        file_count: usize,
        size: u64,
        expanded: bool,
    },
    File {
        worktree_id: WorktreeId,
        rel: Arc<RelPath>,
        name: Arc<str>,
        size: u64,
        depth: usize,
        expanded: bool,
        symbol_count: usize,
        rank: Option<f32>,
    },
    Symbol {
        depth: usize,
        kind: Arc<str>,
        name: Arc<str>,
        row: u32,
    },
}

impl RepoMapPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        Task::ready(workspace.clone().update_in(&mut cx, |_, _window, cx| {
            cx.new(|cx: &mut Context<Self>| {
                let mut panel = Self {
                    workspace: workspace.clone(),
                    focus_handle: cx.focus_handle(),
                    scroll_handle: ScrollHandle::new(),
                    structures: Vec::new(),
                    expanded: HashSet::default(),
                    symbol_maps: HashMap::default(),
                    building: HashSet::default(),
                    active: false,
                    _refresh_task: Task::ready(()),
                    _symbol_task: Task::ready(()),
                };
                panel.refresh(cx);
                panel
            })
        }))
    }

    /// Rebuilds the structure list from the current worktree snapshots.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            let project = workspace.read(cx).project().clone();
            self.structures = build_structures(&project, cx);
        }
        cx.notify();
    }

    fn create_agents_md(
        &mut self,
        worktree_id: WorktreeId,
        root_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let fs = project.read(cx).fs().clone();
        let file_path = root_path.join(AGENTS_FILE_NAME);
        let content = AGENTS_TEMPLATE.to_string();
        self._refresh_task = cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = fs.write(file_path.as_path(), content.as_bytes()).await {
                let message = t!(
                    "Failed to create {path}: {error}",
                    path = file_path.display().to_string(),
                    error = error.to_string()
                )
                .to_string();
                log::error!("{message}");
                workspace
                    .update_in(cx, |workspace, _window, cx| {
                        workspace.show_toast(
                            Toast::new(NotificationId::unique::<RepoMapPanel>(), message).autohide(),
                            cx,
                        )
                    })
                    .log_err();
                return;
            }

            // Bring the new file into the snapshot before opening it.
            let rel = RelPath::new(Path::new(AGENTS_FILE_NAME), PathStyle::local())
                .ok()
                .map(|rel| rel.into_arc());
            let Some(rel) = rel else {
                log::error!("invalid AGENTS.md relative path");
                return;
            };
            let refresh = workspace
                .update_in(cx, |workspace, _window, cx| {
                    let project = workspace.project().clone();
                    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
                        return None;
                    };
                    worktree.update(cx, |worktree, _| {
                        worktree
                            .as_local()
                            .map(|local| local.refresh_entries_for_paths(vec![rel.clone()]))
                    })
                })
                .log_err()
                .flatten();
            if let Some(mut refresh) = refresh {
                use futures::StreamExt as _;
                refresh.next().await;
            }

            this.update_in(cx, |panel, _window, cx| {
                panel.refresh(cx);
            })
            .log_err();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace
                        .open_path(
                            ProjectPath {
                                worktree_id,
                                path: rel.clone(),
                            },
                            None,
                            true,
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                })
                .log_err();
        });
    }

    /// Starts background symbol builds for worktrees that do not have one yet.
    fn request_symbol_maps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let fs = project.read(cx).fs().clone();
        let languages = project.read(cx).languages().clone();

        let mut jobs: Vec<(WorktreeId, Vec<SourceFile>)> = Vec::new();
        for structure in &self.structures {
            if self.symbol_maps.contains_key(&structure.worktree_id)
                || self.building.contains(&structure.worktree_id)
            {
                continue;
            }
            let mut sources = Vec::new();
            collect_sources(&structure.root, &structure.root_path, &mut sources);
            if !sources.is_empty() {
                jobs.push((structure.worktree_id, sources));
            }
        }
        if jobs.is_empty() {
            return;
        }
        for (worktree_id, _) in &jobs {
            self.building.insert(*worktree_id);
        }
        cx.notify();

        self._symbol_task = cx.spawn_in(window, async move |this, cx| {
            for (worktree_id, sources) in jobs {
                let map = symbol_map::build(sources, fs.clone(), languages.clone()).await;
                this.update_in(cx, |panel, _window, cx| {
                    panel.building.remove(&worktree_id);
                    panel.symbol_maps
                        .insert(worktree_id, SymbolIndex::new(map));
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (index, structure) in self.structures.iter().enumerate() {
            rows.push(Row::WorktreeHeader { index });
            rows.push(Row::Conventions {
                worktree_id: structure.worktree_id,
                root_path: structure.root_path.clone(),
                rel: structure.agents_md.clone(),
            });
            push_dir_rows(
                &self.expanded,
                structure.worktree_id,
                self.symbol_maps.get(&structure.worktree_id),
                &structure.root,
                0,
                &mut rows,
            );
        }
        rows
    }

    fn render_row(&self, ix: usize, row: &Row, cx: &Context<Self>) -> AnyElement {
        match row {
            Row::WorktreeHeader { index } => {
                let Some(structure) = self.structures.get(*index) else {
                    return div().into_any_element();
                };
                let building = self.building.contains(&structure.worktree_id);
                h_flex()
                    .id(("worktree-header", ix))
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(
                        v_flex().child(
                            Label::new(structure.root_name.clone()).size(LabelSize::Small),
                        )
                        .child(
                            Label::new(if building {
                                t!("Scanning symbols…")
                            } else {
                                t!(
                                    "{count} files · {size}",
                                    count = structure.total_files,
                                    size = format_size(structure.total_size)
                                )
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                    )
                    .child(
                        IconButton::new(("refresh", ix), IconName::RotateCcw).on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.refresh(cx);
                                this.symbol_maps.clear();
                                // The new symbol task replaces (and cancels) the
                                // previous one, so its in-flight markers must go
                                // with it or those worktrees would never rebuild.
                                this.building.clear();
                                this.request_symbol_maps(window, cx);
                            }),
                        ),
                    )
                    .into_any_element()
            }
            Row::Conventions {
                worktree_id,
                root_path,
                rel,
            } => {
                let present = rel.is_some();
                h_flex()
                    .id(("conventions", ix))
                    .px_2()
                    .py_1()
                    .gap_1()
                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(if present {
                            IconName::Check
                        } else {
                            IconName::FileCode
                        })
                        .size(IconSize::Small),
                    )
                    .child(
                        Label::new(if present {
                            t!("Open AGENTS.md")
                        } else {
                            t!("Create AGENTS.md")
                        })
                        .size(LabelSize::Small)
                        .color(if present {
                            Color::Muted
                        } else {
                            Color::Placeholder
                        }),
                    )
                    .on_click(cx.listener({
                        let worktree_id = *worktree_id;
                        let root_path = root_path.clone();
                        let rel = rel.clone();
                        move |this, _: &ClickEvent, window, cx| {
                            match rel.clone() {
                                Some(rel) => {
                                    this.workspace
                                        .update(cx, |workspace, cx| {
                                            workspace
                                                .open_path(
                                                    ProjectPath {
                                                        worktree_id,
                                                        path: rel,
                                                    },
                                                    None,
                                                    true,
                                                    window,
                                                    cx,
                                                )
                                                .detach_and_log_err(cx);
                                        })
                                        .log_err();
                                }
                                None => {
                                    this.create_agents_md(worktree_id, root_path.clone(), window, cx);
                                }
                            }
                        }
                    }))
                    .into_any_element()
            }
            Row::Dir {
                depth,
                path,
                name,
                file_count,
                size,
                expanded,
            } => h_flex()
                .id(("dir", ix))
                .px_2()
                .py_0p5()
                .pl(px(8. + *depth as f32 * 14.))
                .gap_1()
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().colors().element_hover))
                .on_click(cx.listener({
                    let path = path.clone();
                    move |this, _: &ClickEvent, _window, cx| {
                        if this.expanded.remove(&path) {
                            // Was expanded; now collapsed.
                        } else {
                            this.expanded.insert(path.clone());
                        }
                        cx.notify();
                    }
                }))
                .child(
                    Icon::new(if *expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::Small),
                )
                .child(Label::new(name.clone()).size(LabelSize::Small))
                .child(div().flex_1())
                .child(
                    Label::new(t!(
                        "{count} files · {size}",
                        count = *file_count,
                        size = format_size(*size)
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            Row::File {
                worktree_id,
                rel,
                name,
                size,
                depth,
                expanded,
                symbol_count,
                rank,
            } => {
                let has_symbols = *symbol_count > 0;
                let key = rel.display(PathStyle::Unix).into_owned();
                h_flex()
                    .id(("file", ix))
                    .px_2()
                    .py_0p5()
                    .pl(px(22. + *depth as f32 * 14.))
                    .gap_1()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                    .when(has_symbols, |this| {
                        this.child(
                            Icon::new(if *expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::Small),
                        )
                    })
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when(has_symbols, |this| {
                        this.child(
                            Label::new(t!(
                                "{count} symbols",
                                count = *symbol_count
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Placeholder),
                        )
                    })
                    .child(
                        Label::new(format_size(*size))
                            .size(LabelSize::Small)
                            .color(Color::Placeholder),
                    )
                    .on_click(cx.listener({
                        let project_path = ProjectPath {
                            worktree_id: *worktree_id,
                            path: rel.clone(),
                        };
                        move |this, event: &ClickEvent, window, cx| {
                            // A plain click toggles the definition list once the
                            // file has one; opening the file stays on double
                            // click so browsing the map never steals the editor.
                            if has_symbols && event.click_count() == 1 {
                                if this.expanded.contains(key.as_str()) {
                                    this.expanded.remove(key.as_str());
                                } else {
                                    this.expanded.insert(Arc::from(key.as_str()));
                                }
                                cx.notify();
                                return;
                            }
                            this.workspace
                                .update(cx, |workspace, cx| {
                                    workspace
                                        .open_path(project_path.clone(), None, true, window, cx)
                                        .detach_and_log_err(cx);
                                })
                                .log_err();
                        }
                    }))
                    .when_some(*rank, |this, rank| {
                        this.child(
                            Label::new(format!("{:.1}%", rank * 100.))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        )
                    })
                    .into_any_element()
            }
            Row::Symbol {
                depth,
                kind,
                name,
                row,
            } => h_flex()
                .id(("symbol", ix))
                .px_2()
                .py_0p5()
                .pl(px(34. + *depth as f32 * 14.))
                .gap_1()
                .child(
                    Label::new(name.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(kind.clone())
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
                .child(div().flex_1())
                .child(
                    Label::new(format!(":{}", row + 1))
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
                .into_any_element(),
        }
    }
}

/// Flattens a directory tree into the file list the symbol builder consumes.
fn collect_sources(node: &DirNode, root_path: &Path, out: &mut Vec<SourceFile>) {
    for dir in &node.dirs {
        collect_sources(dir, root_path, out);
    }
    for file in &node.files {
        out.push(SourceFile {
            rel: file.rel.clone(),
            abs_path: root_path.join(file.rel.as_std_path()),
            size: file.size,
        });
    }
}

/// Walks the tree, emitting only the rows within expanded directories, plus
/// the definitions of any expanded file.
fn push_dir_rows(
    expanded: &HashSet<Arc<str>>,
    worktree_id: WorktreeId,
    symbols: Option<&SymbolIndex>,
    node: &DirNode,
    depth: usize,
    rows: &mut Vec<Row>,
) {
    for dir in &node.dirs {
        let is_expanded = expanded.contains(&dir.path);
        rows.push(Row::Dir {
            depth,
            path: dir.path.clone(),
            name: dir.name.clone(),
            file_count: dir.file_count,
            size: dir.total_size,
            expanded: is_expanded,
        });
        if is_expanded {
            push_dir_rows(expanded, worktree_id, symbols, dir, depth + 1, rows);
        }
    }
    for file in &node.files {
        // Forward slashes so the key matches on every platform.
        let key = file.rel.display(PathStyle::Unix).into_owned();
        let is_expanded = expanded.contains(key.as_str());
        let file_symbols = symbols.and_then(|index| index.file(&file.rel));
        rows.push(Row::File {
            worktree_id,
            rel: file.rel.clone(),
            name: file.name.clone(),
            size: file.size,
            depth: depth + 1,
            expanded: is_expanded,
            symbol_count: file_symbols.map_or(0, |file| file.symbols.len()),
            rank: file_symbols.map(|file| file.rank),
        });
        if is_expanded && let Some(file_symbols) = file_symbols {
            for symbol in &file_symbols.symbols {
                rows.push(Row::Symbol {
                    depth: depth + 2,
                    kind: symbol.kind.clone(),
                    name: symbol.name.clone(),
                    row: symbol.row,
                });
            }
        }
    }
}

impl Panel for RepoMapPanel {
    fn activation_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn persistent_name() -> &'static str {
        "Repo Map Panel"
    }

    fn panel_key() -> &'static str {
        "RepoMapPanel"
    }

    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _: &Window, _cx: &App) -> Option<&'static str> {
        Some("Repo Map")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active && !self.active {
            self.refresh(cx);
            self.request_symbol_maps(window, cx);
        }
        self.active = active;
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

impl Focusable for RepoMapPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RepoMapPanel {}

impl Render for RepoMapPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.visible_rows();
        let empty = rows.is_empty();
        v_flex()
            .id("repo-map-panel")
            .track_scroll(&self.scroll_handle)
            .h_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .when(empty, |this| {
                this.child(
                    Label::new(t!("No project is open"))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .mx_2()
                        .my_2(),
                )
            })
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(ix, row)| self.render_row(ix, &row, cx)),
            )
    }
}

fn build_structures(project: &Entity<Project>, cx: &App) -> Vec<WorktreeStructure> {
    let worktrees: Vec<_> = project.read(cx).visible_worktrees(cx).collect();
    let mut structures = Vec::with_capacity(worktrees.len());
    for worktree in worktrees {
        let snapshot = worktree.read(cx).snapshot();
        let root_name: Arc<str> = worktree.read(cx).root_name_str().into();
        let root_path = worktree.read(cx).abs_path().to_path_buf();
        let worktree_id = worktree.read(cx).id();
        let mut structure = WorktreeStructure {
            worktree_id,
            root_name: root_name.clone(),
            root_path,
            root: DirNode::new(root_name, Arc::from("")),
            total_files: 0,
            total_size: 0,
            agents_md: None,
        };
        for entry in snapshot.entries(false, 0) {
            let path = &entry.path;
            if path.is_empty() {
                continue;
            }
            let components: Vec<&str> = path.components().collect();
            let Some(name) = components.last().copied() else {
                continue;
            };
            match entry.kind {
                EntryKind::Dir => {
                    chain(&mut structure.root, &components);
                }
                EntryKind::File => {
                    let mut parent_segments = components.clone();
                    parent_segments.pop();
                    let parent = chain(&mut structure.root, &parent_segments);
                    parent.files.push(FileRow {
                        name: name.into(),
                        rel: path.clone(),
                        size: entry.size,
                    });
                    structure.total_files += 1;
                    structure.total_size += entry.size;
                    if components.len() == 1 && name == AGENTS_FILE_NAME {
                        structure.agents_md = Some(path.clone());
                    }
                }
                EntryKind::UnloadedDir | EntryKind::PendingDir => {}
            }
        }
        finalize(&mut structure.root);
        structures.push(structure);
    }
    structures
}

/// Creates (or finds) the directory chain for `segments` under `root`.
/// Entries arrive in path order, so a parent is always seen before its
/// children.
fn chain<'a>(root: &'a mut DirNode, segments: &[&str]) -> &'a mut DirNode {
    let mut node = root;
    let mut prefix = String::new();
    for segment in segments {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if let Some(index) = node.dirs.iter().position(|dir| &*dir.name == *segment) {
            node = &mut node.dirs[index];
        } else {
            node.dirs
                .push(DirNode::new((*segment).into(), Arc::from(prefix.as_str())));
            let last = node.dirs.len() - 1;
            node = &mut node.dirs[last];
        }
    }
    node
}

/// Computes aggregate sizes and sorts children by weight, heaviest first.
fn finalize(node: &mut DirNode) {
    let mut total_size = 0u64;
    let mut file_count = node.files.len();
    for file in &node.files {
        total_size += file.size;
    }
    for dir in &mut node.dirs {
        finalize(dir);
        total_size += dir.total_size;
        file_count += dir.file_count;
    }
    node.dirs.sort_by(|a, b| {
        b.total_size
            .cmp(&a.total_size)
            .then_with(|| a.name.cmp(&b.name))
    });
    node.files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    node.total_size = total_size;
    node.file_count = file_count;
}

fn format_size(size: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1024. && unit + 1 < units.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_creates_nested_directories() {
        let mut root = DirNode::new("root".into(), "".into());
        let segments = ["crates", "repo_wiki", "src"];
        let node = chain(&mut root, &segments);
        assert_eq!(&*node.name, "src");
        assert_eq!(&*node.path, "crates/repo_wiki/src");
        let node = chain(&mut root, &["crates", "repo_wiki"]);
        assert_eq!(&*node.name, "repo_wiki");
        // The existing directory is reused, not duplicated.
        assert_eq!(root.dirs.len(), 1);
        assert_eq!(root.dirs[0].dirs.len(), 1);
    }

    #[test]
    fn finalize_sorts_heaviest_first() {
        let mut root = DirNode::new("root".into(), "".into());
        chain(&mut root, &["big"]).files.push(FileRow {
            name: "a.rs".into(),
            rel: test_rel("big/a.rs"),
            size: 100,
        });
        chain(&mut root, &["small"]).files.push(FileRow {
            name: "b.rs".into(),
            rel: test_rel("small/b.rs"),
            size: 1,
        });
        chain(&mut root, &[]).files.push(FileRow {
            name: "root.rs".into(),
            rel: test_rel("root.rs"),
            size: 10,
        });
        finalize(&mut root);
        assert_eq!(&*root.dirs[0].name, "big");
        assert_eq!(root.dirs[0].total_size, 100);
        assert_eq!(root.total_size, 111);
        assert_eq!(root.file_count, 3);
    }

    #[test]
    fn format_size_uses_readable_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
    }

    fn test_rel(path: &'static str) -> Arc<RelPath> {
        RelPath::new(Path::new(path), PathStyle::local())
            .expect("valid test path")
            .into_arc()
    }
}
