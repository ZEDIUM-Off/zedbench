//! A real Zedbench window inside a story preview. The canvas builds a real
//! `Workspace` from Zedbench's sources on a made-up project, and a story only
//! adds its slots to it, so the window follows Zedbench as it changes.

use std::cell::Cell;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels,
    Render, Subscription, Task, WeakEntity, Window, div, prelude::*, px,
};
use gpui_kit::base::Root;
use title_bar::{TitleBar, TitleBarSlot};
use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
use project::{LocalProjectFlags, Project, ProjectPath};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::ui::IconName;
use workspace::{AppState, ItemHandle, StatusItemView, Workspace};

use crate::openui_story::OpenUiStory;
use crate::{ToggleStoryBottomDock, ToggleStoryLeftDock, ToggleStoryRightDock};

/// The made-up project the window opens. It is written to Zedbench's own
/// temporary directory on the machine that runs it, so it also works when the
/// story comes from a remote project. That directory is private to the user,
/// unlike `/tmp`, which matters because the window trusts the project.
const FIXTURE: &[(&str, &str)] = &[
    (
        "README.md",
        "# acme-app\n\nA made-up shop used by the stories of Zedbench.\n",
    ),
    (
        "Cargo.toml",
        "[package]\nname = \"acme-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    ),
    (
        "src/main.rs",
        "mod cart;\nmod checkout;\n\nfn main() {\n    let mut cart = cart::Cart::default();\n    cart.add(\"notebook\", 2);\n    checkout::pay(&cart);\n}\n",
    ),
    (
        "src/cart.rs",
        "#[derive(Default)]\npub struct Cart {\n    pub items: Vec<(String, u32)>,\n}\n\nimpl Cart {\n    pub fn add(&mut self, name: &str, quantity: u32) {\n        self.items.push((name.to_owned(), quantity));\n    }\n}\n",
    ),
    (
        "src/checkout.rs",
        "use crate::cart::Cart;\n\npub fn pay(cart: &Cart) {\n    let count: u32 = cart.items.iter().map(|(_, quantity)| quantity).sum();\n    println!(\"Paying for {count} items\");\n}\n",
    ),
];

const OPENED_FILE: &str = "src/checkout.rs";

/// A dock a story can fill, named after the slot that fills it.
pub(crate) trait DockSlot: 'static {
    const SLOT: &'static str;
    const PERSISTENT_NAME: &'static str;
    const TOOLTIP: &'static str;
    const POSITION: DockPosition;
    const PRIORITY: u32;
    fn toggle_action() -> Box<dyn Action>;
}

pub(crate) struct LeftDock;
pub(crate) struct RightDock;
pub(crate) struct BottomDock;

impl DockSlot for LeftDock {
    const SLOT: &'static str = "leftDock";
    const PERSISTENT_NAME: &'static str = "StoryLeftDock";
    const TOOLTIP: &'static str = "Story: Left Dock";
    const POSITION: DockPosition = DockPosition::Left;
    const PRIORITY: u32 = 1001;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleStoryLeftDock)
    }
}

impl DockSlot for RightDock {
    const SLOT: &'static str = "rightDock";
    const PERSISTENT_NAME: &'static str = "StoryRightDock";
    const TOOLTIP: &'static str = "Story: Right Dock";
    const POSITION: DockPosition = DockPosition::Right;
    const PRIORITY: u32 = 1002;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleStoryRightDock)
    }
}

impl DockSlot for BottomDock {
    const SLOT: &'static str = "bottomDock";
    const PERSISTENT_NAME: &'static str = "StoryBottomDock";
    const TOOLTIP: &'static str = "Story: Bottom Dock";
    const POSITION: DockPosition = DockPosition::Bottom;
    const PRIORITY: u32 = 1003;
    fn toggle_action() -> Box<dyn Action> {
        Box::new(ToggleStoryBottomDock)
    }
}

/// A side of the status bar a story can fill.
pub(crate) trait StatusSlot: 'static {
    const SLOT: &'static str;
    const LEFT: bool;
}

pub(crate) struct StatusStart;
pub(crate) struct StatusEnd;

impl StatusSlot for StatusStart {
    const SLOT: &'static str = "statusStart";
    const LEFT: bool = true;
}

impl StatusSlot for StatusEnd {
    const SLOT: &'static str = "statusEnd";
    const LEFT: bool = false;
}

/// The real window of a story preview.
pub(crate) struct StoryWindow {
    workspace: Entity<Workspace>,
    _setup: Task<()>,
}

impl StoryWindow {
    pub(crate) fn new<V: 'static>(
        story: &Entity<OpenUiStory>,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Result<Self, &'static str> {
        let app_state = AppState::try_global(cx).ok_or("Zedbench has no app state")?;
        let project = Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            LocalProjectFlags {
                init_worktree_trust: false,
                watch_global_configs: false,
            },
            cx,
        );
        // Without a database id the workspace never writes a session.
        let workspace =
            cx.new(|cx| Workspace::new(None, project.clone(), app_state.clone(), window, cx));
        let owner = cx.entity_id();
        workspace.update(cx, |workspace, cx| {
            // The window's title and edited state belong to the workspace
            // that holds this preview. An id no workspace has keeps this one
            // from ever owning them, and without a multi-workspace it cannot
            // reach the real window's sidebar.
            workspace.set_multi_workspace(WeakEntity::new_invalid(), Rc::new(Cell::new(owner)), cx);
        });
        let story_window = Self {
            _setup: Self::open_fixture(&project, &workspace, window, cx),
            workspace,
        };
        story_window.sync(story, window, cx);
        // The title bar is added by an observer of new workspaces, which runs
        // only once the current update ends.
        window.defer(cx, {
            let workspace = story_window.workspace.downgrade();
            let story = story.downgrade();
            move |window, cx| {
                if let (Some(workspace), Some(story)) = (workspace.upgrade(), story.upgrade()) {
                    sync_title(&workspace, &story, window, cx);
                }
            }
        });
        Ok(story_window)
    }

    fn open_fixture<V: 'static>(
        project: &Entity<Project>,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Task<()> {
        let project = project.downgrade();
        let workspace = workspace.downgrade();
        let directory = paths::temp_dir().join("story-window").join("acme-app");
        let written = cx.background_spawn({
            let directory = directory.clone();
            async move { write_fixture(&directory) }
        });
        cx.spawn_in(window, async move |_, cx| {
            if written.await.log_err().is_none() {
                return;
            }
            let Some(worktree) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(&directory, true, cx)
                })
                .log_err()
            else {
                return;
            };
            let Some((worktree, _)) = worktree.await.log_err() else {
                return;
            };
            // Zedbench wrote every file of this project, so the preview does
            // not open in Restricted Mode.
            project
                .update(cx, |project, cx| {
                    let worktree_store = project.worktree_store();
                    let worktree_id = worktree.read(cx).id();
                    if let Some(trusted) = TrustedWorktrees::try_get_global(cx) {
                        trusted.update(cx, |trusted, cx| {
                            trusted.trust(
                                &worktree_store,
                                [PathTrust::Worktree(worktree_id)].into_iter().collect(),
                                cx,
                            )
                        });
                    }
                })
                .log_err();
            let Some(path) = RelPath::from_unix_str(OPENED_FILE).log_err() else {
                return;
            };
            let opened = workspace.update_in(cx, |workspace, window, cx| {
                let project_path = ProjectPath {
                    worktree_id: worktree.read(cx).id(),
                    path: path.into_arc(),
                };
                workspace.open_path(project_path, None, false, window, cx)
            });
            if let Some(opened) = opened.log_err() {
                opened.await.log_err();
            }
        })
    }

    /// The workspace with its actions and key context. A real window gets
    /// them from `MultiWorkspace`; without them, every action dispatched in
    /// the preview bubbles up to the window's own workspace and acts on it.
    pub(crate) fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let workspace = self.workspace.clone();
        let key_context = workspace.update(cx, |workspace, cx| workspace.key_context(cx));
        workspace
            .update(cx, |workspace, cx| {
                workspace.actions(div().flex(), window, cx)
            })
            .key_context(key_context)
            .size_full()
            .child(workspace)
            .into_any_element()
    }

    /// Adds the slots the story defines and removes those it no longer
    /// defines, so editing the story updates the window.
    pub(crate) fn sync(&self, story: &Entity<OpenUiStory>, window: &mut Window, cx: &mut App) {
        self.workspace.update(cx, |workspace, cx| {
            sync_panel::<LeftDock>(workspace, story, window, cx);
            sync_panel::<RightDock>(workspace, story, window, cx);
            sync_panel::<BottomDock>(workspace, story, window, cx);
            sync_status::<StatusStart>(workspace, story, window, cx);
            sync_status::<StatusEnd>(workspace, story, window, cx);
        });
        sync_title(&self.workspace, story, window, cx);
    }
}

fn write_fixture(directory: &PathBuf) -> anyhow::Result<()> {
    for (path, content) in FIXTURE {
        let path = directory.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// One slot of the story, rendered where the window puts it.
struct SlotContent {
    story: Entity<OpenUiStory>,
    slot: &'static str,
    _changes: Subscription,
}

impl Render for SlotContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let slot = self.slot;
        let content = self
            .story
            .update(cx, |story, cx| story.render_slot(slot, window, cx));
        div().size_full().child(content)
    }
}

fn slot_root(
    story: &Entity<OpenUiStory>,
    slot: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Root> {
    let content = cx.new(|cx| SlotContent {
        story: story.clone(),
        slot,
        _changes: cx.observe(story, |_, _, cx| cx.notify()),
    });
    cx.new(|cx| Root::new(content, window, cx))
}

pub(crate) struct StoryPanel<D: DockSlot> {
    root: Entity<Root>,
    focus: FocusHandle,
    _dock: PhantomData<D>,
}

impl<D: DockSlot> Render for StoryPanel<D> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(D::PERSISTENT_NAME)
            .track_focus(&self.focus)
            .size_full()
            .child(self.root.clone())
    }
}

impl<D: DockSlot> Focusable for StoryPanel<D> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl<D: DockSlot> EventEmitter<PanelEvent> for StoryPanel<D> {}

impl<D: DockSlot> Panel for StoryPanel<D> {
    fn persistent_name() -> &'static str {
        D::PERSISTENT_NAME
    }

    fn panel_key() -> &'static str {
        D::PERSISTENT_NAME
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        D::POSITION
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == D::POSITION
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        match D::POSITION {
            DockPosition::Bottom => px(240.),
            // The window shares the preview tab, so narrow docks leave room
            // for the editor.
            DockPosition::Left | DockPosition::Right => px(240.),
        }
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Eye)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some(D::TOOLTIP)
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        D::toggle_action()
    }

    fn activation_priority(&self) -> u32 {
        D::PRIORITY
    }
}

pub(crate) struct StoryStatusItem<S: StatusSlot> {
    root: Entity<Root>,
    _side: PhantomData<S>,
}

impl<S: StatusSlot> Render for StoryStatusItem<S> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().flex().items_center().child(self.root.clone())
    }
}

impl<S: StatusSlot> StatusItemView for StoryStatusItem<S> {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    // The story decides whether the item exists; there is no setting to hide it.
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

fn sync_panel<D: DockSlot>(
    workspace: &mut Workspace,
    story: &Entity<OpenUiStory>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let wanted = story.read(cx).defines(D::SLOT);
    match (workspace.panel::<StoryPanel<D>>(cx), wanted) {
        (Some(panel), false) => workspace.remove_panel(&panel, window, cx),
        (None, true) => {
            let root = slot_root(story, D::SLOT, window, cx);
            let panel = cx.new(|cx| StoryPanel::<D> {
                root,
                focus: cx.focus_handle(),
                _dock: PhantomData,
            });
            workspace.add_panel(panel, window, cx);
            workspace.open_panel::<StoryPanel<D>>(window, cx);
        }
        _ => {}
    }
}

fn sync_status<S: StatusSlot>(
    workspace: &mut Workspace,
    story: &Entity<OpenUiStory>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let wanted = story.read(cx).defines(S::SLOT);
    let status_bar = workspace.status_bar().clone();
    let position = status_bar
        .read(cx)
        .position_of_item::<StoryStatusItem<S>>();
    match (position, wanted) {
        (Some(position), false) => {
            status_bar.update(cx, |status_bar, cx| status_bar.remove_item_at(position, cx));
        }
        (None, true) => {
            let root = slot_root(story, S::SLOT, window, cx);
            let item = cx.new(|_| StoryStatusItem::<S> {
                root,
                _side: PhantomData,
            });
            status_bar.update(cx, |status_bar, cx| {
                if S::LEFT {
                    status_bar.add_left_item(item, window, cx);
                } else {
                    status_bar.add_right_item(item, window, cx);
                }
            });
        }
        _ => {}
    }
}

const TITLE_SLOTS: [(&str, TitleBarSlot); 3] = [
    ("titleStart", TitleBarSlot::Start),
    ("titleCenter", TitleBarSlot::Center),
    ("titleEnd", TitleBarSlot::End),
];

/// Draws the window's title bar without the controls that would act on the
/// real window, and adds the title slots the story defines.
fn sync_title(
    workspace: &Entity<Workspace>,
    story: &Entity<OpenUiStory>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(title_bar) = workspace
        .read(cx)
        .titlebar_item()
        .and_then(|item| item.downcast::<TitleBar>().ok())
    else {
        return;
    };
    title_bar.update(cx, |title_bar, cx| {
        title_bar.set_embedded(cx);
        for (name, slot) in TITLE_SLOTS {
            match (title_bar.slot(slot).is_some(), story.read(cx).defines(name)) {
                (true, false) => title_bar.set_slot(slot, None, cx),
                (false, true) => {
                    let root = slot_root(story, name, window, cx);
                    title_bar.set_slot(slot, Some(root.into()), cx);
                }
                _ => {}
            }
        }
    });
}

pub(crate) fn toggle<D: DockSlot>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_panel_focus::<StoryPanel<D>>(window, cx);
}
