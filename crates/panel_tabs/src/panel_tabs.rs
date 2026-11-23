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
    AnyView, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, WeakEntity, Window,
    div,
};
use outline_panel::OutlinePanel;
use project::{LocalProjectFlags, Project};
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
    ItemId, OpenScope, SerializableItem, Workspace, WorkspaceDb, WorkspaceId,
    delete_unloaded_items,
    dock::{Panel as _, PanelHandle},
    item::{Item, ItemEvent},
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
    /// Terminals and the debugger run in the project of the window until
    /// they get a scope of their own.
    fn has_scope(self) -> bool {
        matches!(self, PanelKind::Project | PanelKind::Git)
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
    workspace: WeakEntity<Workspace>,
    /// The project the tab shows: the project of the workspace, or the one of
    /// its scope. What the tab opens goes to the panes of the workspace and
    /// keeps this project.
    project: Entity<Project>,
    scope: Option<TabScope>,
    panel: Arc<dyn PanelHandle>,
    scope_picker: Option<Entity<TitleBar>>,
    workspace_id: Option<WorkspaceId>,
    pending_scope: Option<Task<()>>,
}

impl PanelTab {
    pub fn new(
        workspace: &mut Workspace,
        kind: PanelKind,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let panel = build_panel(kind, workspace, project.clone(), window, cx);
        let workspace_id = workspace.database_id();
        let workspace_handle = workspace.weak_handle();
        let tab = cx.new(|_| Self {
            kind,
            workspace: workspace_handle,
            project: project.clone(),
            scope: None,
            panel,
            scope_picker: None,
            workspace_id,
            pending_scope: None,
        });
        let scope_picker =
            build_scope_picker(kind, workspace, project, tab.downgrade(), window, cx);
        tab.update(cx, |tab, _| tab.scope_picker = scope_picker);
        tab
    }

    pub fn kind(&self) -> PanelKind {
        self.kind
    }

    pub fn scope(&self) -> Option<&TabScope> {
        self.scope.as_ref()
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn panel_view(&self) -> AnyView {
        self.panel.to_any()
    }

    /// Shows the project at `paths`, on this host, in this tab. The window
    /// keeps its own project, and no other workspace opens: the project of
    /// the scope lives as long as a tab shows it.
    pub fn set_scope(
        &mut self,
        paths: PathList,
        remote: Option<RemoteConnectionOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.kind.has_scope() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let window_project = workspace.read(cx).project().clone();
        let window_paths = PathList::new(&workspace.read(cx).root_paths(cx));
        let window_remote = window_project
            .read(cx)
            .remote_connection_options(cx)
            .cloned();
        if window_paths == paths && window_remote == remote {
            self.scope = None;
            self.show_project(window_project, window, cx);
            return;
        }
        let scope = TabScope {
            paths: paths.paths().to_vec(),
            remote: remote.clone(),
        };
        if let Some(project) = workspace
            .read(cx)
            .scope_project(&paths, remote.as_ref(), cx)
        {
            self.scope = Some(scope);
            self.show_project(project, window, cx);
            return;
        }

        let app_state = workspace.read(cx).app_state().clone();
        let connect = remote
            .map(|options| remote_connection::connect_with_modal(&workspace, options, window, cx));
        self.pending_scope = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = async {
                let project = match connect {
                    None => cx.update(|_, cx| {
                        Project::local(
                            app_state.client.clone(),
                            app_state.node_runtime.clone(),
                            app_state.user_store.clone(),
                            app_state.languages.clone(),
                            app_state.fs.clone(),
                            None,
                            LocalProjectFlags::default(),
                            cx,
                        )
                    })?,
                    Some(connect) => {
                        let connected = connect.await;
                        remote_connection::dismiss_connection_modal(&workspace, cx);
                        let session = connected?.context("the connection was cancelled")?;
                        cx.update(|_, cx| {
                            Project::remote(
                                session,
                                app_state.client.clone(),
                                app_state.node_runtime.clone(),
                                app_state.user_store.clone(),
                                app_state.languages.clone(),
                                app_state.fs.clone(),
                                true,
                                cx,
                            )
                        })?
                    }
                };
                for path in paths.paths() {
                    project
                        .update(cx, |project, cx| {
                            project.find_or_create_worktree(path, true, cx)
                        })
                        .await?;
                }
                anyhow::Ok(project)
            }
            .await;
            let Some(project) = opened.context("opening the scope of a tab").log_err() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.scope = Some(scope);
                this.show_project(project, window, cx);
            })
            .log_err();
        }));
    }

    fn show_project(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_scope = None;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let tab = cx.entity().downgrade();
        let kind = self.kind;
        let (panel, scope_picker) = workspace.update(cx, |workspace, cx| {
            workspace.add_scope_project(&project);
            (
                build_panel(kind, workspace, project.clone(), window, cx),
                build_scope_picker(kind, workspace, project.clone(), tab, window, cx),
            )
        });
        // The new panel takes the place of the one that has the focus.
        let had_focus = self
            .panel
            .panel_focus_handle(cx)
            .contains_focused(window, cx);
        self.panel = panel;
        self.scope_picker = scope_picker;
        self.project = project;
        if had_focus {
            window.focus(&self.panel.panel_focus_handle(cx), cx);
        }
        if let Some(save) = self.save_state(cx) {
            save.detach_and_log_err(cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
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

fn build_panel(
    kind: PanelKind,
    workspace: &mut Workspace,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Arc<dyn PanelHandle> {
    match kind {
        PanelKind::Project => {
            Arc::new(ProjectPanel::new_in_project(workspace, project, window, cx))
        }
        PanelKind::Git => Arc::new(GitPanel::new_tab(workspace, project, window, cx)),
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
    project: Entity<Project>,
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
            let mut title_bar = TitleBar::new_in_project(
                "panel-tab-scope",
                workspace,
                project,
                multi_workspace,
                window,
                cx,
            );
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
    use editor::Editor;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Fs as _};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::{AppState, MultiWorkspace};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            project_panel::init(cx);
            workspace::init(app_state, cx);
            git_ui::init(cx);
            crate::init(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_depth = Some(0);
                });
            });
        });
    }

    async fn open_window(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (
        gpui::WindowHandle<MultiWorkspace>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let project = Project::test(fs, ["/window".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        (window, workspace, cx)
    }

    fn scoped_tab(
        workspace: &Entity<Workspace>,
        action: impl gpui::Action,
        cx: &mut VisualTestContext,
    ) -> Entity<PanelTab> {
        cx.dispatch_action(action);
        cx.run_until_parked();
        let tab = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<PanelTab>(cx)
                .expect("the panel tab is active")
        });
        tab.update_in(cx, |tab, window, cx| {
            tab.set_scope(PathList::new(&[PathBuf::from("/other")]), None, window, cx)
        });
        cx.run_until_parked();
        tab
    }

    fn assert_window_keeps_its_project(
        window: gpui::WindowHandle<MultiWorkspace>,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        let (on_screen, count) = window
            .read_with(cx, |multi_workspace, _| {
                (
                    multi_workspace.workspace().clone(),
                    multi_workspace.workspaces().count(),
                )
            })
            .expect("the window is open");
        assert_eq!(&on_screen, workspace, "the window keeps its own project");
        assert_eq!(count, 1, "a scope opens no other workspace");
    }

    #[gpui::test]
    async fn test_a_tab_opens_the_files_of_its_scope_in_the_pane_on_screen(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/window", json!({ "a.rs": "" })).await;
        fs.insert_tree("/other", json!({ "b.rs": "one" })).await;
        let (window, workspace, cx) = open_window(fs.clone(), cx).await;

        let tab = scoped_tab(&workspace, OpenProjectPanel, cx);
        assert_window_keeps_its_project(window, &workspace, cx);
        let scope_project = tab.read_with(cx, |tab, _| tab.project().clone());
        let window_project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        assert_ne!(scope_project, window_project);
        assert_eq!(
            tab.read_with(cx, |tab, cx| tab.tab_content_text(0, cx)),
            "Project · other"
        );
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

        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("the file opens in the pane on screen")
        });
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.project().cloned()),
            Some(scope_project.clone()),
            "the editor works in the project of the scope"
        );
        assert_window_keeps_its_project(window, &workspace, cx);

        editor.update_in(cx, |editor, window, cx| {
            editor.select_all(&Default::default(), window, cx);
            editor.insert("two", window, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
            })
            .await
            .expect("the file saves");
        cx.run_until_parked();
        assert_eq!(
            fs.load(Path::new("/other/b.rs"))
                .await
                .expect("b.rs exists"),
            "two\n",
            "the file saves in the project of the scope"
        );
    }

    #[gpui::test]
    async fn test_a_git_tab_opens_the_diff_of_its_scope_in_the_pane_on_screen(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/window", json!({ "a.rs": "" })).await;
        fs.insert_tree("/other", json!({ ".git": {}, "b.rs": "changed\n" }))
            .await;
        fs.set_head_and_index_for_repo(
            Path::new("/other/.git"),
            &[("b.rs", "original\n".to_string())],
        );
        let (window, workspace, cx) = open_window(fs.clone(), cx).await;

        let tab = scoped_tab(&workspace, OpenGitPanel, cx);
        assert_window_keeps_its_project(window, &workspace, cx);
        let scope_project = tab.read_with(cx, |tab, _| tab.project().clone());
        let git_panel = tab.read_with(cx, |tab, _| {
            tab.panel_view()
                .downcast::<GitPanel>()
                .expect("a git tab shows a git panel")
        });
        assert_eq!(
            git_panel.read_with(cx, |panel, _| panel.project().clone()),
            scope_project
        );

        workspace.update_in(cx, |workspace, window, cx| {
            git_ui::project_diff::ProjectDiff::deploy_at_in_project(
                workspace,
                scope_project.clone(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let diff = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<git_ui::project_diff::ProjectDiff>(cx)
                .expect("the diff opens in the pane on screen")
        });
        assert_eq!(
            diff.read_with(cx, |diff, _| diff.project().clone()),
            scope_project
        );
        assert_window_keeps_its_project(window, &workspace, cx);
    }

    #[gpui::test]
    async fn test_a_scope_can_go_back_to_the_window(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/window", json!({ "a.rs": "" })).await;
        fs.insert_tree("/other", json!({ "b.rs": "" })).await;
        let (window, workspace, cx) = open_window(fs.clone(), cx).await;

        let tab = scoped_tab(&workspace, OpenProjectPanel, cx);
        tab.update_in(cx, |tab, window, cx| {
            tab.set_scope(PathList::new(&[PathBuf::from("/window")]), None, window, cx)
        });
        cx.run_until_parked();
        let window_project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        assert_eq!(
            tab.read_with(cx, |tab, _| tab.project().clone()),
            window_project
        );
        assert_eq!(tab.read_with(cx, |tab, _| tab.scope().cloned()), None);
        assert_window_keeps_its_project(window, &workspace, cx);
    }
}
