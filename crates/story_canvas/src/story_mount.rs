//! Mounts the slots of a story in the real window, so a story only describes
//! what it adds and the rest of the window is Zedbench itself.

use std::marker::PhantomData;

use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels,
    Render, Subscription, Window, div, prelude::*, px,
};
use gpui_kit::base::Root;
use language::Buffer;
use multi_buffer::MultiBuffer;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::ui::IconName;
use workspace::{ItemHandle, StatusItemView, Workspace};

use crate::{
    StoryCanvas, ToggleStoryBottomDock, ToggleStoryLeftDock, ToggleStoryRightDock, init_libraries,
    story_source,
};
use crate::openui_story::OpenUiStory;

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

/// One slot of a mounted story. It keeps the canvas alive, which follows the
/// story file and the stories it extends.
struct SlotContent {
    _canvas: Entity<StoryCanvas>,
    story: Entity<OpenUiStory>,
    slot: &'static str,
    _changes: Subscription,
}

impl SlotContent {
    fn new(
        canvas: Entity<StoryCanvas>,
        story: Entity<OpenUiStory>,
        slot: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        let changes = cx.observe(&story, |_, _, cx| cx.notify());
        Self {
            _canvas: canvas,
            story,
            slot,
            _changes: changes,
        }
    }
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
    canvas: &Entity<StoryCanvas>,
    story: &Entity<OpenUiStory>,
    slot: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Root> {
    let content = cx.new(|cx| SlotContent::new(canvas.clone(), story.clone(), slot, cx));
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
            DockPosition::Left | DockPosition::Right => px(300.),
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

    // Unmounting the story removes it; there is no setting to hide it.
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

/// The story file behind the active item: its source, or its preview.
fn active_story(workspace: &Workspace, cx: &App) -> Option<Entity<Buffer>> {
    let item = workspace.active_item(cx)?;
    if let Some(canvas) = item.downcast::<StoryCanvas>() {
        return Some(canvas.read(cx).source.clone());
    }
    story_source(&item.act_as::<MultiBuffer>(cx)?, cx)
}

pub(crate) fn mount(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(source) = active_story(workspace, cx) else {
        workspace.show_error("Open a .gpui-story file to mount it", cx);
        return;
    };
    unmount(workspace, window, cx);
    init_libraries(cx);
    let project = workspace.project().clone();
    let canvas = cx.new(|cx| StoryCanvas::new(source, project, window, cx));
    let Some(story) = canvas.read(cx).story.as_ref().map(|(story, _)| story.clone()) else {
        workspace.show_error("The story library is invalid", cx);
        return;
    };
    let mut mounted = 0;
    mounted += usize::from(mount_panel::<LeftDock>(workspace, &canvas, &story, window, cx));
    mounted += usize::from(mount_panel::<RightDock>(workspace, &canvas, &story, window, cx));
    mounted += usize::from(mount_panel::<BottomDock>(workspace, &canvas, &story, window, cx));
    mounted += usize::from(mount_status::<StatusStart>(workspace, &canvas, &story, window, cx));
    mounted += usize::from(mount_status::<StatusEnd>(workspace, &canvas, &story, window, cx));
    if mounted == 0 {
        workspace.show_error(
            "The story defines none of leftDock, rightDock, bottomDock, statusStart or statusEnd",
            cx,
        );
    }
}

fn mount_panel<D: DockSlot>(
    workspace: &mut Workspace,
    canvas: &Entity<StoryCanvas>,
    story: &Entity<OpenUiStory>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !story.read(cx).defines(D::SLOT) {
        return false;
    }
    let root = slot_root(canvas, story, D::SLOT, window, cx);
    let panel = cx.new(|cx| StoryPanel::<D> {
        root,
        focus: cx.focus_handle(),
        _dock: PhantomData,
    });
    workspace.add_panel(panel, window, cx);
    workspace.open_panel::<StoryPanel<D>>(window, cx);
    true
}

fn mount_status<S: StatusSlot>(
    workspace: &mut Workspace,
    canvas: &Entity<StoryCanvas>,
    story: &Entity<OpenUiStory>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !story.read(cx).defines(S::SLOT) {
        return false;
    }
    let root = slot_root(canvas, story, S::SLOT, window, cx);
    let item = cx.new(|_| StoryStatusItem::<S> {
        root,
        _side: PhantomData,
    });
    workspace.status_bar().update(cx, |status_bar, cx| {
        if S::LEFT {
            status_bar.add_left_item(item, window, cx);
        } else {
            status_bar.add_right_item(item, window, cx);
        }
    });
    true
}

pub(crate) fn unmount(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    unmount_panel::<LeftDock>(workspace, window, cx);
    unmount_panel::<RightDock>(workspace, window, cx);
    unmount_panel::<BottomDock>(workspace, window, cx);
    unmount_status::<StatusStart>(workspace, cx);
    unmount_status::<StatusEnd>(workspace, cx);
}

fn unmount_panel<D: DockSlot>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(panel) = workspace.panel::<StoryPanel<D>>(cx) {
        workspace.remove_panel(&panel, window, cx);
    }
}

fn unmount_status<S: StatusSlot>(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.status_bar().update(cx, |status_bar, cx| {
        if let Some(position) = status_bar.position_of_item::<StoryStatusItem<S>>() {
            status_bar.remove_item_at(position, cx);
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
