use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Subscription, Window, actions, div, prelude::*,
};
use gpui_kit::base::Root;
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use serde::Deserialize;
use std::path::Path;
use workspace::{Item, Pane, Workspace};

mod zed_theme;

actions!(
    storybook,
    [
        /// Opens the GPUI Kit story of the active `.gpui-story` file.
        OpenCanvas,
        /// Opens the GPUI Kit story of the active `.gpui-story` file in a split.
        OpenCanvasToTheSide
    ]
);

pub const STORY_EXTENSION: &str = "gpui-story";
const MAX_STORY_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Library {
    #[default]
    GpuiKit,
    GpuiAi,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoryFile {
    #[serde(default)]
    library: Library,
    story: String,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenCanvas, window, cx| {
            open_active(workspace, false, window, cx)
        });
        workspace.register_action(|workspace, _: &OpenCanvasToTheSide, window, cx| {
            open_active(workspace, true, window, cx)
        });
    })
    .detach();
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
    source: Entity<Buffer>,
    story: Result<(Library, SharedString, Entity<Root>), SharedString>,
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
            source: source.clone(),
            story: Err(SharedString::default()),
            focus: cx.focus_handle(),
            _edits: edits,
        };
        canvas.load(&source, window, cx);
        canvas
    }

    fn load(&mut self, source: &Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) {
        let (library, name) = match parse(source.read(cx)) {
            Ok(file) => (file.library, file.story),
            Err(error) => {
                self.story = Err(error);
                cx.notify();
                return;
            }
        };
        // Keep the mounted story, and its interaction state, across edits
        // that do not change which story the file names.
        if let Ok((current_library, current, _)) = &self.story
            && *current_library == library
            && current.eq_ignore_ascii_case(&name)
        {
            return;
        }
        self.story = match library {
            Library::GpuiKit => match gpui_component_story::open_story(&name, window, cx) {
                Some(container) => {
                    let root = cx.new(|cx| Root::new(container, window, cx));
                    Ok((library, name.into(), root))
                }
                None => Err(format!(
                    "Unknown GPUI Kit story {name:?}. Known stories: {}",
                    gpui_component_story::catalog()
                        .iter()
                        .map(|(title, _)| *title)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into()),
            },
            Library::GpuiAi => match name.trim().parse::<gpui_ai_gallery::StoryId>() {
                Ok(id) => {
                    let gallery = gpui_ai_gallery::embedded_story(id, cx);
                    let root = cx.new(|cx| Root::new(gallery, window, cx));
                    Ok((library, id.title().into(), root))
                }
                Err(error) => Err(format!(
                    "{error}. Known gpui-ai stories: {}",
                    gpui_ai_gallery::StoryId::ALL
                        .iter()
                        .map(|id| id.slug())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into()),
            },
        };
        cx.notify();
    }
}

fn parse(buffer: &Buffer) -> Result<StoryFile, SharedString> {
    if buffer.len() > MAX_STORY_BYTES {
        return Err("Story file exceeds 64 KiB".into());
    }
    serde_json::from_str::<StoryFile>(&buffer.text())
        .map_err(|error| format!("Invalid GPUI story: {error}").into())
}

impl Render for StoryCanvas {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let canvas = div()
            .id("gpui-story-canvas")
            .track_focus(&self.focus)
            .size_full();
        match &self.story {
            Ok((_, _, root)) => canvas.child(root.clone()),
            Err(error) => canvas
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(error.clone()),
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
        match &self.story {
            Ok((_, name, _)) => format!("Story: {name}").into(),
            Err(_) => self
                .source
                .read(cx)
                .file()
                .map(|file| format!("Story: {}", file.file_name(cx)).into())
                .unwrap_or_else(|| "GPUI Story".into()),
        }
    }
}
