//! A persistent sidebar panel that makes configured and discovered remote
//! servers visible at a glance, with direct actions to connect and open their
//! projects. The existing remote picker remains available for ad-hoc folders.

use std::{path::PathBuf, sync::Arc};

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    Subscription, Task, WeakEntity, Window, actions,
};
use i18n::{n, t};
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use settings::Settings as _;
use util::ResultExt as _;
use ui::{
    Clickable as _, Icon, IconButton, IconName, Indicator, Label, LabelCommon as _,
    LabelSize, ListItem, ListSubHeader, Tooltip, prelude::*,
};
use workspace::{MultiWorkspace, OpenOptions, Panel, Workspace, dock::{DockPosition, PanelEvent}};

use crate::{
    RemoteSettings, connect_with_modal, open_remote_project,
    remote_connections::Connection,
    remote_servers::RemoteServerProjects,
    ssh_config::parse_ssh_config_hosts,
};

use paths::{global_ssh_config_file, user_ssh_config_file};

actions!(
    remote_panel,
    [
        /// Toggles focus on the Remote Explorer panel.
        ToggleFocus,
    ]
);

#[derive(Clone)]
struct RemoteProject {
    paths: Vec<String>,
}

#[derive(Clone)]
enum RemoteServer {
    Configured {
        connection: Connection,
        projects: Vec<RemoteProject>,
    },
    SshConfig {
        host: Arc<str>,
    },
}

impl RemoteServer {
    fn key(&self) -> Arc<str> {
        match self {
            Self::Configured { connection, .. } => match connection {
                Connection::Ssh(connection) => Arc::from(format!(
                    "ssh:{}@{}:{}",
                    connection.username.as_deref().unwrap_or_default(),
                    connection.host,
                    connection.port.unwrap_or(22)
                )),
                Connection::Wsl(connection) => Arc::from(format!("wsl:{}", connection.distro_name)),
            },
            Self::SshConfig { host } => Arc::from(format!("ssh-config:{host}")),
        }
    }

    fn title(&self) -> Arc<str> {
        match self {
            Self::Configured { connection, .. } => match connection {
                Connection::Ssh(connection) => Arc::from(
                    connection
                        .nickname
                        .clone()
                        .unwrap_or_else(|| connection.host.clone()),
                ),
                Connection::Wsl(connection) => Arc::from(connection.distro_name.clone()),
            },
            Self::SshConfig { host } => host.clone(),
        }
    }

    fn subtitle(&self) -> Arc<str> {
        match self {
            Self::Configured { connection, .. } => match connection {
                Connection::Ssh(connection) => {
                    let username = connection.username.as_deref().unwrap_or("ssh");
                    Arc::from(format!(
                        "{}@{}:{}",
                        username,
                        connection.host,
                        connection.port.unwrap_or(22)
                    ))
                }
                Connection::Wsl(connection) => Arc::from(format!("WSL: {}", connection.distro_name)),
            },
            Self::SshConfig { host } => Arc::from(format!("~/.ssh/config · {host}")),
        }
    }

    fn icon(&self) -> IconName {
        match self {
            Self::Configured {
                connection: Connection::Wsl(_),
                ..
            } => IconName::Terminal,
            _ => IconName::Server,
        }
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        match self {
            Self::Configured { connection, .. } => connection.clone().into(),
            Self::SshConfig { host } => RemoteConnectionOptions::Ssh(SshConnectionOptions {
                host: host.as_ref().to_string().into(),
                ..Default::default()
            }),
        }
    }
}

pub struct RemotePanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    servers: Vec<RemoteServer>,
    expanded: Vec<Arc<str>>,
    _settings_subscription: Subscription,
    _refresh_task: Task<()>,
}

impl RemotePanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        Task::ready(workspace.clone().update_in(&mut cx, |_, window, cx| {
            cx.new(|cx: &mut Context<Self>| {
                let settings_subscription = cx.observe_global_in::<settings::SettingsStore>(window, {
                    let weak = cx.entity().downgrade();
                    move |_, window, cx| {
                        weak.update(cx, |panel, cx| panel.refresh(window, cx)).log_err();
                    }
                });
                let mut panel = Self {
                    workspace,
                    focus_handle: cx.focus_handle(),
                    scroll_handle: ScrollHandle::new(),
                    servers: Vec::new(),
                    expanded: Vec::new(),
                    _settings_subscription: settings_subscription,
                    _refresh_task: Task::ready(()),
                };
                panel.refresh(window, cx);
                panel
            })
        }))
    }

    /// Rebuilds the server list from settings and the local SSH config files.
    fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let settings = RemoteSettings::get_global(cx);
        let servers: Vec<RemoteServer> = settings
            .ssh_connections()
            .map(|connection| RemoteServer::Configured {
                projects: connection
                    .projects
                    .iter()
                    .map(|project| RemoteProject {
                        paths: project.paths.clone(),
                    })
                    .collect(),
                connection: connection.into(),
            })
            .chain(settings.wsl_connections().map(|connection| RemoteServer::Configured {
                projects: connection
                    .projects
                    .iter()
                    .map(|project| RemoteProject {
                        paths: project.paths.clone(),
                    })
                    .collect(),
                connection: connection.into(),
            }))
            .collect();

        let read_ssh_config = settings.read_ssh_config;
        let mut servers_for_task = servers.clone();
        self._refresh_task = cx.spawn(async move |this, cx| {
            if !read_ssh_config {
                return;
            }
            let mut hosts = std::collections::BTreeSet::new();
            let user_config = user_ssh_config_file();
            if let Ok(content) = std::fs::read_to_string(&user_config) {
                hosts.extend(parse_ssh_config_hosts(&content));
            }
            if let Some(global_config) = global_ssh_config_file()
                && let Ok(content) = std::fs::read_to_string(&global_config)
            {
                hosts.extend(parse_ssh_config_hosts(&content));
            }
            let configured_hosts: Vec<String> = this
                .update(cx, |panel, _| {
                    panel
                        .servers
                        .iter()
                        .filter_map(|server| match server {
                            RemoteServer::Configured {
                                connection: Connection::Ssh(connection),
                                ..
                            } => Some(connection.host.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            hosts.retain(|host| !configured_hosts.iter().any(|configured| configured == host));

            this.update(cx, |panel, cx| {
                servers_for_task.extend(
                    hosts
                        .into_iter()
                        .map(|host| RemoteServer::SshConfig {
                            host: Arc::from(host),
                        }),
                );
                panel.servers = servers_for_task;
                cx.notify();
            })
            .ok();
        });

        // The synchronous part is shown immediately; SSH config hosts are
        // appended once the async read finishes.
        self.servers = servers;
        cx.notify();
    }

    fn connect(&mut self, server: RemoteServer, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let options = server.connection_options();
        let task = connect_with_modal(&workspace, options, window, cx);
        cx.spawn_in(window, async move |_, _| {
            match task.await {
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(error) => {
                    let _ = error;
                }
            }
        })
        .detach();
    }

    fn open_project(
        &mut self,
        server: RemoteServer,
        project: RemoteProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let app_state = workspace.read_with(cx, |workspace, _| workspace.app_state().clone());
        let server_key = server.key();
        let Some(path) = project.paths.first().cloned() else {
            return;
        };
        let mut options = server.connection_options();
        if let RemoteConnectionOptions::Ssh(ref mut ssh) = options {
            RemoteSettings::get_global(cx).fill_connection_options_from_settings(ssh);
        }
        let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
        cx.spawn_in(window, async move |_, cx| {
            let result = open_remote_project(
                options,
                vec![PathBuf::from(path)],
                app_state,
                OpenOptions {
                    requesting_window,
                    ..Default::default()
                },
                cx,
            )
            .await;
            if let Err(error) = result {
                log::error!("failed to open remote project {server_key}: {error:#}");
            }
        })
        .detach();
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let (fs, weak) = workspace.read_with(cx, |workspace, _| {
            (workspace.app_state().fs.clone(), workspace.weak_handle())
        });
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs, window, weak.clone(), cx)
            });
        });
    }

    fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let options = RemoteConnectionOptions::Ssh(SshConnectionOptions::default());
        let task = connect_with_modal(&workspace, options, window, cx);
        cx.spawn(async move |_, _| {
            task.await.log_err();
        })
        .detach();
    }

    fn toggle_expanded(&mut self, key: Arc<str>) {
        if let Some(index) = self.expanded.iter().position(|entry| entry == &key) {
            self.expanded.remove(index);
        } else {
            self.expanded.push(key);
        }
    }
}

impl Focusable for RemotePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RemotePanel {}

impl Render for RemotePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("remote-panel")
            .track_focus(&self.focus_handle)
            .key_context("RemotePanel")
            .size_full()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .pt_2()
                    .child(Label::new(t!("Remote Explorer")).size(LabelSize::Small))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                IconButton::new("remote-refresh", IconName::RotateCw)
                                    .tooltip(Tooltip::text(t!("Refresh")))
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.refresh(window, cx);
                                    })),
                            )
                            .child(
                                IconButton::new("remote-new", IconName::Plus)
                                    .tooltip(Tooltip::text(t!("New Remote Connection")))
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.new_connection(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("remote-list")
                    .track_scroll(&self.scroll_handle)
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .px_1()
                            .pb_3()
                            .child(
                                ListSubHeader::new(t!("Servers")).end_slot(
                                    Indicator::dot()
                                        .color(if self.servers.iter().any(|server| {
                                            remote::has_active_connection(&server.connection_options(), cx)
                                        }) {
                                            ui::Color::Success
                                        } else {
                                            ui::Color::Muted
                                        })
                                        .into_any_element(),
                                ),
                            )
                            .children({
                                let mut items = Vec::new();
                                for server in self.servers.iter().cloned() {
                                    items.push(self.render_server(server, cx).into_any_element());
                                }
                                items
                            })
                            .when(self.servers.is_empty(), |this| {
                                this.child(
                                    v_flex()
                                        .gap_1()
                                        .px_2()
                                        .py_4()
                                        .child(Label::new(t!("No remote servers configured.")))
                                        .child(
                                            Label::new(t!(
                                                "Add one in settings or connect to a new host to get started."
                                            ))
                                            .color(ui::Color::Muted),
                                        ),
                                )
                            })
                            .child(
                                ListItem::new("remote-open-picker")
                                    .inset(true)
                                    .start_slot(Icon::new(IconName::FolderOpen))
                                    .child(Label::new(t!("Browse Remote Projects…")))
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.open_picker(window, cx);
                                    })),
                            ),
                    ),
            )
    }
}

impl RemotePanel {
    fn render_server(&self, server: RemoteServer, cx: &mut Context<Self>) -> impl IntoElement {
        let key = server.key();
        let expanded = self.expanded.contains(&key);
        let active = remote::has_active_connection(&server.connection_options(), cx);
        let has_projects = matches!(&server, RemoteServer::Configured { projects, .. } if !projects.is_empty());
        let title = server.title();
        let subtitle = server.subtitle();
        let icon = server.icon();
        let server_for_action = server.clone();
        let key_for_toggle = key.clone();

        v_flex()
            .child(
                ListItem::new(format!("remote-server-{key}"))
                    .inset(true)
                    .start_slot(Icon::new(icon))
                    .child(
                        v_flex()
                                                        .child(Label::new(title))
                            .child(
                                Label::new(subtitle).size(LabelSize::Small).color(ui::Color::Muted),
                            ),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .when(active, |this| {
                                this.child(Indicator::dot().color(ui::Color::Success))
                            })
                            .when(has_projects, |this| {
                                this.child(
                                    IconButton::new(
                                        format!("remote-toggle-{key_for_toggle}"),
                                        if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        },
                                    )
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        panel.toggle_expanded(key_for_toggle.clone());
                                        cx.notify();
                                    })),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.connect(server_for_action.clone(), window, cx);
                    })),
            )
            .when(expanded, |this| {
                let projects = match &server {
                    RemoteServer::Configured { projects, .. } => projects.clone(),
                    RemoteServer::SshConfig { .. } => Vec::new(),
                };
                this.children(projects.into_iter().filter_map(|project| {
                    let paths = project.paths;
                    let path = paths.first().cloned()?;
                    let key = path.clone();
                    Some(
                        ListItem::new(format!("remote-project-{key}"))
                            .inset(true)
                            .start_slot(Icon::new(IconName::Folder))
                            .child(Label::new(path).color(ui::Color::Muted))
                            .on_click({
                                let server = server.clone();
                                cx.listener(move |panel, _, window, cx| {
                                    panel.open_project(
                                        server.clone(),
                                        RemoteProject { paths: paths.clone() },
                                        window,
                                        cx,
                                    );
                                })
                            }),
                    )
                }))
            })
    }
}

impl Panel for RemotePanel {
    fn persistent_name() -> &'static str {
        "Remote Panel"
    }

    fn panel_key() -> &'static str {
        "RemotePanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        gpui::px(280.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some(n!("Remote Explorer"))
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}
