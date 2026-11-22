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
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, Pixels, ScrollHandle, Task, WeakEntity, Window, actions,
};
use i18n::t;
use path::{PathStyle, rel_path::RelPath};
use project::{Project, ProjectPath};
use ui::prelude::*;
use ui::{Checkbox, Tooltip};
use util::ResultExt;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};
use worktree::{EntryKind, WorktreeId};

use crate::conventions::{AGENTS_FILE_NAME, AGENTS_TEMPLATE};
use crate::snapshot;
use crate::symbol_map::{self, SourceFile, SymbolLine, SymbolMap};

actions!(
    repo_map_panel,
    [
        /// Toggle the repo map panel.
        ToggleFocus,
        /// Copy the whole symbol map to the clipboard as JSON.
        CopyToClipboard,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace: &mut Workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RepoMapPanel>(window, cx);
        });
        workspace.register_action(
            |workspace: &mut Workspace, _: &CopyToClipboard, _window, cx| {
                let Some(panel) = workspace.panel::<RepoMapPanel>(cx) else {
                    return;
                };
                let (json, symbols) = panel.read(cx).export_json();
                cx.write_to_clipboard(ClipboardItem::new_string(json));
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<RepoMapPanel>(),
                        t!("Copied {count} symbols as JSON", count = symbols).to_string(),
                    )
                    .autohide(),
                    cx,
                );
            },
        );
    })
    .detach();
}

pub struct RepoMapPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    structures: Vec<WorktreeStructure>,
    expanded: HashSet<Arc<str>>,
    /// Symbol maps per worktree, built on demand on a background thread.
    symbol_maps: HashMap<WorktreeId, SymbolIndex>,
    /// Worktrees whose symbol build is in flight.
    building: HashSet<WorktreeId>,
    /// Whether function-body variables are listed.
    include_locals: bool,
    active: bool,
    _refresh_task: Task<()>,
    _symbol_task: Task<()>,
}

/// A symbol map plus a path index, so rendering a file row does not scan the
/// whole map.
struct SymbolIndex {
    map: Arc<SymbolMap>,
    by_rel: HashMap<Arc<RelPath>, usize>,
    /// When this map was built. A reused map says so in the header instead of
    /// pretending to be fresh.
    built_at: chrono::DateTime<chrono::Utc>,
}

impl SymbolIndex {
    fn new(map: Arc<SymbolMap>, built_at: chrono::DateTime<chrono::Utc>) -> Self {
        let by_rel = map
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.rel.clone(), index))
            .collect();
        Self {
            map,
            by_rel,
            built_at,
        }
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
    /// The snapshot's mtime, so a symbol map can tell which files changed
    /// without reading every one of them.
    modified_ns: Option<i64>,
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
        in_degree: u32,
        out_degree: u32,
    },
    Symbol {
        depth: usize,
        kind: Arc<str>,
        name: Arc<str>,
        signature: Option<Arc<str>>,
        row: u32,
    },
    /// A C++ access label such as `public:`.
    Access {
        depth: usize,
        access: Arc<str>,
    },
    /// A run of variables summarized on one line.
    SymbolGroup {
        depth: usize,
        kind: Arc<str>,
        count: usize,
        first_row: u32,
        last_row: u32,
    },
}

impl RepoMapPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        Task::ready(workspace.update_in(&mut cx, |workspace, _window, cx| {
            let project = workspace.project().clone();
            let workspace_handle = cx.entity().downgrade();
            cx.new(|cx: &mut Context<Self>| Self {
                workspace: workspace_handle,
                project: project.clone(),
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
                structures: build_structures(&project, cx),
                expanded: HashSet::default(),
                symbol_maps: HashMap::default(),
                building: HashSet::default(),
                include_locals: false,
                active: false,
                _refresh_task: Task::ready(()),
                _symbol_task: Task::ready(()),
            })
        }))
    }

    /// Rebuilds the structure list from the current worktree snapshots.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.structures = build_structures(&project, cx);
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
                            Toast::new(NotificationId::unique::<RepoMapPanel>(), message)
                                .autohide(),
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

    /// The symbol maps of every worktree as JSON, with the number of symbols
    /// they contain: `[{"path": ..., "symbols": [{"name", "line", "kind"}]}]`.
    fn export_json(&self) -> (String, usize) {
        let mut files = Vec::new();
        let mut symbols = 0;
        for structure in &self.structures {
            let Some(index) = self.symbol_maps.get(&structure.worktree_id) else {
                continue;
            };
            symbols += index
                .map
                .ranked()
                .map(|file| file.symbols.len())
                .sum::<usize>();
            files.extend(index.map.json_entries());
        }
        let json = serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_owned());
        (json, symbols)
    }

    /// Starts background symbol builds for worktrees that are not being built.
    ///
    /// A worktree whose map is already shown is rebuilt too: the build reuses
    /// the map when every file still has the stamp it had, so an activation
    /// costs a stat per file instead of a parse per file, and an edit made
    /// while the panel was closed shows up without a manual refresh.
    fn request_symbol_maps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let fs = project.read(cx).fs().clone();
        let languages = project.read(cx).languages().clone();
        let cache_dir = paths::repo_map_cache_dir();

        let mut jobs: Vec<(WorktreeId, PathBuf, Vec<SourceFile>)> = Vec::new();
        for structure in &self.structures {
            if self.building.contains(&structure.worktree_id) {
                continue;
            }
            let mut sources = Vec::new();
            collect_sources(&structure.root, &structure.root_path, &mut sources);
            if !sources.is_empty() {
                jobs.push((structure.worktree_id, structure.root_path.clone(), sources));
            }
        }
        if jobs.is_empty() {
            return;
        }
        for (worktree_id, _, _) in &jobs {
            self.building.insert(*worktree_id);
        }
        cx.notify();

        self._symbol_task = cx.spawn_in(window, async move |this, cx| {
            for (worktree_id, root_path, sources) in jobs {
                let outcome = symbol_map::build_cached(
                    sources,
                    fs.clone(),
                    languages.clone(),
                    std::slice::from_ref(&root_path),
                    &cache_dir,
                )
                .await;
                let symbol_map::BuildOutcome {
                    map,
                    built_at,
                    from_memory,
                    ..
                } = outcome;
                // Pi's MCP server reads this file with no IPC channel, so it has
                // to be written wherever the map itself was produced. A map that
                // came from memory is already on disk, under the time it was
                // actually built, which is what the document says.
                if !from_memory
                    && let Err(error) =
                        snapshot::write(&map, &root_path, &paths::repo_map_snapshots_dir())
                {
                    log::error!("failed to write the repo map snapshot for {root_path:?}: {error}");
                }
                this.update_in(cx, |panel, _window, cx| {
                    panel.building.remove(&worktree_id);
                    panel
                        .symbol_maps
                        .insert(worktree_id, SymbolIndex::new(map, built_at));
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
                self.include_locals,
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
                let mapped = self
                    .symbol_maps
                    .get(&structure.worktree_id)
                    .map(|index| (index.map.ranked().count(), index.built_at));
                h_flex()
                    .id(("worktree-header", ix))
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(
                        v_flex()
                            .child(Label::new(structure.root_name.clone()).size(LabelSize::Small))
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
                            )
                            // A map that was reused says how old it is, so nobody
                            // reads a stale map as a fresh one.
                            .when_some(mapped, |this, (files, built_at)| {
                                this.child(
                                    Label::new(t!(
                                        "Map: {files} files, built {age}",
                                        files = files,
                                        age = format_age(built_at)
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        Checkbox::new(("include-locals", ix), self.include_locals.into())
                            .label(t!("Locals"))
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(t!("Show local variables")))
                            .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                                this.include_locals = *state == ToggleState::Selected;
                                cx.notify();
                            })),
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
                                // A refresh is a request to look at the files as
                                // they are now, not to parse all of them again:
                                // dropping the remembered maps makes the rebuild
                                // re-read every file's stat and keep the parses
                                // that still match.
                                for structure in &this.structures {
                                    symbol_map::forget_cached_map(std::slice::from_ref(
                                        &structure.root_path,
                                    ));
                                }
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
                        move |this, _: &ClickEvent, window, cx| match rel.clone() {
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
                in_degree,
                out_degree,
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
                            Label::new(t!("{count} symbols", count = *symbol_count))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        )
                    })
                    .when(has_symbols, |this| {
                        this.child(
                            Label::new(t!(
                                "in {inbound} / out {outbound}",
                                inbound = *in_degree,
                                outbound = *out_degree
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
                signature,
                row,
            } => h_flex()
                .id(("symbol", ix))
                .px_2()
                .py_0p5()
                .pl(px(34. + *depth as f32 * 14.))
                .gap_1()
                .child(
                    Label::new(match signature {
                        Some(signature) => format!("{name}{signature}"),
                        None => name.to_string(),
                    })
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
            Row::Access { depth, access } => h_flex()
                .id(("access", ix))
                .px_2()
                .py_0p5()
                .pl(px(34. + *depth as f32 * 14.))
                .gap_1()
                .child(
                    Label::new(format!("{access}:"))
                        .size(LabelSize::Small)
                        .color(Color::Placeholder),
                )
                .into_any_element(),
            Row::SymbolGroup {
                depth,
                kind,
                count,
                first_row,
                last_row,
            } => h_flex()
                .id(("symbol-group", ix))
                .px_2()
                .py_0p5()
                .pl(px(34. + *depth as f32 * 14.))
                .gap_1()
                .child(
                    Label::new(t!(
                        "{kind} x{count} (lines {first}–{last})",
                        kind = kind.to_string(),
                        count = *count,
                        first = first_row + 1,
                        last = last_row + 1
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
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
            modified_ns: file.modified_ns,
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
    include_locals: bool,
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
            push_dir_rows(
                expanded,
                worktree_id,
                symbols,
                dir,
                depth + 1,
                include_locals,
                rows,
            );
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
            in_degree: file_symbols.map_or(0, |file| file.in_degree),
            out_degree: file_symbols.map_or(0, |file| file.out_degree),
        });
        if is_expanded && let Some(file_symbols) = file_symbols {
            push_symbol_rows(&file_symbols.symbols, depth + 2, include_locals, rows);
        }
    }
}

/// The symbol rows of one file: the same grouping, folding and filtering the
/// agent sees in `repo_map`.
fn push_symbol_rows(
    symbols: &[crate::symbol_map::SymbolEntry],
    depth: usize,
    include_locals: bool,
    rows: &mut Vec<Row>,
) {
    for line in symbol_map::symbol_lines(symbols, include_locals) {
        match line {
            SymbolLine::Access {
                depth: line_depth,
                access,
            } => rows.push(Row::Access {
                depth: depth + line_depth as usize,
                access,
            }),
            SymbolLine::Symbol(symbol) => rows.push(Row::Symbol {
                depth: depth + symbol.depth as usize,
                kind: symbol.kind,
                name: symbol.name,
                signature: symbol.signature,
                row: symbol.row,
            }),
            SymbolLine::Folded {
                depth: line_depth,
                kind,
                count,
                first_row,
                last_row,
            } => rows.push(Row::SymbolGroup {
                depth: depth + line_depth as usize,
                kind,
                count,
                first_row,
                last_row,
            }),
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
        5
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
                        modified_ns: symbol_map::modified_nanos(entry.mtime),
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
    node.files
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
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

/// How long ago a map was built, in the coarse units a reader needs to decide
/// whether the map is worth refreshing.
fn format_age(built_at: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now()
        .signed_duration_since(built_at)
        .num_seconds()
        .max(0);
    if seconds < 60 {
        t!("just now").to_string()
    } else if seconds < 3_600 {
        t!("{minutes} min ago", minutes = seconds / 60).to_string()
    } else if seconds < 86_400 {
        t!("{hours} h ago", hours = seconds / 3_600).to_string()
    } else {
        t!("{days} d ago", days = seconds / 86_400).to_string()
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
            modified_ns: None,
        });
        chain(&mut root, &["small"]).files.push(FileRow {
            name: "b.rs".into(),
            rel: test_rel("small/b.rs"),
            size: 1,
            modified_ns: None,
        });
        chain(&mut root, &[]).files.push(FileRow {
            name: "root.rs".into(),
            rel: test_rel("root.rs"),
            size: 10,
            modified_ns: None,
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

    #[gpui::test]
    async fn loading_the_panel_during_a_workspace_update_does_not_panic(
        cx: &mut gpui::TestAppContext,
    ) {
        use project::FakeFs;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            workspace::AppState::test(cx);
            super::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({
                "src": {
                    "main.rs": "fn main() {}\n",
                },
            }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("test window should have a workspace");
        let workspace_handle = workspace.downgrade();

        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_, cx| {
                    RepoMapPanel::load(workspace_handle, cx.clone()).await
                })
            })
            .expect("test window should be open")
            .await
            .expect("repo map panel should load");
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| panel.read(cx).structures.len()),
            1,
            "the panel should be built from the workspace's worktrees"
        );
    }

    #[gpui::test]
    async fn opening_the_panel_during_a_workspace_update_does_not_panic(
        cx: &mut gpui::TestAppContext,
    ) {
        use project::FakeFs;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            workspace::AppState::test(cx);
            super::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({ "src": { "main.rs": "fn main() {}\n" } }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("test window should have a workspace");
        let workspace_handle = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_, cx| {
                    RepoMapPanel::load(workspace_handle, cx.clone()).await
                })
            })
            .expect("test window should be open")
            .await
            .expect("repo map panel should load");

        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx);
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.open_panel::<RepoMapPanel>(window, cx);
        });
        cx.run_until_parked();

        assert!(
            workspace.read_with(&cx, |workspace, cx| {
                workspace
                    .panel::<RepoMapPanel>(cx)
                    .is_some_and(|panel| panel.read(cx).active)
            }),
            "opening the panel should activate it without panicking"
        );
    }

    fn test_rel(path: &'static str) -> Arc<RelPath> {
        RelPath::new(Path::new(path), PathStyle::local())
            .expect("valid test path")
            .into_arc()
    }

    #[test]
    fn symbol_rows_carry_access_folding_and_signatures() {
        let entry = |name: &str, kind: &str, row: u32, access: Option<&str>| {
            crate::symbol_map::SymbolEntry {
                name: name.into(),
                kind: kind.into(),
                row,
                owner: Some("Widget".into()),
                depth: 2,
                access: access.map(Into::into),
                signature: Some("(int x) -> int".into()),
                local: false,
            }
        };
        let mut symbols = vec![
            entry("Widget", "class", 0, None),
            entry("draw", "method", 3, Some("public")),
            entry("colour", "field", 5, Some("private")),
            entry("width", "field", 6, Some("private")),
            entry("height", "field", 7, Some("private")),
        ];
        symbols[0].depth = 0;
        symbols[0].owner = None;

        let mut rows = Vec::new();
        push_symbol_rows(&symbols, 1, false, &mut rows);
        let access: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Access { access, .. } => Some(&**access),
                _ => None,
            })
            .collect();
        assert_eq!(access, ["public", "private"]);
        // Fields stay named instead of folding; only variables summarize.
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::Symbol { .. }))
                .count(),
            5
        );
        assert!(
            rows.iter().any(|row| matches!(
                row,
                Row::Symbol { signature: Some(signature), .. } if &**signature == "(int x) -> int"
            )),
            "signatures reach the panel"
        );
    }
}
