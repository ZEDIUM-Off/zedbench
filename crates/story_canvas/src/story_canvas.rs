use anyhow::Result;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Subscription, Task, Window, actions, div, prelude::*,
};
use gpui_kit::base::Root;
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use project::{Project, ProjectPath};
use std::collections::HashMap;
use std::path::Path;
use util::ResultExt as _;
use util::rel_path::RelPath;
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
    workspace: &mut Workspace,
    item: &dyn ItemHandle,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Box<dyn ItemHandle>> {
    let source = story_source(&item.act_as::<MultiBuffer>(cx)?, cx)?;
    init_libraries(cx);
    let project = workspace.project().clone();
    Some(Box::new(
        cx.new(|cx| StoryCanvas::new(source, project, window, cx)),
    ))
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
    let project = workspace.project().clone();
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
        let view = cx.new(|cx| StoryCanvas::new(source, project, window, cx));
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
    project: Entity<Project>,
    library: Result<Library, SharedString>,
    story: Option<(Entity<OpenUiStory>, Entity<Root>)>,
    /// The stories this one extends, opened once and followed on edit.
    bases: HashMap<ProjectPath, Result<Entity<Buffer>, SharedString>>,
    opening: HashMap<ProjectPath, Task<()>>,
    /// Why the story could not be composed with its bases.
    problem: Option<SharedString>,
    focus: FocusHandle,
    _edits: Vec<Subscription>,
}

/// The story followed by the stories it extends, or why the chain stops.
struct Chain {
    sources: Vec<String>,
    missing: Option<ProjectPath>,
    problem: Option<SharedString>,
}

impl StoryCanvas {
    fn new(
        source: Entity<Buffer>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let edits = Self::follow(&source, window, cx);
        let mut canvas = Self {
            id: cx.entity_id(),
            source,
            project,
            library: openui_library::library()
                .map_err(|error| format!("Invalid story library: {error}").into()),
            story: None,
            bases: HashMap::default(),
            opening: HashMap::default(),
            problem: None,
            focus: cx.focus_handle(),
            _edits: vec![edits],
        };
        canvas.load(window, cx);
        canvas
    }

    fn follow(buffer: &Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe_in(buffer, window, |canvas, _, event, window, cx| {
            if matches!(event, BufferEvent::Edited { .. }) {
                canvas.load(window, cx);
            }
        })
    }

    fn chain(&self, cx: &App) -> Chain {
        let buffer = self.source.read(cx);
        let mut chain = Chain {
            sources: vec![buffer.text()],
            missing: None,
            problem: None,
        };
        let mut location = buffer
            .file()
            .map(|file| (file.worktree_id(cx), file.path().clone()));
        while let Some(target) = chain
            .sources
            .last()
            .and_then(|source| openui_library::extended_story(source))
        {
            if chain.sources.len() > openui_library::MAX_EXTENDS {
                chain.problem = Some(
                    format!(
                        "A story extends at most {} others",
                        openui_library::MAX_EXTENDS
                    )
                    .into(),
                );
                break;
            }
            let Some((worktree_id, path)) = &location else {
                chain.problem = Some("Save the story before it extends another".into());
                break;
            };
            let Some(path) = openui_library::resolve_story_path(path.as_unix_str(), target)
                .and_then(|path| RelPath::from_unix_str(&path).ok().map(RelPath::into_arc))
            else {
                chain.problem = Some(format!("{target} is outside the project").into());
                break;
            };
            let project_path = ProjectPath {
                worktree_id: *worktree_id,
                path,
            };
            match self.bases.get(&project_path) {
                Some(Ok(base)) => {
                    chain.sources.push(base.read(cx).text());
                    location = Some((project_path.worktree_id, project_path.path));
                }
                Some(Err(error)) => {
                    chain.problem = Some(error.clone());
                    break;
                }
                None => {
                    chain.missing = Some(project_path);
                    break;
                }
            }
        }
        chain
    }

    fn open_base(&mut self, path: ProjectPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening.contains_key(&path) {
            return;
        }
        let task = self
            .project
            .update(cx, |project, cx| project.open_buffer(path.clone(), cx));
        let task = cx.spawn_in(window, {
            let path = path.clone();
            async move |this, cx| {
                let result = task.await;
                this.update_in(cx, |canvas, window, cx| {
                    canvas.opening.remove(&path);
                    let base = match result {
                        Ok(buffer) => {
                            canvas._edits.push(Self::follow(&buffer, window, cx));
                            Ok(buffer)
                        }
                        Err(error) => Err(format!(
                            "Cannot open {}: {error}",
                            path.path.as_unix_str()
                        )
                        .into()),
                    };
                    canvas.bases.insert(path, base);
                    canvas.load(window, cx);
                })
                .log_err();
            }
        });
        self.opening.insert(path, task);
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.library.is_err() {
            return;
        }
        let chain = self.chain(cx);
        self.problem = chain.problem;
        if let Some(missing) = chain.missing {
            self.problem = Some(format!("Loading {}…", missing.path.as_unix_str()).into());
            self.open_base(missing, window, cx);
            if self.story.is_some() {
                cx.notify();
                return;
            }
        }
        let text = openui_library::compose_story(chain.sources.iter().map(String::as_str));
        let text = if text.len() > MAX_STORY_BYTES {
            "root = Column([Text(\"Story exceeds 64 KiB with the stories it extends\", \"error\")])"
                .to_owned()
        } else {
            text
        };
        let Ok(library) = &self.library else {
            return;
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
            (Ok(_), Some((_, root))) => canvas
                .flex()
                .flex_col()
                .when_some(self.problem.clone(), |canvas, problem| {
                    canvas.child(div().flex_none().px_2().py_1().text_sm().child(problem))
                })
                .child(div().flex_1().min_h_0().child(root.clone())),
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
