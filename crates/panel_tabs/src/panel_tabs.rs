//! The native panels of Zed — project, git, outline, terminal and debug — as
//! tabs of a pane. Each tab has its own scope: a project or worktree that the
//! person picks with the same pickers as the title bar, without changing the
//! project of the window.

use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use debugger_ui::debugger_panel::DebugPanel;
use git_ui::git_panel::GitPanel;
use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    WeakEntity, Window, div,
};
use outline_panel::OutlinePanel;
use project::Project;
use project_panel::ProjectPanel;
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, rc::Rc, sync::Arc};
use terminal_view::terminal_panel::TerminalPanel;
use theme::ActiveTheme as _;
use title_bar::TitleBar;
use ui::{Icon, IconName, prelude::*};
use util::{ResultExt as _, path_list::PathList};
use workspace::{
    ItemId, MultiWorkspace, OpenMode, OpenScope, SerializableItem, Workspace, WorkspaceDb,
    WorkspaceId, delete_unloaded_items,
    dock::{Panel as _, PanelHandle},
    item::{Item, ItemEvent, ItemHandle},
};
use zed_actions::panel_tabs::{
    OpenDebugPanel, OpenGitPanel, OpenOutlinePanel, OpenProjectPanel, OpenTerminalPanel,
};

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<PanelTab>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenProjectPanel, window, cx| {
            open_panel_tab(workspace, PanelKind::Project, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenGitPanel, window, cx| {
            open_panel_tab(workspace, PanelKind::Git, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenOutlinePanel, window, cx| {
            open_panel_tab(workspace, PanelKind::Outline, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenTerminalPanel, window, cx| {
            open_panel_tab(workspace, PanelKind::Terminal, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenDebugPanel, window, cx| {
            open_panel_tab(workspace, PanelKind::Debug, window, cx);
        });
    })
    .detach();
}

fn open_panel_tab(
    workspace: &mut Workspace,
    kind: PanelKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let tab = PanelTab::new(workspace, kind, window, cx);
    workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanelKind {
    Project,
    Git,
    Outline,
    Terminal,
    Debug,
}

impl PanelKind {
    fn label(self) -> &'static str {
        match self {
            PanelKind::Project => "Project",
            PanelKind::Git => "Git",
            PanelKind::Outline => "Outline",
            PanelKind::Terminal => "Terminal",
            PanelKind::Debug => "Debug",
        }
    }

    fn icon(self) -> IconName {
        match self {
            PanelKind::Project => IconName::FileTree,
            PanelKind::Git => IconName::GitBranch,
            PanelKind::Outline => IconName::ListTree,
            PanelKind::Terminal => IconName::Terminal,
            PanelKind::Debug => IconName::Debug,
        }
    }

    /// The outline follows the editor on screen: it has no project to pick.
    fn has_scope(self) -> bool {
        self != PanelKind::Outline
    }
}

/// The project of a tab, when it is not the project of the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabScope {
    pub paths: Vec<PathBuf>,
    pub remote: Option<RemoteConnectionOptions>,
}

#[derive(Serialize, Deserialize)]
struct SerializedPanelTab {
    kind: PanelKind,
    scope: Option<TabScope>,
}

pub struct PanelTab {
    kind: PanelKind,
    /// The workspace whose pane holds the tab.
    host: WeakEntity<Workspace>,
    /// The workspace of the scope: the host, or one that the window keeps
    /// in the background for this project.
    scope_workspace: WeakEntity<Workspace>,
    scope: Option<TabScope>,
    panel: Arc<dyn PanelHandle>,
    scope_picker: Option<Entity<TitleBar>>,
    workspace_id: Option<WorkspaceId>,
    pending_scope: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PanelTab {
    pub fn new(
        workspace: &mut Workspace,
        kind: PanelKind,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = build_panel(kind, workspace, window, cx);
        let host = cx.entity().downgrade();
        let workspace_id = workspace.database_id();
        let tab = cx.new(|_| Self {
            kind,
            host: host.clone(),
            scope_workspace: host,
            scope: None,
            panel,
            scope_picker: None,
            workspace_id,
            pending_scope: None,
            _subscriptions: Vec::new(),
        });
        let scope_picker = build_scope_picker(kind, workspace, tab.downgrade(), window, cx);
        tab.update(cx, |tab, _| tab.scope_picker = scope_picker);
        tab
    }

    pub fn kind(&self) -> PanelKind {
        self.kind
    }

    pub fn scope(&self) -> Option<&TabScope> {
        self.scope.as_ref()
    }

    pub fn panel_view(&self) -> AnyView {
        self.panel.to_any()
    }

    /// Shows the project at `paths` in this tab. The window keeps showing
    /// its own project.
    pub fn set_scope(
        &mut self,
        paths: PathList,
        remote: Option<RemoteConnectionOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let host_paths = PathList::new(&host.read(cx).root_paths(cx));
        let host_remote = host
            .read(cx)
            .project()
            .read(cx)
            .remote_connection_options(cx)
            .cloned();
        if host_paths == paths && host_remote == remote {
            self.scope = None;
            self.show_workspace(host, window, cx);
            return;
        }
        let scope = TabScope {
            paths: paths.paths().to_vec(),
            remote: remote.clone(),
        };
        let Some(multi_workspace) = host
            .read(cx)
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
        else {
            return;
        };
        if let Some(existing) =
            multi_workspace
                .read(cx)
                .workspace_for_paths(&paths, remote.as_ref(), cx)
        {
            self.scope = Some(scope);
            self.show_workspace(existing, window, cx);
            return;
        }
        // A new workspace takes the focus of the window, but the person stays
        // on this tab.
        let focused = window.focused(cx);
        let open: Task<Result<Entity<Workspace>>> = if remote.is_none() {
            // The open of the title bar would bring a project that the window
            // already holds on screen: the tab opens it in the background.
            let opening = Workspace::new_local(
                paths.paths().to_vec(),
                host.read(cx).app_state().clone(),
                window.window_handle().downcast::<MultiWorkspace>(),
                None,
                None,
                OpenMode::Add,
                cx,
            );
            cx.spawn(async move |_, _| Ok(opening.await?.workspace))
        } else {
            multi_workspace.update(cx, |multi_workspace, cx| {
                let host = host.clone();
                multi_workspace.find_or_create_workspace(
                    paths,
                    remote,
                    None,
                    move |options, window, cx| {
                        remote_connection::connect_with_modal(&host, options, window, cx)
                    },
                    None,
                    OpenMode::Add,
                    None,
                    window,
                    cx,
                )
            })
        };
        self.pending_scope = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = open.await;
            remote_connection::dismiss_connection_modal(&host, cx);
            let Some(scope_workspace) = opened.context("opening the scope of a tab").log_err()
            else {
                return;
            };
            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    // Opening a remote project brings it on screen: the window
                    // goes back to its own project.
                    if multi_workspace.workspace() != &host {
                        multi_workspace.activate(host.clone(), None, window, cx);
                    }
                })
                .log_err();
            this.update_in(cx, |this, window, cx| {
                this.scope = Some(scope);
                this.show_workspace(scope_workspace, window, cx);
                if let Some(focused) = focused {
                    window.focus(&focused, cx);
                }
            })
            .log_err();
        }));
    }

    fn show_workspace(
        &mut self,
        scope_workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_scope = None;
        let tab = cx.entity().downgrade();
        let kind = self.kind;
        let (panel, scope_picker) = scope_workspace.update(cx, |workspace, cx| {
            (
                build_panel(kind, workspace, window, cx),
                build_scope_picker(kind, workspace, tab, window, cx),
            )
        });
        if kind == PanelKind::Project
            && scope_workspace.downgrade() != self.host
            && let Ok(project_panel) = panel.to_any().downcast::<ProjectPanel>()
        {
            let host = self.host.clone();
            project_panel.update(cx, |project_panel, _| project_panel.set_opens_in(host));
        }
        self._subscriptions.clear();
        if scope_workspace.downgrade() != self.host {
            self._subscriptions.push(cx.subscribe_in(
                &scope_workspace,
                window,
                |this, scope_workspace, event, window, cx| {
                    if let workspace::Event::ItemAdded { item } = event {
                        this.bring_to_host(scope_workspace, item.as_ref(), window, cx);
                    }
                },
            ));
        }
        self.panel = panel;
        self.scope_picker = scope_picker;
        self.scope_workspace = scope_workspace.downgrade();
        if let Some(save) = self.save_state(cx) {
            save.detach_and_log_err(cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    /// The workspace of the scope is not on screen: what the person opens
    /// from it (a diff, a file, a definition) moves to the pane on screen.
    /// The tabs that the workspace restores, or adds without focus, stay
    /// there.
    fn bring_to_host(
        &self,
        scope_workspace: &Entity<Workspace>,
        item: &dyn ItemHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if item.downcast::<PanelTab>().is_some()
            || scope_workspace.read(cx).is_restoring()
            || !item.item_focus_handle(cx).is_focused(window)
        {
            return;
        }
        let host = self.host.clone();
        let scope_workspace = scope_workspace.downgrade();
        let item_id = item.item_id();
        // The pane on screen reads each of its tabs when it takes an item,
        // this one too: the move cannot run while this tab is being updated.
        window.defer(cx, move |window, cx| {
            move_to_host(host, scope_workspace, item_id, window, cx);
        });
    }

    fn save_state(&self, cx: &mut Context<Self>) -> Option<Task<Result<()>>> {
        let workspace_id = self.workspace_id?;
        let item_id = cx.entity_id().as_u64();
        let json = serde_json::to_string(&SerializedPanelTab {
            kind: self.kind,
            scope: self.scope.clone(),
        })
        .log_err()?;
        let db = PanelTabsDb::global(cx);
        Some(cx.background_spawn(async move { db.save_tab(item_id, workspace_id, json).await }))
    }
}

fn move_to_host(
    host: WeakEntity<Workspace>,
    scope_workspace: WeakEntity<Workspace>,
    item_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    let (Some(host), Some(scope_workspace)) = (host.upgrade(), scope_workspace.upgrade()) else {
        return;
    };
    let scope_on_screen = host
        .read(cx)
        .multi_workspace()
        .and_then(|multi_workspace| multi_workspace.upgrade())
        .is_some_and(|multi_workspace| multi_workspace.read(cx).workspace() == &scope_workspace);
    if scope_on_screen {
        return;
    }
    let Some(source) = scope_workspace
        .read(cx)
        .panes()
        .iter()
        .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
        .cloned()
    else {
        return;
    };
    let destination = host.read(cx).active_pane().clone();
    let index = destination.read(cx).items_len();
    workspace::move_item(&source, &destination, item_id, index, true, window, cx);
}

fn build_panel(
    kind: PanelKind,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Arc<dyn PanelHandle> {
    match kind {
        PanelKind::Project => Arc::new(ProjectPanel::new(workspace, window, cx)),
        PanelKind::Git => Arc::new(GitPanel::new_tab(workspace, window, cx)),
        PanelKind::Outline => Arc::new(OutlinePanel::new_tab(workspace, window, cx)),
        PanelKind::Terminal => {
            let panel = cx.new(|cx| TerminalPanel::new_tab(workspace, window, cx));
            // A panel of the dock starts its first terminal when the dock
            // opens; a tab is open from the start.
            panel.update(cx, |panel, cx| panel.set_active(true, window, cx));
            Arc::new(panel)
        }
        PanelKind::Debug => Arc::new(DebugPanel::new(workspace, window, cx)),
    }
}

/// The project, worktree and branch pickers of the title bar, acting on the
/// scope of the tab.
fn build_scope_picker(
    kind: PanelKind,
    workspace: &mut Workspace,
    tab: WeakEntity<PanelTab>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<TitleBar>> {
    kind.has_scope().then(|| {
        let open_scope: OpenScope = Rc::new(move |paths, remote, window, cx| {
            let tab = tab.clone();
            window.defer(cx, move |window, cx| {
                tab.update(cx, |tab, cx| tab.set_scope(paths, remote, window, cx))
                    .log_err();
            });
        });
        let multi_workspace = workspace.multi_workspace().cloned();
        cx.new(|cx| {
            let mut title_bar =
                TitleBar::new("panel-tab-scope", workspace, multi_workspace, window, cx);
            title_bar.set_scope_picker(open_scope, cx);
            title_bar
        })
    })
}

impl EventEmitter<ItemEvent> for PanelTab {}

impl Focusable for PanelTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.panel_focus_handle(cx)
    }
}

impl Render for PanelTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .when_some(self.scope_picker.clone(), |this, scope_picker| {
                this.child(
                    h_flex()
                        .flex_none()
                        .px_1()
                        .py_0p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(scope_picker),
                )
            })
            .child(div().flex_1().min_h_0().child(self.panel.to_any()))
    }
}

impl Item for PanelTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        let label = self.kind.label();
        let folder = self
            .scope
            .as_ref()
            .and_then(|scope| scope.paths.first())
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        match folder {
            Some(folder) => format!("{label} · {folder}").into(),
            None => label.into(),
        }
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(self.kind.icon()))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_id = workspace.database_id();
        if let Some(save) = self.save_state(cx) {
            save.detach_and_log_err(cx);
        }
    }
}

impl SerializableItem for PanelTab {
    fn serialized_item_kind() -> &'static str {
        "PanelTab"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = PanelTabsDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "panel_tabs", &db, cx)
    }

    fn deserialize(
        _: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = PanelTabsDb::global(cx);
        let saved = db
            .tab(item_id, workspace_id)
            .and_then(|json| json.context("no panel tab was saved for this item"))
            .and_then(|json| Ok(serde_json::from_str::<SerializedPanelTab>(&json)?));
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => return Task::ready(Err(error)),
        };
        let tab = workspace.update(cx, |workspace, cx| {
            PanelTab::new(workspace, saved.kind, window, cx)
        });
        if let (Ok(tab), Some(scope)) = (&tab, saved.scope) {
            let tab = tab.downgrade();
            // The window may not hold its workspaces yet.
            window.defer(cx, move |window, cx| {
                tab.update(cx, |tab, cx| {
                    tab.set_scope(PathList::new(&scope.paths), scope.remote, window, cx)
                })
                .log_err();
            });
        }
        Task::ready(tab)
    }

    fn serialize(
        &mut self,
        _: &mut Workspace,
        _: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        self.save_state(cx)
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

pub struct PanelTabsDb(ThreadSafeConnection);

impl Domain for PanelTabsDb {
    const NAME: &str = stringify!(PanelTabsDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE panel_tabs (
            workspace_id INTEGER,
            item_id INTEGER,
            state TEXT NOT NULL,
            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(PanelTabsDb, [WorkspaceDb]);

impl PanelTabsDb {
    query! {
        pub async fn save_tab(
            item_id: ItemId,
            workspace_id: WorkspaceId,
            state: String
        ) -> Result<()> {
            INSERT INTO panel_tabs(item_id, workspace_id, state)
            VALUES (?1, ?2, ?3)
            ON CONFLICT DO UPDATE SET state = ?3
        }
    }

    query! {
        pub fn tab(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT state
            FROM panel_tabs
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    async fn test_a_tab_shows_the_project_it_picks(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            project_panel::init(cx);
            workspace::init(app_state, cx);
            crate::init(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_depth = Some(0);
                });
            });
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/window", json!({ "a.rs": "" })).await;
        fs.insert_tree("/other", json!({ "b.rs": "" })).await;
        let project = Project::test(fs.clone(), ["/window".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        cx.dispatch_action(OpenProjectPanel);
        cx.run_until_parked();
        let tab = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<PanelTab>(cx)
                .expect("the project tab is active")
        });
        assert_eq!(tab.read_with(cx, |tab, _| tab.kind()), PanelKind::Project);
        assert!(tab.read_with(cx, |tab, _| tab.scope_picker.is_some()));

        tab.update_in(cx, |tab, window, cx| {
            tab.set_scope(PathList::new(&[PathBuf::from("/other")]), None, window, cx)
        });
        cx.run_until_parked();

        let window_roots = window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).root_paths(cx)
            })
            .expect("the window is open");
        assert_eq!(
            window_roots,
            vec![Arc::from(std::path::Path::new("/window"))]
        );
        let tab_roots = tab.read_with(cx, |tab, cx| {
            tab.scope_workspace
                .upgrade()
                .map(|workspace| workspace.read(cx).root_paths(cx))
                .unwrap_or_default()
        });
        assert_eq!(tab_roots, vec![Arc::from(std::path::Path::new("/other"))]);
        assert_eq!(
            tab.read_with(cx, |tab, cx| tab.tab_content_text(0, cx)),
            "Project · other"
        );
        let scope_project = tab.read_with(cx, |tab, cx| {
            tab.scope_workspace
                .upgrade()
                .map(|workspace| workspace.read(cx).project().clone())
                .expect("the tab has a scope")
        });
        let worktree_id = scope_project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("the scope has a worktree")
        });
        let project_panel = tab.read_with(cx, |tab, _| {
            tab.panel_view()
                .downcast::<ProjectPanel>()
                .expect("a project tab shows a project panel")
        });
        project_panel.update_in(cx, |panel, window, cx| {
            panel.select_path_for_test(
                project::ProjectPath {
                    worktree_id,
                    path: util::rel_path::rel_path("b.rs").into(),
                },
                cx,
            );
            window.focus(&panel.focus_handle(cx), cx);
        });
        cx.dispatch_action(project_panel::Open);
        cx.run_until_parked();

        let opened = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
        });
        assert_eq!(
            opened.map(|path| (path.worktree_id, path.path)),
            Some((worktree_id, util::rel_path::rel_path("b.rs").into())),
            "a file of the scope opens in the pane on screen"
        );
        let on_screen = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        assert_eq!(on_screen, workspace, "the window keeps its own project");
    }

    #[gpui::test]
    async fn test_what_a_scope_opens_moves_to_the_pane_on_screen(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            project_panel::init(cx);
            workspace::init(app_state, cx);
            git_ui::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/window", json!({ "a.rs": "" })).await;
        fs.insert_tree("/other", json!({ "b.rs": "", "c.rs": "" }))
            .await;
        let project = Project::test(fs.clone(), ["/window".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        cx.dispatch_action(OpenProjectPanel);
        cx.run_until_parked();
        let tab = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<PanelTab>(cx)
                .expect("the project tab is active")
        });
        tab.update_in(cx, |tab, window, cx| {
            tab.set_scope(PathList::new(&[PathBuf::from("/other")]), None, window, cx)
        });
        cx.run_until_parked();
        let scope_workspace = tab
            .read_with(cx, |tab, _| tab.scope_workspace.upgrade())
            .expect("the tab has a scope");
        assert_ne!(scope_workspace, workspace);
        let worktree_id = scope_workspace.read_with(cx, |scope_workspace, cx| {
            scope_workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("the scope has a worktree")
        });
        let path = |name: &str| project::ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path(name).into(),
        };

        // The pane on screen holds the tab itself: the move must not read it
        // while it is being updated.
        scope_workspace
            .update_in(cx, |scope_workspace, window, cx| {
                scope_workspace.open_path(path("b.rs"), None, true, window, cx)
            })
            .await
            .expect("b.rs opens");
        cx.run_until_parked();
        let host_paths = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .filter_map(|item| item.project_path(cx))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(host_paths(cx), vec![path("b.rs")]);

        // The workspace of the scope restores its own tabs without focus.
        scope_workspace
            .update_in(cx, |scope_workspace, window, cx| {
                scope_workspace.open_path(path("c.rs"), None, false, window, cx)
            })
            .await
            .expect("c.rs opens");
        cx.run_until_parked();
        assert_eq!(host_paths(cx), vec![path("b.rs")]);

        // The git panel of the scope opens its diff in the workspace of the
        // scope.
        scope_workspace.update_in(cx, |scope_workspace, window, cx| {
            git_ui::project_diff::ProjectDiff::deploy_at(scope_workspace, None, window, cx)
        });
        cx.run_until_parked();
        let diff_on_screen = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<git_ui::project_diff::ProjectDiff>(cx)
                .is_some()
        });
        assert!(diff_on_screen, "the diff of the scope opens on screen");
        let on_screen = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        assert_eq!(on_screen, workspace, "the window keeps its own project");
    }
}
