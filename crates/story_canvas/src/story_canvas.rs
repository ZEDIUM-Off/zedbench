use anyhow::Result;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Subscription, Task, Window, actions, div, prelude::*,
};
use gpui_kit::base::Root;
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use project::Project;
use std::path::Path;
use workspace::item::{ItemHandle, SaveOptions};
use workspace::ui::{Icon, IconName};
use workspace::{Item, OpenInPreview, Pane, Workspace};

mod openui_library;
mod openui_story;
mod zed_theme;

use openui::Library;
use openui_story::OpenUiStory;

actions!(
    storybook,
    [
        /// Previews the OpenUI story of the active `.gpui-story` file.
        OpenCanvas,
        /// Previews the OpenUI story of the active `.gpui-story` file in a split.
        OpenCanvasToTheSide
    ]
);

pub const STORY_EXTENSION: &str = "gpui-story";
const MAX_STORY_BYTES: usize = 64 * 1024;

pub fn init(cx: &mut App) {
    workspace::register_in_place_preview(in_place_preview, cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &OpenCanvas, window, cx| {
            open_active(workspace, false, window, cx)
        });
        workspace.register_action(|workspace, _: &OpenCanvasToTheSide, window, cx| {
            open_active(workspace, true, window, cx)
        });
        let Some(window) = window else {
            return;
        };
        let workspace_entity = cx.entity();
        cx.subscribe_in(&workspace_entity, window, open_added_story)
            .detach();
    })
    .detach();
}

/// Opens the canvas of a story file as the `open_in_preview` setting asks.
fn open_added_story(
    workspace: &mut Workspace,
    _: &Entity<Workspace>,
    event: &workspace::Event,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace::Event::ItemAdded { item } = event else {
        return;
    };
    if workspace.is_restoring() {
        return;
    }
    let Some(source) = item
        .act_as::<MultiBuffer>(cx)
        .and_then(|buffer| story_source(&buffer, cx))
    else {
        return;
    };
    let Some(mode) = workspace::open_in_preview_mode(item.as_ref(), cx) else {
        return;
    };
    let Some(pane) = workspace.pane_for_item_id(item.item_id()) else {
        return;
    };
    match mode {
        OpenInPreview::Editor => {}
        OpenInPreview::Side => open(workspace, source, pane, true, window, cx),
        OpenInPreview::Replace => {
            workspace.toggle_preview_in_place(item.as_ref(), window, cx);
        }
    }
}

fn in_place_preview(
    _: &mut Workspace,
    item: &dyn ItemHandle,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Box<dyn ItemHandle>> {
    let source = story_source(&item.act_as::<MultiBuffer>(cx)?, cx)?;
    init_libraries(cx);
    Some(Box::new(cx.new(|cx| StoryCanvas::new(source, window, cx))))
}

/// The buffer of `buffer` when it is a single `.gpui-story` file.
pub fn story_source(buffer: &Entity<MultiBuffer>, cx: &App) -> Option<Entity<Buffer>> {
    buffer.read(cx).as_singleton().filter(|source| {
        source.read(cx).file().is_some_and(|file| {
            Path::new(file.file_name(cx))
                .extension()
                .is_some_and(|extension| extension == STORY_EXTENSION)
        })
    })
}

fn open_active(
    workspace: &mut Workspace,
    side: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(source) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<MultiBuffer>(cx))
        .and_then(|buffer| story_source(&buffer, cx))
    {
        let origin = workspace.active_pane().clone();
        open(workspace, source, origin, side, window, cx);
    }
}

pub fn open(
    workspace: &mut Workspace,
    source: Entity<Buffer>,
    origin: Entity<Pane>,
    side: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = if side {
        workspace.adjacent_pane_of(&origin, window, cx)
    } else {
        origin
    };
    pane.update(cx, |pane, cx| {
        let existing = pane
            .items_of_type::<StoryCanvas>()
            .find(|view| view.read(cx).source == source)
            .and_then(|view| pane.index_for_item(&view));
        if let Some(index) = existing {
            pane.activate_item(index, !side, !side, window, cx);
            return;
        }
        // The libraries' globals are only needed once a story is shown, so
        // Zedbench pays for them on first use rather than at startup.
        init_libraries(cx);
        let view = cx.new(|cx| StoryCanvas::new(source, window, cx));
        pane.add_item(Box::new(view), !side, !side, None, window, cx);
    });
}

fn init_libraries(cx: &mut App) {
    if cx.has_global::<LibrariesInitialized>() {
        return;
    }
    gpui_component_story::init_embedded(cx);
    // After Kit, which already ran gpui_component::init.
    gpui_ai_gallery::init_embedded(cx);
    zed_theme::init(cx);
    cx.set_global(LibrariesInitialized);
}

struct LibrariesInitialized;

impl gpui::Global for LibrariesInitialized {}

struct StoryCanvas {
    id: EntityId,
    source: Entity<Buffer>,
    library: Result<Library, SharedString>,
    story: Option<(Entity<OpenUiStory>, Entity<Root>)>,
    focus: FocusHandle,
    _edits: Subscription,
}

impl StoryCanvas {
    fn new(source: Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let edits = cx.subscribe_in(&source, window, |canvas, buffer, event, window, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                canvas.load(&buffer.clone(), window, cx);
            }
        });
        let mut canvas = Self {
            id: cx.entity_id(),
            source: source.clone(),
            library: openui_library::library()
                .map_err(|error| format!("Invalid story library: {error}").into()),
            story: None,
            focus: cx.focus_handle(),
            _edits: edits,
        };
        canvas.load(&source, window, cx);
        canvas
    }

    fn load(&mut self, source: &Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(library) = &self.library else {
            return;
        };
        let buffer = source.read(cx);
        let text = if buffer.len() > MAX_STORY_BYTES {
            "root = Column([Text(\"Story file exceeds 64 KiB\", \"error\")])".to_owned()
        } else {
            buffer.text()
        };
        match &self.story {
            Some((story, _)) => {
                story.update(cx, |story, cx| story.set_source(library, &text, cx));
            }
            None => {
                let story = cx.new(|_| OpenUiStory::new(library, &text));
                let root = cx.new(|cx| Root::new(story.clone(), window, cx));
                self.story = Some((story, root));
            }
        }
        cx.notify();
    }
}

impl Render for StoryCanvas {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let canvas = div()
            .id("gpui-story-canvas")
            .key_context("StoryCanvas")
            .track_focus(&self.focus)
            .size_full();
        match (&self.library, &self.story) {
            (Ok(_), Some((_, root))) => canvas.child(root.clone()),
            (Err(error), _) => canvas
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(error.clone()),
            (Ok(_), None) => canvas,
        }
    }
}

impl Focusable for StoryCanvas {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<()> for StoryCanvas {}

impl Item for StoryCanvas {
    type Event = ();

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.source
            .read(cx)
            .file()
            .map(|file| format!("Preview {}", file.file_name(cx)).into())
            .unwrap_or_else(|| "GPUI Story".into())
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Eye))
    }

    // Shown in its file's tab, the canvas is the only item left for the file,
    // so closing it must not drop unsaved edits silently.
    fn is_dirty(&self, cx: &App) -> bool {
        workspace::is_in_place_preview(self.id, cx) && self.source.read(cx).is_dirty()
    }

    fn can_save(&self, cx: &App) -> bool {
        self.source.read(cx).file().is_some()
    }

    fn save(
        &mut self,
        _: SaveOptions,
        project: Entity<Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        project.update(cx, |project, cx| project.save_buffer(self.source.clone(), cx))
    }
}
