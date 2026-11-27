//! A persistent sidebar panel that makes configured and discovered remote
//! servers visible at a glance, with direct actions to connect and open their
//! projects. The existing remote picker remains available for ad-hoc folders.

use std::{path::PathBuf, sync::Arc};

use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ScrollHandle, Subscription, Task, TaskExt, WeakEntity, Window, actions,
};
use i18n::{n, t};
use menu;
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use settings::{Settings as _, update_settings_file};
use ui::{
    Clickable as _, ElevationIndex, Icon, IconButton, IconName, Indicator, Label, LabelCommon as _,
    LabelSize, ListItem, ListSubHeader, Modal, ModalFooter, ModalHeader, Section, Tooltip,
    prelude::*,
};
use workspace::{
    ModalView, MultiWorkspace, OpenOptions, Panel, Workspace, dock::{DockPosition, PanelEvent},
};

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
                let settings_subscription = cx.observe_global_in::<settings::SettingsStore>(
                    window,
                    |panel, window, cx| panel.refresh(window, cx),
                );
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
        let weak_workspace = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                NewSshConnectionModal::new(weak_workspace.clone(), window, cx)
            });
        });
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
        10
    }
}

struct NewSshConnectionModal {
    workspace: WeakEntity<Workspace>,
    nickname: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    username: Entity<Editor>,
    note: Entity<Editor>,
    private_key_path: Entity<Editor>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl NewSshConnectionModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let nickname = cx.new(|cx| Editor::single_line(window, cx));
        let host = cx.new(|cx| Editor::single_line(window, cx));
        let port = cx.new(|cx| Editor::single_line(window, cx));
        let username = cx.new(|cx| Editor::single_line(window, cx));
        let note = cx.new(|cx| Editor::single_line(window, cx));
        let private_key_path = cx.new(|cx| Editor::single_line(window, cx));
        port.update(cx, |editor, cx| editor.set_text("22", window, cx));
        nickname.update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));

        let mut subscriptions = Vec::new();
        for editor in [&nickname, &host, &port, &username, &note, &private_key_path] {
            subscriptions.push(cx.observe(editor, |_, _, cx| cx.notify()));
        }

        Self {
            workspace,
            nickname,
            host,
            port,
            username,
            note,
            private_key_path,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.read(cx).text(cx).trim().to_string();
        if host.is_empty() {
            return;
        }
        let port = self
            .port
            .read(cx)
            .text(cx)
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .unwrap_or(22);
        let nickname = trimmed_option(self.nickname.read(cx).text(cx));
        let username = trimmed_option(self.username.read(cx).text(cx));
        let private_key_path = trimmed_option(self.private_key_path.read(cx).text(cx));
        let (connection, options) =
            ssh_connection_and_options(nickname, host, port, username, private_key_path);

        if let Some(fs) = self.workspace.upgrade().map(|workspace| {
            workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone())
        }) {
            update_settings_file(fs, cx, move |settings, _| {
                settings
                    .remote
                    .ssh_connections
                    .get_or_insert_with(Vec::new)
                    .push(connection);
            });
        }

        cx.emit(DismissEvent);
        if let Some(workspace) = self.workspace.upgrade() {
            // `confirm` runs inside this modal's listener, so the modal entity is
            // leased. Connecting synchronously would make `toggle_modal` hide the
            // same entity and panic with a double lease. Defer to the end of the
            // effect cycle, after the lease is released. Bind to the window handle
            // rather than an entity: both the modal and the workspace are leased
            // here, and `connect_with_modal` needs to lease the workspace itself.
            let window_handle = window.window_handle();
            cx.defer(move |cx| {
                cx.update_window(window_handle, |_, window, cx| {
                    let task = connect_with_modal(
                        &workspace,
                        RemoteConnectionOptions::Ssh(options),
                        window,
                        cx,
                    );
                    task.detach_and_log_err(cx);
                })
                .ok();
            });
        }
    }

    fn field(label: SharedString, editor: Entity<Editor>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small))
            .child(div().w_full().child(editor))
    }
}

impl Focusable for NewSshConnectionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for NewSshConnectionModal {}

impl ModalView for NewSshConnectionModal {}

impl Render for NewSshConnectionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host_missing = self.host.read(cx).text(cx).trim().is_empty();

        div()
            .elevation_3(cx)
            .w(rems(30.))
            .max_h(rems(36.))
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .child(
                Modal::new("new-ssh-connection", None)
                    .header(
                        ModalHeader::new()
                            .show_dismiss_button(true)
                            .headline(t!("New SSH Connection")),
                    )
                    .section(
                        Section::new()
                            .child(Self::field(t!("Name"), self.nickname.clone()))
                            .child(Self::field(t!("Host"), self.host.clone()))
                            .child(Self::field(t!("Port"), self.port.clone()))
                            .child(Self::field(t!("Username"), self.username.clone()))
                            .child(Self::field(
                                t!("Private Key Path"),
                                self.private_key_path.clone(),
                            ))
                            .child(Self::field(t!("Note"), self.note.clone())),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("new-ssh-cancel", t!("Cancel"))
                                        .layer(ElevationIndex::ModalSurface)
                                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                                )
                                .child(
                                    Button::new("new-ssh-connect", t!("OK"))
                                        .style(ButtonStyle::Filled)
                                        .layer(ElevationIndex::ModalSurface)
                                        .disabled(host_missing)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm(window, cx)
                                        })),
                                ),
                        ),
                    ),
            )
    }
}

fn trimmed_option(text: String) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn ssh_connection_and_options(
    nickname: Option<String>,
    host: String,
    port: u16,
    username: Option<String>,
    private_key_path: Option<String>,
) -> (settings::SshConnection, SshConnectionOptions) {
    let mut args = Vec::new();
    if let Some(path) = &private_key_path {
        args.push("-i".to_string());
        args.push(path.clone());
    }
    let connection = settings::SshConnection {
        host: host.clone(),
        username: username.clone(),
        port: Some(port),
        args: Vec::new(),
        projects: Default::default(),
        nickname: nickname.clone(),
        upload_binary_over_ssh: None,
        port_forwards: None,
        connection_timeout: None,
        private_key_path,
    };
    let options = SshConnectionOptions {
        host: host.into(),
        username,
        port: Some(port),
        args: Some(args),
        nickname,
        ..Default::default()
    };
    (connection, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use project::{FakeFs, Project};

    #[test]
    fn new_connection_form_uses_the_entered_values_for_connection_options() {
        let (connection, options) = ssh_connection_and_options(
            Some("dev".to_string()),
            "host.example".to_string(),
            2222,
            Some("alice".to_string()),
            Some("/keys/id_ed25519".to_string()),
        );

        assert_eq!(connection.host, "host.example");
        assert_eq!(connection.port, Some(2222));
        assert_eq!(connection.username.as_deref(), Some("alice"));
        assert_eq!(connection.nickname.as_deref(), Some("dev"));
        assert_eq!(
            connection.private_key_path.as_deref(),
            Some("/keys/id_ed25519")
        );
        assert_eq!(options.host.to_string(), "host.example");
        assert_eq!(options.port, Some(2222));
        assert_eq!(options.username.as_deref(), Some("alice"));
        assert_eq!(options.nickname.as_deref(), Some("dev"));
        assert_eq!(
            options.args,
            Some(vec!["-i".to_string(), "/keys/id_ed25519".to_string()])
        );

        let (_, options_without_key) =
            ssh_connection_and_options(None, "host.example".to_string(), 22, None, None);
        assert_eq!(options_without_key.args, Some(Vec::new()));
    }

    #[gpui::test]
    async fn new_connection_opens_the_form_instead_of_connecting(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            crate::init(cx);
        });

        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("test window should have a workspace");
        let workspace_handle = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_, cx| {
                    RemotePanel::load(workspace_handle, cx.clone()).await
                })
            })
            .expect("test window should be open")
            .await
            .expect("remote panel should load");

        window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.new_connection(window, cx));
            })
            .expect("test window should be open");
        cx.run_until_parked();

        let modal = cx.update(|cx| workspace.read(cx).active_modal::<NewSshConnectionModal>(cx));
        assert!(modal.is_some(), "new connection should show the form first");

        window
            .update(cx, |_, window, cx| {
                modal.expect("form should be open").update(cx, |modal, cx| {
                    modal.confirm(window, cx);
                });
            })
            .expect("test window should be open");
        cx.run_until_parked();

        assert!(
            cx.update(|cx| {
                workspace
                    .read(cx)
                    .active_modal::<NewSshConnectionModal>(cx)
                    .is_some()
            }),
            "an empty host must not submit the form"
        );
    }

    #[gpui::test]
    async fn confirming_a_filled_form_does_not_double_lease_the_modal(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            crate::init(cx);
        });

        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("test window should have a workspace");
        let workspace_handle = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_, cx| {
                    RemotePanel::load(workspace_handle, cx.clone()).await
                })
            })
            .expect("test window should be open")
            .await
            .expect("remote panel should load");

        window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| panel.new_connection(window, cx));
            })
            .expect("test window should be open");
        cx.run_until_parked();

        let modal = cx
            .update(|cx| workspace.read(cx).active_modal::<NewSshConnectionModal>(cx))
            .expect("form should be open");

        // Fill in the host so `confirm` takes the submit path instead of returning
        // early on an empty host.
        window
            .update(cx, |_, window, cx| {
                modal.update(cx, |modal, cx| {
                    modal.host.update(cx, |editor, cx| {
                        editor.set_text("host.example", window, cx);
                    });
                });
            })
            .expect("test window should be open");

        // This used to panic with double_lease_panic<NewSshConnectionModal>: `confirm`
        // ran inside the modal's listener and synchronously called `toggle_modal`,
        // which tried to hide that same leased entity.
        window
            .update(cx, |_, window, cx| {
                modal.update(cx, |modal, cx| modal.confirm(window, cx));
            })
            .expect("confirm must not panic");

        assert!(
            cx.update(|cx| {
                workspace
                    .read(cx)
                    .active_modal::<NewSshConnectionModal>(cx)
                    .is_none()
            }),
            "the form must be dismissed after confirming"
        );
    }

    #[gpui::test]
    async fn settings_changes_refresh_the_server_list_without_panicking(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            crate::init(cx);
        });

        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("test window should have a workspace");
        let workspace_handle = workspace.downgrade();
        let panel = window
            .update(cx, |_, window, cx| {
                cx.spawn_in(window, async move |_, cx| {
                    RemotePanel::load(workspace_handle, cx.clone()).await
                })
            })
            .expect("test window should be open")
            .await
            .expect("remote panel should load");
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(panel.clone(), window, cx)
                })
            })
            .expect("test window should be open");

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.remote.ssh_connections = Some(vec![settings::SshConnection {
                        host: "host-a.example".to_string(),
                        ..Default::default()
                    }]);
                });
            });
        });
        cx.run_until_parked();

        let hosts: Vec<String> = cx.update(|cx| {
            panel
                .read(cx)
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
        });
        assert_eq!(hosts, vec!["host-a.example".to_string()]);
    }
}
