//! The layouts of a window as vertical tabs, and under them what Herdr Core
//! keeps alive: agents and terminals, on this machine and on the others.
//! A layout arranges views; the core owns the programs. Nothing here starts
//! or ends a program unless the person asks for exactly that.

mod herdr_processes;
mod herdr_terminal;

use anyhow::Result;
use collections::HashSet;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, actions::SelectAll};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, AsyncWindowContext, Axis, ClickEvent,
    Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, PromptLevel, Role, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window, actions, px,
};
use herdr::{AgentState, ConnectionState, HerdrStore, RunParams, TerminalRecord};
use herdr_processes::{HerdrProcesses, ProcessView};
use project::Project;
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use theme::ActiveTheme as _;
use ui::{
    ContextMenu, CountBadge, Disclosure, IconButtonShape, Indicator, PopoverMenu, Tooltip,
    prelude::*, right_click_menu, tooltip_container,
};
use util::ResultExt as _;
use workspace::{
    CloseIntent, LayoutId, LayoutOutline, LayoutScope, OpenMode, PathList, SavedWorkspaceLayouts,
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotifyTaskExt as _,
};

pub use herdr_terminal::{
    HerdrTerminal, agent_color, find_herdr_terminal, is_agent, open_herdr_terminal, terminal_icon,
};

actions!(
    layouts_panel,
    [
        /// Moves focus to the layouts panel, or back to the center.
        ToggleFocus,
        /// Shows or hides the layouts panel.
        Toggle,
        /// Adds an empty layout to the window and shows it.
        NewLayout,
        /// Shows the next layout of the window.
        NextLayout,
        /// Shows the previous layout of the window.
        PreviousLayout,
    ]
);

const PANEL_KEY: &str = "LayoutsPanel";
const OVERVIEW: &str = "Overview";
const PLACING_DELAY: std::time::Duration = std::time::Duration::from_millis(800);
const DEFAULT_WIDTH: Pixels = px(248.);
const NARROW_WIDTH: Pixels = px(44.);
/// Under this width the panel shows icons only.
const NARROW_LIMIT: Pixels = px(120.);
const PREVIEW_WIDTH: Pixels = px(300.);
const PREVIEW_HEIGHT: Pixels = px(108.);
const PREVIEW_ROWS: usize = 6;

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<HerdrTerminal>(cx);
    workspace::register_serializable_item::<HerdrProcesses>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<LayoutsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<LayoutsPanel>(window, cx) {
                workspace.close_panel::<LayoutsPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &NewLayout, window, cx| {
            new_layout(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NextLayout, window, cx| {
            workspace.activate_adjacent_layout(true, window, cx);
        });
        workspace.register_action(|workspace, _: &PreviousLayout, window, cx| {
            workspace.activate_adjacent_layout(false, window, cx);
        });
        workspace.register_action(|workspace, _: &herdr::NewTerminal, window, cx| {
            new_herdr_terminal(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &herdr::OpenPrograms, window, cx| {
            open_processes(workspace, window, cx);
        });
    })
    .detach();
}

/// Adds a layout next to the one on screen, in its group.
fn new_layout(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> LayoutId {
    let active = workspace.active_layout();
    let group = workspace
        .layouts()
        .iter()
        .find(|layout| layout.id() == active)
        .map(|layout| layout.group().clone())
        .unwrap_or_default();
    let mut number = workspace.layouts().len() + 1;
    let name = loop {
        let name = format!("Layout {number}");
        if !workspace
            .layouts()
            .iter()
            .any(|layout| **layout.name() == *name)
        {
            break name;
        }
        number += 1;
    };
    workspace.create_layout(name, group, window, cx)
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedLayoutsPanel {
    #[serde(default)]
    folded_groups: Vec<String>,
    /// Layouts, workspaces and worktrees whose terminals are folded.
    #[serde(default)]
    folded_nodes: Vec<String>,
    #[serde(default)]
    wide_width: Option<f32>,
}

/// Where a new terminal starts. Shown before anything runs, so that no
/// program starts in a place the person did not read.
#[derive(Clone)]
struct RunTarget {
    machine: Option<String>,
    folder: Option<String>,
    label: SharedString,
}

enum EditTarget {
    Rename(LayoutId),
    Group(LayoutId),
    Command(RunTarget),
}

struct Editing {
    target: EditTarget,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

struct LayoutSummary {
    id: LayoutId,
    name: SharedString,
    group: SharedString,
    scope: Option<LayoutScope>,
    active: bool,
    tabs: usize,
    agents: usize,
    terminals: usize,
    /// Tabs on a terminal that Herdr Core ended or no longer has.
    ended: usize,
    blocked: usize,
    working: usize,
    /// Its Herdr tabs, where they work.
    herdr: Vec<(Place, TerminalRow)>,
}

impl LayoutSummary {
    /// The machine of its scope: `None` for a global layout.
    fn machine(&self) -> Option<Option<String>> {
        self.scope.as_ref().map(|scope| scope.machine.clone())
    }

    fn icon(&self) -> IconName {
        if self.agents > 0 {
            IconName::ZedAgent
        } else if self.terminals + self.ended > 0 {
            IconName::Terminal
        } else if self.tabs > 0 {
            IconName::File
        } else {
            IconName::Screen
        }
    }

    fn detail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(scope) = &self.scope {
            parts.push(folder_name(&scope.folder));
        }
        if self.agents > 0 {
            parts.push(count_of(self.agents, "agent"));
        }
        if self.terminals > 0 {
            parts.push(count_of(self.terminals, "terminal"));
        }
        if self.ended > 0 {
            parts.push(format!("{} ended", self.ended));
        }
        let others = self
            .tabs
            .saturating_sub(self.agents + self.terminals + self.ended);
        if others > 0 {
            parts.push(count_of(others, "tab"));
        }
        if parts.is_empty() || (parts.len() == 1 && self.scope.is_some()) {
            parts.push("Empty".to_string());
            parts.join(" · ")
        } else {
            parts.join(" · ")
        }
    }
}

fn count_of(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

struct TerminalRow {
    address: SharedString,
    name: SharedString,
    detail: SharedString,
    tooltip: SharedString,
    state: Option<AgentState>,
    is_agent: bool,
    is_live: bool,
}

pub struct LayoutsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<HerdrStore>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    folded_groups: HashSet<SharedString>,
    folded_nodes: HashSet<String>,
    /// The width to give back when the panel leaves its narrow form.
    wide_width: Pixels,
    editing: Option<Editing>,
    error: Option<SharedString>,
    /// The layouts that other workspaces saved, to open them from here.
    saved_elsewhere: Vec<SavedWorkspaceLayouts>,
    /// The Herdr programs the window has had a tab for, by address and
    /// start. `None` until read from the database.
    placed: Option<HashSet<(String, u64)>>,
    placing: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LayoutsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
        let serialized = cx
            .background_spawn({
                let kvp = kvp.clone();
                async move { kvp.read_kvp(PANEL_KEY) }
            })
            .await
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedLayoutsPanel>(&json).log_err())
            .unwrap_or_default();
        let placed_key =
            workspace.read_with(&cx, |workspace, _| workspace.database_id().map(placed_key))?;
        let placed = match placed_key {
            Some(key) => cx
                .background_spawn(async move { kvp.read_kvp(&key) })
                .await
                .log_err()
                .flatten()
                .and_then(|json| serde_json::from_str::<Vec<(String, u64)>>(&json).log_err())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_entity = cx.entity();
            let project = workspace.project().clone();
            let panel = cx.new(|cx| Self::new(workspace_entity, project, serialized, window, cx));
            panel.update(cx, |panel, cx| {
                panel.placed = Some(placed.into_iter().collect());
                panel.schedule_placing(window, cx);
            });
            panel
        })
    }

    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        serialized: SerializedLayoutsPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = HerdrStore::global(cx);
        let catalog = workspace::saved_layout_catalog(cx);
        cx.spawn(async move |this, cx| {
            let catalog = catalog.await;
            this.update(cx, |this, cx| {
                this.saved_elsewhere = catalog;
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        Self {
            saved_elsewhere: Vec::new(),
            placed: None,
            placing: None,
            _subscriptions: vec![
                cx.observe_in(&store, window, |this, _, window, cx| {
                    this.schedule_placing(window, cx);
                    cx.notify();
                }),
                cx.subscribe(&workspace, |_, _, _: &workspace::Event, cx| cx.notify()),
            ],
            workspace: workspace.downgrade(),
            project,
            store,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            folded_groups: serialized
                .folded_groups
                .into_iter()
                .map(SharedString::from)
                .collect(),
            folded_nodes: serialized.folded_nodes.into_iter().collect(),
            wide_width: serialized.wide_width.map_or(DEFAULT_WIDTH, px),
            editing: None,
            error: None,
        }
    }

    /// Places the new programs of Herdr Core a moment after they start: a
    /// terminal the window started gets its tab first, where it asked.
    fn schedule_placing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.placing.is_some() || self.placed.is_none() {
            return;
        }
        self.placing = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PLACING_DELAY).await;
            this.update_in(cx, |this, window, cx| {
                this.placing = None;
                this.place_new_programs(window, cx);
            })
            .log_err();
        }));
    }

    /// Gives each live program of Herdr Core that the window never had a
    /// tab for a tab in the layout of its place, which it adds when missing.
    /// A program is placed once: closing its tab keeps it out.
    fn place_new_programs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let roots = self.place_roots(cx);
        let shown = shown_herdr_addresses(workspace.read(cx), cx);
        let store = self.store.read(cx);
        if !store.is_connected() {
            return;
        }
        let Some(placed) = self.placed.as_mut() else {
            return;
        };
        let known = store
            .terminals()
            .into_iter()
            .map(|record| (record.address.clone(), record.created_ms))
            .collect::<HashSet<_>>();
        let before = placed.len();
        placed.retain(|key| known.contains(key));
        let mut changed = placed.len() != before;
        let mut new = Vec::new();
        for record in store.terminals() {
            if !record.is_live() {
                continue;
            }
            if placed.insert((record.address.clone(), record.created_ms)) {
                changed = true;
                if !shown.contains(record.address.as_str()) {
                    let place = place_of(record.machine(), record.current_folder(), &roots);
                    if !place.workspace.is_empty() {
                        new.push((record.address.clone(), place));
                    }
                }
            }
        }
        if changed {
            self.save_placed(cx);
        }
        if !new.is_empty() {
            self.update_workspace(window, cx, move |workspace, window, cx| {
                place_programs(workspace, new, window, cx);
            });
        }
    }

    fn save_placed(&self, cx: &mut Context<Self>) {
        let Some(placed) = &self.placed else {
            return;
        };
        let Some(key) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).database_id())
            .map(placed_key)
        else {
            return;
        };
        let placed = placed.iter().cloned().collect::<Vec<_>>();
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let json = serde_json::to_string(&placed)?;
            kvp.write_kvp(key, json).await
        })
        .detach_and_log_err(cx);
    }

    /// The folders that hold workspaces: those of the project and of the
    /// other workspaces of Zed. A home folder holds none, or every program
    /// would land in it.
    fn place_roots(&self, cx: &App) -> Vec<(Option<String>, String)> {
        let store = self.store.read(cx);
        let mut roots = project_roots(&self.project, &self.store, cx);
        for saved in &self.saved_elsewhere {
            let machine = match &saved.remote {
                Some(remote) => match remote_machine(remote, store) {
                    Some(machine) => machine,
                    None => continue,
                },
                None => None,
            };
            for path in saved.paths.paths() {
                let root = (machine.clone(), path.to_string_lossy().into_owned());
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        roots
    }

    fn serialize(&self, cx: &mut Context<Self>) {
        let serialized = SerializedLayoutsPanel {
            folded_groups: self
                .folded_groups
                .iter()
                .map(|group| group.to_string())
                .collect(),
            folded_nodes: self.folded_nodes.iter().cloned().collect(),
            wide_width: Some(f32::from(self.wide_width)),
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let json = serde_json::to_string(&serialized)?;
            kvp.write_kvp(PANEL_KEY.to_string(), json).await
        })
        .detach_and_log_err(cx);
    }

    fn width(&self, workspace: &Workspace, cx: &Context<Self>) -> Pixels {
        let panel = cx.entity();
        workspace
            .left_dock()
            .read(cx)
            .stored_panel_size_state(&panel)
            .and_then(|state| state.size)
            .unwrap_or(DEFAULT_WIDTH)
    }

    fn set_narrow(&mut self, narrow: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let width = if narrow {
            let current = self.width(workspace.read(cx), cx);
            if current >= NARROW_LIMIT {
                self.wide_width = current;
            }
            NARROW_WIDTH
        } else {
            self.wide_width.max(NARROW_LIMIT)
        };
        let dock = workspace.read(cx).left_dock().clone();
        let panel = cx.entity();
        self.serialize(cx);
        // The dock reads the panel to resize it, which it cannot do while the
        // panel is the one being updated.
        window.defer(cx, move |window, cx| {
            dock.update(cx, |dock, cx| {
                dock.resize_panel(&panel, Some(width), window, cx);
            });
        });
    }

    fn layout_summaries(
        &self,
        workspace: &Workspace,
        roots: &[(Option<String>, String)],
        cx: &App,
    ) -> Vec<LayoutSummary> {
        let store = self.store.read(cx);
        let active = workspace.active_layout();
        workspace
            .layouts()
            .iter()
            .map(|layout| {
                let mut summary = LayoutSummary {
                    id: layout.id(),
                    name: layout.name().clone(),
                    group: layout.group().clone(),
                    scope: layout.scope().cloned(),
                    active: layout.id() == active,
                    tabs: 0,
                    agents: 0,
                    terminals: 0,
                    ended: 0,
                    blocked: 0,
                    working: 0,
                    herdr: self.layout_terminals(workspace, layout.id(), roots, cx),
                };
                for pane in workspace.layout_panes(layout.id()) {
                    for item in pane.read(cx).items() {
                        summary.tabs += 1;
                        let Some(terminal) = item.downcast::<HerdrTerminal>() else {
                            continue;
                        };
                        let terminal = terminal.read(cx);
                        if terminal.is_ended(cx) {
                            summary.ended += 1;
                            continue;
                        }
                        match store.terminal(terminal.address()) {
                            Some(record) if is_agent(record) => {
                                summary.agents += 1;
                                match record.agent_state() {
                                    Some(AgentState::Blocked) => summary.blocked += 1,
                                    Some(AgentState::Working) => summary.working += 1,
                                    _ => {}
                                }
                            }
                            _ => summary.terminals += 1,
                        }
                    }
                }
                summary
            })
            .collect()
    }

    /// The Herdr tabs of a layout, one line per terminal, where they work.
    fn layout_terminals(
        &self,
        workspace: &Workspace,
        layout: LayoutId,
        roots: &[(Option<String>, String)],
        cx: &App,
    ) -> Vec<(Place, TerminalRow)> {
        let store = self.store.read(cx);
        let mut seen = HashSet::default();
        let mut entries = Vec::new();
        for pane in workspace.layout_panes(layout) {
            for item in pane.read(cx).items() {
                let Some(terminal) = item.downcast::<HerdrTerminal>() else {
                    continue;
                };
                let terminal = terminal.read(cx);
                if !seen.insert(terminal.address().clone()) {
                    continue;
                }
                let is_ended = terminal.is_ended(cx);
                let record = terminal
                    .record(cx)
                    .or_else(|| store.terminal(terminal.address()).filter(|_| !is_ended));
                let entry = match record {
                    Some(record) => (
                        place_of(record.machine(), record.current_folder(), roots),
                        terminal_row(record, !is_ended),
                    ),
                    None => (
                        Place::default(),
                        TerminalRow {
                            address: terminal.address().clone(),
                            name: terminal.address().clone(),
                            detail: "Gone from Herdr Core".into(),
                            tooltip: terminal.address().clone(),
                            state: None,
                            is_agent: false,
                            is_live: false,
                        },
                    ),
                };
                entries.push(entry);
            }
        }
        entries
    }

    fn start_edit(
        &mut self,
        target: EditTarget,
        text: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, {
            let editor = editor.clone();
            move |_, _, event: &EditorEvent, window, cx| {
                if let EditorEvent::Blurred = event {
                    let editor = editor.clone();
                    // Focus settles after the event: a click in the field
                    // blurs it for a moment.
                    cx.defer_in(window, move |this, window, cx| {
                        let is_current = this
                            .editing
                            .as_ref()
                            .is_some_and(|editing| editing.editor == editor);
                        if is_current && !editor.focus_handle(cx).is_focused(window) {
                            this.editing = None;
                            cx.notify();
                        }
                    });
                }
            }
        });
        editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        self.editing = Some(Editing {
            target,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn confirm_edit(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let text = editing.editor.read(cx).text(cx).trim().to_string();
        match editing.target {
            EditTarget::Rename(layout) => {
                if !text.is_empty() {
                    self.update_workspace(window, cx, move |workspace, window, cx| {
                        workspace.rename_layout(layout, text, window, cx);
                    });
                }
            }
            EditTarget::Group(layout) => {
                self.update_workspace(window, cx, move |workspace, window, cx| {
                    workspace.set_layout_group(layout, text, window, cx);
                });
            }
            EditTarget::Command(target) => {
                if !text.is_empty() {
                    self.run(&target, Some(text), window, cx);
                }
            }
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    /// Runs `update` once the panel is done with its own update: the
    /// workspace and its docks read their panels, and cannot read the one
    /// that is being updated.
    fn update_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| update(workspace, window, cx))
                .log_err();
        });
    }

    fn report<T>(&mut self, result: Result<T>, cx: &mut Context<Self>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
                None
            }
        }
    }

    /// Starts a shell, or `line` in a shell, where `target` says, and opens
    /// its tab in the layout on screen.
    fn run(
        &mut self,
        target: &RunTarget,
        line: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let run = run_in_herdr(self.workspace.clone(), target, line, window, cx);
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| {
                this.report(result, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn open_terminal(&mut self, address: &str, window: &mut Window, cx: &mut Context<Self>) {
        let address = address.to_string();
        self.update_workspace(window, cx, move |workspace, window, cx| {
            open_herdr_terminal(workspace, &address, window, cx);
        });
    }

    fn open_terminal_in_new_layout(
        &mut self,
        address: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address = address.to_string();
        let name = name.to_string();
        self.update_workspace(window, cx, move |workspace, window, cx| {
            open_in_new_layout(workspace, &address, &name, window, cx);
        });
    }

    fn end_program(
        &mut self,
        address: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = end_program(&self.store, address, name, window, cx);
        cx.spawn(async move |this, cx| {
            let result = end.await;
            this.update(cx, |this, cx| {
                this.report(result, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn remove_terminal(&mut self, address: &str, cx: &mut Context<Self>) {
        let remove = self.store.read(cx).remove(address, cx);
        cx.spawn(async move |this, cx| {
            let result = remove.await;
            this.update(cx, |this, cx| {
                this.report(result, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn close_layout(&mut self, layout: LayoutId, window: &mut Window, cx: &mut Context<Self>) {
        self.update_workspace(window, cx, move |workspace, window, cx| {
            workspace
                .remove_layout(layout, window, cx)
                .detach_and_log_err(cx);
        });
    }

    fn toggle_group(&mut self, group: &SharedString, cx: &mut Context<Self>) {
        if !self.folded_groups.remove(group) {
            self.folded_groups.insert(group.clone());
        }
        self.serialize(cx);
        cx.notify();
    }

    fn toggle_node(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.folded_nodes.remove(key) {
            self.folded_nodes.insert(key.to_string());
        }
        self.serialize(cx);
        cx.notify();
    }

    fn group_color(group: &str, cx: &App) -> gpui::Hsla {
        let index = group.bytes().fold(0u32, |sum, byte| {
            sum.wrapping_mul(31).wrapping_add(u32::from(byte))
        });
        cx.theme().players().color_for_participant(index).cursor
    }

    fn render_header(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        if narrow {
            return v_flex()
                .w_full()
                .py_1()
                .items_center()
                .child(
                    IconButton::new("layouts-expand", IconName::ChevronRight)
                        .aria_label("Show Names")
                        .icon_size(IconSize::Small)
                        .shape(IconButtonShape::Square)
                        .tooltip(Tooltip::text("Show Names"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_narrow(false, window, cx);
                        })),
                )
                .into_any_element();
        }
        h_flex()
            .w_full()
            .h(px(32.))
            .px_2p5()
            .gap_1()
            .child(
                Label::new("Layouts")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(self.render_new_layout_menu(cx))
            .when(self.store.read(cx).is_connected(), |header| {
                header.child(self.render_new_terminal_menu(cx))
            })
            .child(
                IconButton::new("herdr-processes", IconName::ListTree)
                    .aria_label("Herdr Programs")
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .tooltip(|_, cx| {
                        Tooltip::for_action("Herdr Programs", &herdr::OpenPrograms, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.update_workspace(window, cx, |workspace, window, cx| {
                            open_processes(workspace, window, cx);
                        });
                    })),
            )
            .child(
                IconButton::new("layouts-narrow", IconName::ChevronLeft)
                    .aria_label("Show Icons Only")
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .tooltip(Tooltip::text("Show Icons Only"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_narrow(true, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_layout(
        &self,
        index: usize,
        layout: &LayoutSummary,
        groups: &[SharedString],
        scopes: &[LayoutScope],
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = layout.id;
        let colors = cx.theme().colors();
        let icon_color = if layout.active {
            Color::Accent
        } else {
            Color::Muted
        };
        let rename_editor = self
            .editing
            .as_ref()
            .and_then(|editing| match editing.target {
                EditTarget::Rename(editing_id) if editing_id == id => Some(editing.editor.clone()),
                _ => None,
            });

        let row = h_flex()
            .id(("layout", index))
            .role(Role::Tab)
            .aria_label(format!("{} · {}", layout.name, layout.detail()))
            .aria_selected(layout.active)
            .w_full()
            .rounded_sm()
            .cursor_pointer()
            .when(layout.active, |row| row.bg(colors.ghost_element_selected))
            .hover(|style| style.bg(colors.ghost_element_hover))
            .map(|row| {
                if narrow {
                    row.h(px(32.)).justify_center().child(
                        div()
                            .relative()
                            .child(
                                Icon::new(layout.icon())
                                    .size(IconSize::Small)
                                    .color(icon_color),
                            )
                            .when(layout.blocked > 0, |icon| {
                                icon.child(
                                    div()
                                        .absolute()
                                        .top(px(-3.))
                                        .right(px(-4.))
                                        .child(Indicator::dot().color(Color::Error)),
                                )
                            }),
                    )
                } else {
                    row.gap_2()
                        .pr_1p5()
                        .py_1()
                        .child(
                            div()
                                .w(px(2.))
                                .h(px(26.))
                                .rounded_full()
                                .when(layout.active, |bar| bar.bg(colors.text_accent)),
                        )
                        .child(
                            Icon::new(layout.icon())
                                .size(IconSize::Small)
                                .color(icon_color),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .map(|column| match rename_editor {
                                    Some(editor) => column.child(editor),
                                    None => column.child(
                                        Label::new(layout.name.clone())
                                            .size(LabelSize::Small)
                                            .truncate(),
                                    ),
                                })
                                .child(
                                    Label::new(layout.detail())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .when(layout.working > 0 && layout.blocked == 0, |row| {
                            row.child(Indicator::dot().color(Color::Warning))
                        })
                        .when(layout.blocked > 0, |row| {
                            row.child(CountBadge::new(layout.blocked))
                        })
                        .when(!layout.herdr.is_empty(), |row| {
                            let key = layout_key(id);
                            row.child(
                                Disclosure::new(
                                    ("layout-terminals", index),
                                    !self.folded_nodes.contains(&key),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_node(&key, cx)),
                                ),
                            )
                        })
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() > 1 {
                    this.rename_layout(id, window, cx);
                } else {
                    this.update_workspace(window, cx, move |workspace, window, cx| {
                        workspace.activate_layout(id, window, cx);
                    });
                }
            }))
            .hoverable_tooltip({
                let workspace = self.workspace.clone();
                let store = self.store.clone();
                move |_, cx| {
                    let preview = workspace
                        .upgrade()
                        .and_then(|workspace| {
                            LayoutPreview::build(workspace.read(cx), id, &store, cx)
                        })
                        .unwrap_or_default();
                    cx.new(|_| preview).into()
                }
            });

        let panel = cx.entity().downgrade();
        let groups = groups.to_vec();
        let group = layout.group.clone();
        let scope = layout.scope.clone();
        let scopes = scopes.to_vec();
        let is_only_layout = index == 0 && groups.is_empty() && self.is_single_layout(cx);
        right_click_menu(("layout-menu", index))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let groups = groups.clone();
                let group = group.clone();
                let scope = scope.clone();
                let scopes = scopes.clone();
                ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.entry("Rename", None, {
                        let panel = panel.clone();
                        move |window, cx| {
                            panel
                                .update(cx, |this, cx| this.rename_layout(id, window, cx))
                                .log_err();
                        }
                    });
                    menu = menu.separator().header("Group");
                    for name in &groups {
                        menu = menu.toggleable_entry(
                            name.clone(),
                            *name == group,
                            IconPosition::Start,
                            None,
                            {
                                let panel = panel.clone();
                                let name = name.clone();
                                move |window, cx| {
                                    let name = name.clone();
                                    panel
                                        .update(cx, |this, cx| {
                                            this.update_workspace(
                                                window,
                                                cx,
                                                move |workspace, window, cx| {
                                                    workspace
                                                        .set_layout_group(id, name, window, cx);
                                                },
                                            );
                                        })
                                        .log_err();
                                }
                            },
                        );
                    }
                    if !group.is_empty() {
                        menu = menu.entry("No Group", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |this, cx| {
                                        this.update_workspace(
                                            window,
                                            cx,
                                            move |workspace, window, cx| {
                                                workspace.set_layout_group(id, "", window, cx);
                                            },
                                        );
                                    })
                                    .log_err();
                            }
                        });
                    }
                    menu = menu.entry("New Group…", None, {
                        let panel = panel.clone();
                        move |window, cx| {
                            panel
                                .update(cx, |this, cx| {
                                    this.start_edit(
                                        EditTarget::Group(id),
                                        "",
                                        "Name of the group",
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    });
                    menu = menu.separator().header("Scope");
                    let choices = std::iter::once(None)
                        .chain(scopes.iter().cloned().map(Some))
                        .chain(
                            scope
                                .clone()
                                .filter(|scope| !scopes.contains(scope))
                                .map(Some),
                        );
                    for choice in choices {
                        let label: SharedString = match &choice {
                            Some(choice) => scope_label(choice).into(),
                            None => "Global".into(),
                        };
                        menu = menu.toggleable_entry(
                            label,
                            choice == scope,
                            IconPosition::Start,
                            None,
                            {
                                let panel = panel.clone();
                                move |window, cx| {
                                    let choice = choice.clone();
                                    panel
                                        .update(cx, |this, cx| {
                                            this.update_workspace(
                                                window,
                                                cx,
                                                move |workspace, window, cx| {
                                                    workspace
                                                        .set_layout_scope(id, choice, window, cx);
                                                },
                                            );
                                        })
                                        .log_err();
                                }
                            },
                        );
                    }
                    if !is_only_layout {
                        menu = menu.separator().entry("Close Layout", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |this, cx| this.close_layout(id, window, cx))
                                    .log_err();
                            }
                        });
                    }
                    menu
                })
            })
            .into_any_element()
    }

    /// The layouts that other workspaces saved, under the name of their
    /// folder. Opening one opens its workspace in the window.
    fn render_saved_elsewhere(
        &self,
        current: Option<workspace::WorkspaceId>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let places = self
            .saved_elsewhere
            .iter()
            .filter(|place| Some(place.workspace_id) != current)
            .collect::<Vec<_>>();
        if places.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let mut list = v_flex().w_full().px_1().gap_px().child(
            div().px_1p5().child(
                Label::new("Other Workspaces")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
        );
        for place in places {
            let key = format!("saved:{}", i64::from(place.workspace_id));
            let is_folded = self.folded_nodes.contains(&key);
            let folder = place
                .paths
                .paths()
                .first()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            let host = place.remote.as_ref().map(|remote| remote.display_name());
            list = list.child(
                h_flex()
                    .id(SharedString::from(key.clone()))
                    .role(Role::Button)
                    .aria_label(format!("Workspace {}", folder_name(&folder)))
                    .aria_expanded(!is_folded)
                    .w_full()
                    .gap_1()
                    .px_1p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.ghost_element_hover))
                    .child(
                        Icon::new(if is_folded {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(folder_name(&folder))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .when_some(host, |row, host| {
                        row.child(
                            Label::new(host)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .tooltip(Tooltip::text(display_folder(&folder)))
                    .on_click(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| this.toggle_node(&key, cx)
                    })),
            );
            if is_folded {
                continue;
            }
            for layout in &place.layouts {
                let workspace_id = place.workspace_id;
                let paths = place.paths.clone();
                let remote = place.remote.clone();
                let id = layout.id;
                let mut detail = Vec::new();
                if !layout.group.is_empty() {
                    detail.push(layout.group.to_string());
                }
                if let Some(scope) = &layout.scope {
                    detail.push(folder_name(&scope.folder));
                }
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("{key}:{id:?}")))
                        .role(Role::Tab)
                        .aria_label(format!("{} · {}", layout.name, folder_name(&folder)))
                        .w_full()
                        .gap_2()
                        .pl(indent(2))
                        .pr_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(colors.ghost_element_hover))
                        .child(
                            Icon::new(IconName::Screen)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(layout.name.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                        .when(!detail.is_empty(), |row| {
                            row.child(
                                Label::new(detail.join(" · "))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Placeholder)
                                    .truncate(),
                            )
                        })
                        .tooltip(Tooltip::text(format!(
                            "Open {} on this layout",
                            display_folder(&folder)
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_saved_layout(
                                workspace_id,
                                paths.clone(),
                                remote.clone(),
                                id,
                                window,
                                cx,
                            );
                        })),
                );
            }
        }
        Some(list.into_any_element())
    }

    /// Opens a saved workspace in the window, on one of its layouts. The
    /// workspace on screen closes first, asking about unsaved files; its
    /// Herdr programs keep running.
    fn open_saved_layout(
        &mut self,
        workspace_id: workspace::WorkspaceId,
        paths: PathList,
        remote: Option<RemoteConnectionOptions>,
        layout: LayoutId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_workspace(window, cx, move |workspace, window, cx| {
            let Some(multi_workspace) = workspace
                .multi_workspace()
                .and_then(|multi_workspace| multi_workspace.upgrade())
            else {
                return;
            };
            let show_layout = workspace::show_saved_layout_on_open(workspace_id, layout, cx);
            let close = workspace.prepare_to_close(CloseIntent::ReplaceWindow, window, cx);
            let modal_workspace = cx.entity();
            let weak_workspace = cx.entity().downgrade();
            window
                .spawn(cx, async move |cx| {
                    show_layout.await.log_err();
                    if !close.await? {
                        return anyhow::Ok(());
                    }
                    let open = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                        multi_workspace.find_or_create_workspace(
                            paths,
                            remote,
                            None,
                            move |options, window, cx| {
                                remote_connection::connect_with_modal(
                                    &modal_workspace,
                                    options,
                                    window,
                                    cx,
                                )
                            },
                            None,
                            OpenMode::Activate,
                            None,
                            window,
                            cx,
                        )
                    })?;
                    let opened = open.await?;
                    // A workspace that was open already keeps the layout it
                    // showed: show the one asked for.
                    opened.update_in(cx, |opened, window, cx| {
                        opened.activate_layout(layout, window, cx);
                    })?;
                    anyhow::Ok(())
                })
                .detach_and_notify_err(weak_workspace, window, cx);
        });
    }

    fn is_single_layout(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_none_or(|workspace| workspace.read(cx).layouts().len() < 2)
    }

    fn rename_layout(&mut self, layout: LayoutId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .layouts()
                .iter()
                .find(|candidate| candidate.id() == layout)
                .map(|layout| layout.name().clone())
        }) else {
            return;
        };
        self.start_edit(
            EditTarget::Rename(layout),
            &name,
            "Name of the layout",
            window,
            cx,
        );
    }

    fn render_layouts(
        &self,
        layouts: &[LayoutSummary],
        scopes: &[LayoutScope],
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut groups = Vec::<SharedString>::new();
        for layout in layouts {
            if !layout.group.is_empty() && !groups.contains(&layout.group) {
                groups.push(layout.group.clone());
            }
        }

        let mut list = v_flex().w_full().px_1().gap_px();
        for (index, layout) in layouts.iter().enumerate() {
            if layout.group.is_empty() {
                list = list.child(self.render_layout(index, layout, &groups, scopes, narrow, cx));
                list = list.children(self.render_terminals_of(layout, narrow, cx));
            }
        }
        // Groups sit under the machine of their scope, so that two
        // workspaces of the same name on two machines stay apart.
        let mut sections = Vec::<(Option<Option<String>>, SharedString)>::new();
        for layout in layouts {
            let section = (layout.machine(), layout.group.clone());
            if !layout.group.is_empty() && !sections.contains(&section) {
                sections.push(section);
            }
        }
        sections.sort_by(|a, b| a.0.cmp(&b.0));
        let mut current_machine = None;
        let mut machine_is_folded = false;
        for (group_index, (machine, group)) in sections.iter().enumerate() {
            if current_machine.as_ref() != Some(machine) {
                current_machine = Some(machine.clone());
                machine_is_folded = false;
                if let Some(machine) = machine {
                    let key = format!("machine:{}", machine.as_deref().unwrap_or(""));
                    machine_is_folded = self.folded_nodes.contains(&key);
                    if !narrow {
                        list = list.child(self.render_machine_header(
                            &key,
                            machine.as_deref(),
                            machine_is_folded,
                            cx,
                        ));
                    }
                }
            }
            if machine_is_folded {
                continue;
            }
            let is_folded = self.folded_groups.contains(group);
            let members = layouts
                .iter()
                .enumerate()
                .filter(|(_, layout)| layout.group == *group && layout.machine() == *machine)
                .collect::<Vec<_>>();
            let blocked = members
                .iter()
                .map(|(_, layout)| layout.blocked)
                .sum::<usize>();
            let color = Self::group_color(group, cx);
            let header = h_flex()
                .id(("layout-group", group_index))
                .role(Role::Button)
                .aria_label(format!("Group {group}"))
                .aria_expanded(!is_folded)
                .w_full()
                .mt_1p5()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .map(|header| {
                    if narrow {
                        header
                            .h(px(12.))
                            .justify_center()
                            .child(div().w(px(16.)).h(px(3.)).rounded_full().bg(color))
                    } else {
                        header
                            .gap_1p5()
                            .px_1p5()
                            .py_0p5()
                            .child(div().w(px(3.)).h(px(12.)).rounded_full().bg(color))
                            .child(
                                Label::new(group.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .child(div().flex_1())
                            .when(is_folded && blocked > 0, |header| {
                                header.child(CountBadge::new(blocked))
                            })
                            .child(
                                Label::new(members.len().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Icon::new(if is_folded {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                    }
                })
                .tooltip(Tooltip::text(group.clone()))
                .on_click(cx.listener({
                    let group = group.clone();
                    move |this, _, _, cx| this.toggle_group(&group, cx)
                }));
            list = list.child(header);
            if !is_folded {
                for (index, layout) in members {
                    list =
                        list.child(self.render_layout(index, layout, &groups, scopes, narrow, cx));
                    list = list.children(self.render_terminals_of(layout, narrow, cx));
                }
            }
        }
        list.into_any_element()
    }

    fn render_machine_header(
        &self,
        key: &str,
        machine: Option<&str>,
        is_folded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name: SharedString = machine.unwrap_or("This machine").to_string().into();
        h_flex()
            .id(SharedString::from(key.to_string()))
            .role(Role::Button)
            .aria_label(format!("Machine {name}"))
            .aria_expanded(!is_folded)
            .w_full()
            .mt_1p5()
            .gap_1()
            .px_1p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Icon::new(if is_folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Icon::new(IconName::Server)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(name)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .on_click(cx.listener({
                let key = key.to_string();
                move |this, _, _, cx| this.toggle_node(&key, cx)
            }))
            .into_any_element()
    }

    fn render_terminals_of(
        &self,
        layout: &LayoutSummary,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if narrow || self.folded_nodes.contains(&layout_key(layout.id)) {
            return Vec::new();
        }
        self.render_layout_tree(layout.id, &layout.herdr, cx)
    }

    /// The Herdr tabs of a layout by workspace, then worktree, then agents
    /// and terminals. The terminals of the main worktree of a workspace sit
    /// right under it.
    fn render_layout_tree(
        &self,
        layout: LayoutId,
        entries: &[(Place, TerminalRow)],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut workspaces: BTreeMap<
            (Option<String>, String),
            BTreeMap<Option<String>, Vec<&TerminalRow>>,
        > = BTreeMap::new();
        for (place, row) in entries {
            workspaces
                .entry((place.machine.clone(), place.workspace.clone()))
                .or_default()
                .entry(place.worktree.clone())
                .or_default()
                .push(row);
        }
        let prefix = layout_key(layout);
        let mut elements = Vec::new();
        for ((machine, folder), worktrees) in workspaces {
            let key = format!("{prefix}:{}:{folder}", machine.as_deref().unwrap_or(""));
            let is_folded = self.folded_nodes.contains(&key);
            let rows = worktrees.values().flatten().copied().collect::<Vec<_>>();
            let name: SharedString = if folder.is_empty() {
                "Unknown place".into()
            } else {
                folder_name(&folder).into()
            };
            let tooltip: SharedString = match &machine {
                Some(machine) => format!("{machine} · {}", display_folder(&folder)).into(),
                None => display_folder(&folder).into(),
            };
            elements.push(self.render_node_header(
                &key,
                IconName::Folder,
                name,
                machine.clone().map(SharedString::from),
                tooltip,
                &rows,
                1,
                is_folded,
                cx,
            ));
            if is_folded {
                continue;
            }
            for (worktree, rows) in worktrees {
                match worktree {
                    None => self.render_kinds(&key, &rows, 2, &mut elements, cx),
                    Some(worktree) => {
                        let key = format!("{key}:{worktree}");
                        let is_folded = self.folded_nodes.contains(&key);
                        elements.push(self.render_node_header(
                            &key,
                            IconName::GitBranch,
                            worktree.clone().into(),
                            None,
                            format!("Worktree {worktree}").into(),
                            &rows,
                            2,
                            is_folded,
                            cx,
                        ));
                        if !is_folded {
                            self.render_kinds(&key, &rows, 3, &mut elements, cx);
                        }
                    }
                }
            }
        }
        elements
    }

    /// The agents, then the terminals, each under its own caption.
    fn render_kinds(
        &self,
        key: &str,
        rows: &[&TerminalRow],
        depth: usize,
        elements: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        for (is_agent, caption) in [(true, "Agents"), (false, "Terminals")] {
            let kind = rows
                .iter()
                .filter(|row| row.is_agent == is_agent)
                .collect::<Vec<_>>();
            if kind.is_empty() {
                continue;
            }
            elements.push(
                div()
                    .pl(indent(depth))
                    .pt_0p5()
                    .child(
                        Label::new(caption)
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .into_any_element(),
            );
            for row in kind {
                elements.push(self.render_terminal(key, row, depth, cx));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_node_header(
        &self,
        key: &str,
        icon: IconName,
        name: SharedString,
        machine: Option<SharedString>,
        tooltip: SharedString,
        rows: &[&TerminalRow],
        depth: usize,
        is_folded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let blocked = rows
            .iter()
            .filter(|row| row.is_live && row.state == Some(AgentState::Blocked))
            .count();
        let working = rows
            .iter()
            .filter(|row| row.is_live && row.state == Some(AgentState::Working))
            .count();
        h_flex()
            .id(SharedString::from(format!("herdr-node-{key}")))
            .role(Role::Button)
            .aria_label(format!("{name} · {}", rows.len()))
            .aria_expanded(!is_folded)
            .w_full()
            .gap_1()
            .pl(indent(depth))
            .pr_1p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Icon::new(if is_folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(Label::new(name).size(LabelSize::Small).truncate())
                    .when_some(machine, |line, machine| {
                        line.child(
                            Label::new(machine)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when(is_folded && blocked > 0, |header| {
                header.child(CountBadge::new(blocked))
            })
            .when(is_folded && blocked == 0 && working > 0, |header| {
                header.child(Indicator::dot().color(Color::Warning))
            })
            .when(is_folded, |header| {
                header.child(
                    Label::new(rows.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener({
                let key = key.to_string();
                move |this, _, _, cx| this.toggle_node(&key, cx)
            }))
            .into_any_element()
    }

    fn render_terminal(
        &self,
        key: &str,
        row: &TerminalRow,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let address = row.address.clone();
        let name = row.name.clone();
        let is_live = row.is_live;
        let element = h_flex()
            .id(SharedString::from(format!("herdr-{key}-{}", row.address)))
            .role(Role::Button)
            .aria_label({
                let mut label = vec![row.name.to_string()];
                if !row.detail.is_empty() {
                    label.push(row.detail.to_string());
                }
                if let Some(state) = row.state.filter(|_| row.is_live) {
                    label.push(state.label().to_string());
                }
                label.join(" · ")
            })
            .w_full()
            .gap_2()
            .pl(indent(depth))
            .pr_1p5()
            .py_0p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .child(
                h_flex()
                    .w(px(16.))
                    .flex_none()
                    .justify_center()
                    .map(|slot| match row.state {
                        Some(state) if row.is_live => {
                            slot.child(Indicator::dot().color(agent_color(state)))
                        }
                        _ => slot.child(
                            Icon::new(if row.is_agent {
                                IconName::ZedAgent
                            } else {
                                IconName::Terminal
                            })
                            .size(IconSize::XSmall)
                            .color(if row.is_live {
                                Color::Muted
                            } else {
                                Color::Disabled
                            }),
                        ),
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(row.name.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .when(!row.detail.is_empty(), |column| {
                        column.child(
                            Label::new(row.detail.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when_some(
                row.state
                    .filter(|state| row.is_live && *state != AgentState::Idle),
                |element, state| {
                    element.child(
                        Label::new(state.label())
                            .size(LabelSize::XSmall)
                            .color(agent_color(state)),
                    )
                },
            )
            .tooltip(Tooltip::text(row.tooltip.clone()))
            .on_click(cx.listener({
                let address = address.clone();
                move |this, _, window, cx| {
                    if is_live {
                        this.open_terminal(&address, window, cx);
                    }
                }
            }));

        let panel = cx.entity().downgrade();
        right_click_menu(SharedString::from(format!(
            "herdr-menu-{key}-{}",
            row.address
        )))
        .trigger(move |_, _, _| element)
        .menu(move |window, cx| {
            let panel = panel.clone();
            let address = address.clone();
            let name = name.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                if !is_live {
                    return menu.entry("Remove from Herdr Core", None, move |_, cx| {
                        panel
                            .update(cx, |this, cx| this.remove_terminal(&address, cx))
                            .log_err();
                    });
                }
                menu.entry("Open", None, {
                    let panel = panel.clone();
                    let address = address.clone();
                    move |window, cx| {
                        panel
                            .update(cx, |this, cx| this.open_terminal(&address, window, cx))
                            .log_err();
                    }
                })
                .entry("Open in a New Layout", None, {
                    let panel = panel.clone();
                    let address = address.clone();
                    let name = name.clone();
                    move |window, cx| {
                        panel
                            .update(cx, |this, cx| {
                                this.open_terminal_in_new_layout(&address, &name, window, cx);
                            })
                            .log_err();
                    }
                })
                .separator()
                .entry("End Program…", None, move |window, cx| {
                    panel
                        .update(cx, |this, cx| this.end_program(&address, &name, window, cx))
                        .log_err();
                })
            })
        })
        .into_any_element()
    }

    fn render_new_layout_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        IconButton::new("layouts-new", IconName::Plus)
            .aria_label("New Layout")
            .icon_size(IconSize::Small)
            .shape(IconButtonShape::Square)
            .tooltip(|_, cx| Tooltip::for_action("New Layout", &NewLayout, cx))
            .on_click(cx.listener(|this, _, window, cx| {
                this.update_workspace(window, cx, |workspace, window, cx| {
                    new_layout(workspace, window, cx);
                });
            }))
            .into_any_element()
    }

    fn render_new_terminal_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let scope = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_layout_scope().cloned());
        let targets = run_targets(scope.as_ref(), &self.project, &self.store, cx);
        let panel = cx.entity().downgrade();
        PopoverMenu::new("herdr-new-terminal")
            .trigger_with_tooltip(
                IconButton::new("herdr-new-terminal-trigger", IconName::Terminal)
                    .aria_label("New Terminal in Herdr Core")
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square),
                Tooltip::text("New Terminal in Herdr Core"),
            )
            .anchor(Anchor::TopRight)
            .menu(move |window, cx| {
                let targets = targets.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for target in &targets {
                        menu = menu
                            .header(target.label.clone())
                            .entry("Shell", None, {
                                let panel = panel.clone();
                                let target = target.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |this, cx| this.run(&target, None, window, cx))
                                        .log_err();
                                }
                            })
                            .entry("Command…", None, {
                                let panel = panel.clone();
                                let target = target.clone();
                                move |window, cx| {
                                    let target = target.clone();
                                    panel
                                        .update(cx, |this, cx| {
                                            this.start_edit(
                                                EditTarget::Command(target),
                                                "",
                                                "pi, cargo test, …",
                                                window,
                                                cx,
                                            );
                                        })
                                        .log_err();
                                }
                            });
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    /// The field of an edit that has no row of its own, with what it is for.
    fn render_edit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editing = self.editing.as_ref()?;
        let caption: SharedString = match &editing.target {
            EditTarget::Rename(_) => return None,
            EditTarget::Group(_) => "New group for the layout".into(),
            EditTarget::Command(target) => format!("Run in {}", target.label).into(),
        };
        Some(
            v_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(caption)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(
                    div()
                        .w_full()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(editing.editor.clone()),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let (message, color, can_retry): (SharedString, Color, bool) =
            match self.store.read(cx).state() {
                ConnectionState::Connecting => {
                    ("Connecting to Herdr Core…".into(), Color::Muted, false)
                }
                ConnectionState::Connected { core, version } => (
                    format!("{} · hc {version}", core.name).into(),
                    Color::Muted,
                    false,
                ),
                ConnectionState::Disconnected { error } => (error.clone(), Color::Warning, true),
            };
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(self.error.clone(), |footer, error| {
                footer.child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            ),
                        )
                        .child(
                            IconButton::new("layouts-dismiss-error", IconName::Close)
                                .aria_label("Dismiss")
                                .icon_size(IconSize::XSmall)
                                .shape(IconButtonShape::Square)
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .id("herdr-connection")
                    .role(Role::Status)
                    .aria_label(message.clone())
                    .w_full()
                    .h(px(26.))
                    .px_2()
                    .gap_1p5()
                    .child(Indicator::dot().color(if can_retry {
                        Color::Warning
                    } else if self.store.read(cx).is_connected() {
                        Color::Success
                    } else {
                        Color::Muted
                    }))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(message.clone())
                                .size(LabelSize::XSmall)
                                .color(color)
                                .truncate(),
                        ),
                    )
                    .tooltip(Tooltip::text(message))
                    .when(can_retry, |footer| {
                        footer.child(
                            Button::new("herdr-reconnect", "Retry")
                                .label_size(LabelSize::XSmall)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store.update(cx, |store, cx| store.reconnect(cx));
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The narrow form of what the core runs: who needs a person, and how
    /// many, without names.
    fn render_narrow_inventory(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let count = |state| {
            store
                .terminals()
                .into_iter()
                .filter(|record| record.is_live() && record.agent_state() == Some(state))
                .count()
        };
        let blocked = count(AgentState::Blocked);
        let working = count(AgentState::Working);
        let tooltip = format!(
            "{} · {}",
            if blocked == 1 {
                "1 agent needs you".to_string()
            } else {
                format!("{blocked} agents need you")
            },
            if working == 1 {
                "1 works".to_string()
            } else {
                format!("{working} work")
            }
        );
        v_flex()
            .id("herdr-narrow")
            .role(Role::Button)
            .aria_label(tooltip.clone())
            .w_full()
            .py_1p5()
            .gap_1()
            .items_center()
            .cursor_pointer()
            .child(
                Icon::new(if blocked > 0 {
                    IconName::BellDot
                } else {
                    IconName::Bell
                })
                .size(IconSize::Small)
                .color(if blocked > 0 {
                    Color::Error
                } else {
                    Color::Muted
                }),
            )
            .when(blocked > 0, |column| {
                column.child(
                    Label::new(blocked.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .when(working > 0, |column| {
                column.child(Indicator::dot().color(Color::Warning))
            })
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(|this, _, window, cx| this.set_narrow(false, window, cx)))
            .into_any_element()
    }
}

fn layout_key(layout: LayoutId) -> String {
    format!("{layout:?}")
}

fn terminal_row(record: &TerminalRecord, is_live: bool) -> TerminalRow {
    let name = record.display_name();
    let detail = if record.program() != name {
        record.program().to_string()
    } else {
        String::new()
    };
    let mut tooltip = vec![
        format!("{} · {}", record.address, record.program()),
        record.current_folder().to_string(),
    ];
    if let Some(message) = record
        .status
        .as_ref()
        .and_then(|status| status.msg.as_deref())
        .filter(|message| !message.is_empty())
    {
        tooltip.push(message.to_string());
    }
    TerminalRow {
        address: record.address.clone().into(),
        name: name.into(),
        detail: detail.into(),
        tooltip: tooltip.join("\n").into(),
        state: record.agent_state(),
        is_agent: is_agent(record),
        is_live: is_live && record.is_live(),
    }
}

/// The Herdr machine of a project: `None` for a local project, the machine
/// named as the SSH host of a remote one. A remote project that no machine
/// of the core reaches has no place in Herdr.
fn project_machine(project: &Project, store: &HerdrStore, cx: &App) -> Option<Option<String>> {
    match project.remote_connection_options(cx) {
        Some(options) => remote_machine(&options, store),
        None => Some(None),
    }
}

/// The Herdr machine that an SSH connection reaches, by its host name.
fn remote_machine(options: &RemoteConnectionOptions, store: &HerdrStore) -> Option<Option<String>> {
    let RemoteConnectionOptions::Ssh(options) = options else {
        return None;
    };
    let host = options.host.to_string();
    store
        .machines()
        .iter()
        .find(|machine| {
            machine.name.eq_ignore_ascii_case(&host) || machine.host.eq_ignore_ascii_case(&host)
        })
        .map(|machine| Some(machine.name.clone()))
}

/// The folders of the project of a window, with their Herdr machine.
fn project_roots(
    project: &Entity<Project>,
    store: &Entity<HerdrStore>,
    cx: &App,
) -> Vec<(Option<String>, String)> {
    let project = project.read(cx);
    let Some(machine) = project_machine(project, store.read(cx), cx) else {
        return Vec::new();
    };
    project
        .visible_worktrees(cx)
        .map(|worktree| {
            (
                machine.clone(),
                worktree.read(cx).abs_path().to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Where a terminal of the window starts unless the person says otherwise:
/// in the first folder of its project, on the machine of the project.
fn default_target(project: &Entity<Project>, store: &Entity<HerdrStore>, cx: &App) -> RunTarget {
    match project_roots(project, store, cx).into_iter().next() {
        Some((machine, folder)) => RunTarget {
            label: place_label(machine.as_deref(), Some(&folder)),
            machine,
            folder: Some(folder),
        },
        None => RunTarget {
            machine: None,
            folder: None,
            label: place_label(None, None),
        },
    }
}

fn place_label(machine: Option<&str>, folder: Option<&String>) -> SharedString {
    let machine = machine.unwrap_or("This machine");
    match folder {
        Some(folder) => format!("{machine} · {}", short_folder(folder)).into(),
        None => format!("{machine} · home folder").into(),
    }
}

/// Every place the panel offers to start a terminal: the place of the
/// project first, then the home folder of each machine.
fn run_targets(
    scope: Option<&LayoutScope>,
    project: &Entity<Project>,
    store: &Entity<HerdrStore>,
    cx: &App,
) -> Vec<RunTarget> {
    let default = default_target(project, store, cx);
    let mut targets = Vec::new();
    if let Some(scope) = scope
        && (scope.machine != default.machine || Some(&scope.folder) != default.folder.as_ref())
    {
        targets.push(scope_target(scope));
    }
    targets.push(default.clone());
    let machines = std::iter::once(None).chain(
        store
            .read(cx)
            .machines()
            .iter()
            .filter(|machine| machine.is_connected())
            .map(|machine| Some(machine.name.clone())),
    );
    for machine in machines {
        if default.folder.is_some() || default.machine != machine {
            targets.push(RunTarget {
                label: place_label(machine.as_deref(), None),
                machine,
                folder: None,
            });
        }
    }
    targets
}

/// Starts a shell, or `line` in a shell, where `target` says, and opens its
/// tab in the layout on screen.
fn run_in_herdr(
    workspace: WeakEntity<Workspace>,
    target: &RunTarget,
    line: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let store = HerdrStore::global(cx);
    let run = store.read(cx).run(
        RunParams {
            line,
            cwd: target.folder.clone(),
            on: target.machine.clone(),
            no_parent: true,
            no_notify: true,
            ..RunParams::default()
        },
        cx,
    );
    window.spawn(cx, async move |cx| {
        let record = run.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            open_herdr_terminal(workspace, &record.address, window, cx);
        })
    })
}

fn new_herdr_terminal(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = HerdrStore::global(cx);
    let target = match workspace.active_layout_scope() {
        Some(scope) => scope_target(scope),
        None => default_target(workspace.project(), &store, cx),
    };
    let weak_workspace = cx.entity().downgrade();
    run_in_herdr(weak_workspace.clone(), &target, None, window, cx).detach_and_notify_err(
        weak_workspace,
        window,
        cx,
    );
}

fn open_in_new_layout(
    workspace: &mut Workspace,
    address: &str,
    name: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let layout = new_layout(workspace, window, cx);
    workspace.rename_layout(layout, name.to_string(), window, cx);
    let weak_workspace = cx.entity().downgrade();
    let workspace_id = workspace.database_id();
    let project = workspace.project().downgrade();
    let address = SharedString::from(address.to_string());
    let item =
        cx.new(|cx| HerdrTerminal::new(address, weak_workspace, workspace_id, project, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// Ends a program once the person confirms it.
fn end_program(
    store: &Entity<HerdrStore>,
    address: &str,
    name: &str,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("End the program of “{name}”?"),
        Some("Herdr Core hangs up on it, then terminates it. What it did not save is lost."),
        &["End Program", "Cancel"],
        cx,
    );
    let store = store.clone();
    let address = address.to_string();
    cx.spawn(async move |cx| {
        if answer.await != Ok(0) {
            return Ok(());
        }
        store
            .read_with(cx, |store, cx| store.kill(&address, cx))
            .await
    })
}

/// Opens a table of the programs of Herdr Core in the pane on screen,
/// limited to the scope of the layout on screen.
fn open_processes(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let scope = workspace.active_layout_scope().cloned();
    add_process_table(workspace, scope, window, cx);
}

fn add_process_table(
    workspace: &mut Workspace,
    scope: Option<LayoutScope>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak_workspace = cx.entity().downgrade();
    let workspace_id = workspace.database_id();
    let view = ProcessView::scoped(scope);
    let item = cx.new(|cx| HerdrProcesses::new(weak_workspace, workspace_id, view, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

fn placed_key(workspace_id: workspace::WorkspaceId) -> String {
    format!("{PANEL_KEY}-placed-{}", i64::from(workspace_id))
}

/// The addresses of the Herdr tabs of every layout of a window.
fn shown_herdr_addresses(workspace: &Workspace, cx: &App) -> HashSet<SharedString> {
    workspace
        .layouts()
        .iter()
        .flat_map(|layout| workspace.layout_panes(layout.id()))
        .flat_map(|pane| {
            pane.read(cx)
                .items()
                .filter_map(|item| item.downcast::<HerdrTerminal>())
                .map(|terminal| terminal.read(cx).address().clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The scope of the layout for a place: its worktree, or its workspace.
fn place_scope(place: &Place) -> LayoutScope {
    LayoutScope {
        machine: place.machine.clone(),
        folder: match &place.worktree {
            Some(worktree) => worktree_folder(&place.workspace, worktree),
            None => place.workspace.clone(),
        },
    }
}

/// Opens a tab for each program in the layout of its place, without
/// showing that layout. A workspace seen for the first time gets a group
/// named after it, with an overview of its programs; each worktree gets a
/// layout, `main` for the main one.
fn place_programs(
    workspace: &mut Workspace,
    programs: Vec<(String, Place)>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    for (address, place) in programs {
        let group = folder_name(&place.workspace);
        let workspace_scope = LayoutScope {
            machine: place.machine.clone(),
            folder: place.workspace.clone(),
        };
        let has_overview = workspace
            .layouts()
            .iter()
            .any(|layout| layout.scope() == Some(&workspace_scope) && **layout.name() == *OVERVIEW);
        if !has_overview {
            let overview = workspace.add_hidden_layout(
                OVERVIEW,
                group.clone(),
                Some(workspace_scope.clone()),
                window,
                cx,
            );
            if let Some(pane) = workspace.layout_active_pane(overview) {
                let weak_workspace = cx.entity().downgrade();
                let workspace_id = workspace.database_id();
                let view = ProcessView::scoped(Some(workspace_scope));
                let item = cx
                    .new(|cx| HerdrProcesses::new(weak_workspace, workspace_id, view, window, cx));
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(item), false, false, None, window, cx);
                });
            }
        }
        let scope = place_scope(&place);
        let existing = workspace
            .layouts()
            .iter()
            .find(|layout| layout.scope() == Some(&scope) && **layout.name() != *OVERVIEW)
            .map(|layout| layout.id());
        let layout = existing.unwrap_or_else(|| {
            let name = place.worktree.clone().unwrap_or_else(|| "main".to_string());
            workspace.add_hidden_layout(name, group.clone(), Some(scope), window, cx)
        });
        let Some(pane) = workspace.layout_active_pane(layout) else {
            continue;
        };
        let weak_workspace = cx.entity().downgrade();
        let workspace_id = workspace.database_id();
        let project = workspace.project().downgrade();
        let address = SharedString::from(address);
        let item = cx.new(|cx| {
            HerdrTerminal::new(address, weak_workspace, workspace_id, project, window, cx)
        });
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), false, false, None, window, cx);
        });
    }
}

/// The folder of a worktree kept under `.worktrees/` of its workspace.
fn worktree_folder(workspace: &str, worktree: &str) -> String {
    let separator = if workspace.contains('\\') && !workspace.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!("{workspace}{separator}.worktrees{separator}{worktree}")
}

fn scope_label(scope: &LayoutScope) -> String {
    match &scope.machine {
        Some(machine) => format!("{} · {machine}", display_folder(&scope.folder)),
        None => display_folder(&scope.folder),
    }
}

fn scope_target(scope: &LayoutScope) -> RunTarget {
    RunTarget {
        label: place_label(scope.machine.as_deref(), Some(&scope.folder)),
        machine: scope.machine.clone(),
        folder: Some(scope.folder.clone()),
    }
}

/// The places a layout can take as its scope: the folders of the project,
/// then every workspace and worktree where Herdr Core runs programs.
fn scope_choices(
    roots: &[(Option<String>, String)],
    store: &Entity<HerdrStore>,
    cx: &App,
) -> Vec<LayoutScope> {
    let mut choices = roots
        .iter()
        .map(|(machine, folder)| LayoutScope {
            machine: machine.clone(),
            folder: folder.clone(),
        })
        .collect::<Vec<_>>();
    let mut running = BTreeMap::new();
    for record in store.read(cx).terminals() {
        if !record.is_live() {
            continue;
        }
        let place = place_of(record.machine(), record.current_folder(), roots);
        let folder = match &place.worktree {
            Some(worktree) => worktree_folder(&place.workspace, worktree),
            None => place.workspace.clone(),
        };
        running.insert(
            (place.machine.clone(), folder.clone()),
            LayoutScope {
                machine: place.machine,
                folder,
            },
        );
    }
    for scope in running.into_values() {
        if !choices.contains(&scope) {
            choices.push(scope);
        }
    }
    choices
}

fn is_within(folder: &str, root: &str) -> bool {
    let root = root.trim_end_matches(['/', '\\']);
    match folder.strip_prefix(root) {
        Some(rest) => rest.is_empty() || rest.starts_with(['/', '\\']),
        None => false,
    }
}

/// The space before a line of the panel at `depth` in its tree.
fn indent(depth: usize) -> Pixels {
    px(6. + 12. * depth as f32)
}

/// Where a terminal works: a workspace, the folder with a Git repository at
/// its root, and its worktree when not the main one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Place {
    /// `None` for the machine of the core this window talks to.
    pub(crate) machine: Option<String>,
    pub(crate) workspace: String,
    pub(crate) worktree: Option<String>,
}

/// Herdr Core does not say where the repositories are. A folder under
/// `.worktrees/` is a worktree of the folder above it; any other folder
/// belongs to the deepest of `roots` on its machine that holds it, or is its
/// own workspace.
pub(crate) fn place_of(
    machine: Option<&str>,
    folder: &str,
    roots: &[(Option<String>, String)],
) -> Place {
    let machine = machine.map(str::to_string);
    let folder = folder.trim_end_matches(['/', '\\']);
    for separator in ["/.worktrees/", "\\.worktrees\\"] {
        if let Some((workspace, rest)) = folder.split_once(separator) {
            let worktree = rest.split(['/', '\\']).next().unwrap_or(rest);
            return Place {
                machine,
                workspace: workspace.to_string(),
                worktree: Some(worktree.to_string()),
            };
        }
    }
    let workspace = roots
        .iter()
        .filter(|(root_machine, root)| {
            *root_machine == machine && is_within(folder, root) && !is_home_folder(root)
        })
        .map(|(_, root)| root.trim_end_matches(['/', '\\']))
        .max_by_key(|root| root.len())
        .unwrap_or(folder)
        .to_string();
    Place {
        machine,
        workspace,
        worktree: None,
    }
}

fn is_home_folder(folder: &str) -> bool {
    folder_components(folder) == ["~"]
}

/// The name a folder goes by: its last name, or `~` for a home folder.
pub(crate) fn folder_name(folder: &str) -> String {
    folder_components(folder)
        .last()
        .cloned()
        .unwrap_or_else(|| folder.to_string())
}

/// A folder as a person reads it: `~/…` for what is in a home folder.
pub(crate) fn display_folder(folder: &str) -> String {
    let components = folder_components(folder);
    match components.split_first() {
        Some((first, rest)) if first == "/" => format!("/{}", rest.join("/")),
        Some(_) => components.join("/"),
        None => folder.to_string(),
    }
}

/// The names of a folder from the root of its machine. A home folder shows
/// as `~`, on any machine: its owner is the person who reads the list.
fn folder_components(folder: &str) -> Vec<String> {
    let trimmed = folder.trim_end_matches(['/', '\\']);
    let names = trimmed
        .split(['/', '\\'])
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let is_unix = trimmed.starts_with('/');
    let is_windows = trimmed.as_bytes().get(1) == Some(&b':');
    let home = |rest: &[&str]| {
        std::iter::once("~".to_string())
            .chain(rest.iter().map(|name| name.to_string()))
            .collect()
    };
    match names.as_slice() {
        ["home" | "Users", _, rest @ ..] if is_unix => home(rest),
        [_, users, _, rest @ ..] if is_windows && users.eq_ignore_ascii_case("users") => home(rest),
        _ if is_unix => std::iter::once("/".to_string())
            .chain(names.iter().map(|name| name.to_string()))
            .collect(),
        _ => names.iter().map(|name| name.to_string()).collect(),
    }
}

/// The last two names of a folder: enough to tell worktrees apart.
fn short_folder(folder: &str) -> String {
    let trimmed = folder.trim_end_matches(['/', '\\']);
    let mut names = trimmed.rsplit(['/', '\\']).filter(|name| !name.is_empty());
    match (names.next(), names.next()) {
        (Some(last), Some(parent)) => format!("{parent}/{last}"),
        (Some(last), None) => last.to_string(),
        _ => folder.to_string(),
    }
}

impl Focusable for LayoutsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for LayoutsPanel {}

impl Render for LayoutsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(workspace) = self.workspace.upgrade() else {
            return div().into_any_element();
        };
        let roots = project_roots(&self.project, &self.store, cx);
        let scopes = scope_choices(&roots, &self.store, cx);
        let (narrow, layouts, current_id) = {
            let workspace = workspace.read(cx);
            (
                self.width(workspace, cx) < NARROW_LIMIT,
                self.layout_summaries(workspace, &roots, cx),
                workspace.database_id(),
            )
        };

        v_flex()
            .id("layouts-panel")
            .key_context("LayoutsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm_edit))
            .on_action(cx.listener(Self::cancel_edit))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(narrow, cx))
            .child(
                v_flex()
                    .id("layouts-panel-content")
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .pb_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(self.render_layouts(&layouts, &scopes, narrow, cx))
                    .when(!narrow, |content| {
                        content.children(self.render_saved_elsewhere(current_id, cx))
                    })
                    .when(narrow, |content| {
                        content
                            .child(div().mx_2().h_px().bg(cx.theme().colors().border_variant))
                            .child(self.render_narrow_inventory(cx))
                    }),
            )
            .when(!narrow, |panel| {
                panel
                    .children(self.render_edit_bar(cx))
                    .child(self.render_footer(cx))
            })
            .into_any_element()
    }
}

impl Panel for LayoutsPanel {
    fn persistent_name() -> &'static str {
        PANEL_KEY
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        DEFAULT_WIDTH
    }

    fn min_size(&self, _: &Window, _: &App) -> Option<Pixels> {
        Some(NARROW_WIDTH)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::SplitAlt)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Layouts Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    /// The agents that wait for a person, on the button of the panel.
    fn icon_label(&self, _: &Window, cx: &App) -> Option<String> {
        let blocked = self
            .store
            .read(cx)
            .terminals()
            .into_iter()
            .filter(|record| record.is_live() && record.agent_state() == Some(AgentState::Blocked))
            .count();
        (blocked > 0).then(|| blocked.to_string())
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

#[derive(Default)]
enum PreviewNode {
    #[default]
    Empty,
    Pane {
        label: SharedString,
        icon: IconName,
        color: Color,
        is_terminal: bool,
        tabs: usize,
    },
    Split {
        axis: Axis,
        children: Vec<(f32, PreviewNode)>,
    },
}

/// What a layout looks like and holds, to tell it from the others before
/// showing it.
#[derive(Default)]
struct LayoutPreview {
    name: SharedString,
    group: SharedString,
    root: PreviewNode,
    agents: Vec<(SharedString, AgentState)>,
    others: Vec<PreviewTab>,
}

struct PreviewTab {
    name: SharedString,
    icon: IconName,
    /// A tab on a terminal that Herdr Core ended or no longer has.
    ended: bool,
}

impl LayoutPreview {
    fn build(
        workspace: &Workspace,
        id: LayoutId,
        store: &Entity<HerdrStore>,
        cx: &App,
    ) -> Option<Self> {
        let layout = workspace
            .layouts()
            .iter()
            .find(|layout| layout.id() == id)?;
        let store = store.read(cx);
        let mut preview = Self {
            name: layout.name().clone(),
            group: layout.group().clone(),
            root: PreviewNode::Empty,
            agents: Vec::new(),
            others: Vec::new(),
        };

        fn node(outline: &LayoutOutline, cx: &App) -> PreviewNode {
            match outline {
                LayoutOutline::Split { axis, children } => PreviewNode::Split {
                    axis: *axis,
                    children: children
                        .iter()
                        .map(|(flex, child)| (*flex, node(child, cx)))
                        .collect(),
                },
                LayoutOutline::Pane(pane) => {
                    let pane = pane.read(cx);
                    let Some(item) = pane.active_item() else {
                        return PreviewNode::Empty;
                    };
                    let terminal = item.downcast::<HerdrTerminal>();
                    let (icon, color) = match &terminal {
                        Some(terminal) => terminal_icon(terminal.read(cx).record(cx)),
                        None => (IconName::File, Color::Muted),
                    };
                    PreviewNode::Pane {
                        label: item.tab_content_text(0, cx),
                        icon,
                        color,
                        is_terminal: terminal.is_some(),
                        tabs: pane.items_len(),
                    }
                }
            }
        }
        preview.root = node(&workspace.layout_outline(id)?, cx);

        for pane in workspace.layout_panes(id) {
            for item in pane.read(cx).items() {
                let label = item.tab_content_text(0, cx);
                let Some(terminal) = item.downcast::<HerdrTerminal>() else {
                    preview.others.push(PreviewTab {
                        name: label,
                        icon: IconName::File,
                        ended: false,
                    });
                    continue;
                };
                let terminal = terminal.read(cx);
                let ended = terminal.is_ended(cx);
                match store.terminal(terminal.address()) {
                    Some(record) if !ended && is_agent(record) => preview
                        .agents
                        .push((label, record.agent_state().unwrap_or(AgentState::Idle))),
                    _ => preview.others.push(PreviewTab {
                        name: label,
                        icon: IconName::Terminal,
                        ended,
                    }),
                }
            }
        }
        preview.agents.sort_by_key(|(_, state)| *state);
        Some(preview)
    }

    fn render_node(node: &PreviewNode, cx: &App) -> AnyElement {
        let colors = cx.theme().colors();
        match node {
            PreviewNode::Empty => div()
                .size_full()
                .rounded_sm()
                .border_1()
                .border_dashed()
                .border_color(colors.border_variant)
                .into_any_element(),
            PreviewNode::Pane {
                label,
                icon,
                color,
                is_terminal,
                tabs,
            } => v_flex()
                .size_full()
                .p_1()
                .overflow_hidden()
                .rounded_sm()
                .border_1()
                .border_color(colors.border_variant)
                .bg(if *is_terminal {
                    colors.terminal_background
                } else {
                    colors.editor_background
                })
                .child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(*icon).size(IconSize::XSmall).color(*color))
                        .child(Label::new(label.clone()).size(LabelSize::XSmall).truncate()),
                )
                .when(*tabs > 1, |pane| {
                    pane.child(
                        Label::new(format!("+{}", tabs - 1))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
            PreviewNode::Split { axis, children } => {
                let container = match axis {
                    Axis::Horizontal => h_flex(),
                    Axis::Vertical => v_flex(),
                };
                container
                    .size_full()
                    .items_stretch()
                    .gap_0p5()
                    .children(children.iter().map(|(flex, child)| {
                        let mut part = div().min_w_0().min_h_0().flex_basis(px(0.));
                        part.style().flex_grow = Some(flex.max(0.05));
                        part.child(Self::render_node(child, cx))
                    }))
                    .into_any_element()
            }
        }
    }
}

impl Render for LayoutPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden_others = self.others.len().saturating_sub(PREVIEW_ROWS);
        let miniature = Self::render_node(&self.root, cx);
        tooltip_container(cx, move |container, _| {
            container
                .w(PREVIEW_WIDTH)
                .py_2()
                .gap_2()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Label::new(self.name.clone()).truncate())
                        .child(div().flex_1())
                        .when(!self.group.is_empty(), |header| {
                            header.child(
                                Label::new(self.group.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .child(div().w_full().h(PREVIEW_HEIGHT).child(miniature))
                .when(!self.agents.is_empty(), |container| {
                    container.child(v_flex().gap_0p5().children(self.agents.iter().map(
                        |(name, state)| {
                            h_flex()
                                .gap_1p5()
                                .child(Indicator::dot().color(agent_color(*state)))
                                .child(div().flex_1().min_w_0().child(
                                    Label::new(name.clone()).size(LabelSize::Small).truncate(),
                                ))
                                .child(
                                    Label::new(state.label())
                                        .size(LabelSize::XSmall)
                                        .color(agent_color(*state)),
                                )
                        },
                    )))
                })
                .when(!self.others.is_empty(), |container| {
                    container.child(
                        v_flex()
                            .gap_0p5()
                            .children(self.others.iter().take(PREVIEW_ROWS).map(|tab| {
                                let color = if tab.ended {
                                    Color::Disabled
                                } else {
                                    Color::Muted
                                };
                                h_flex()
                                    .gap_1p5()
                                    .child(Icon::new(tab.icon).size(IconSize::XSmall).color(color))
                                    .child(
                                        Label::new(tab.name.clone())
                                            .size(LabelSize::Small)
                                            .color(color)
                                            .truncate(),
                                    )
                                    .when(tab.ended, |row| {
                                        row.child(
                                            Label::new("ended")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Disabled),
                                        )
                                    })
                            }))
                            .when(hidden_others > 0, move |list| {
                                list.child(
                                    Label::new(format!("and {hidden_others} more"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_folder_holds_no_workspace() {
        let roots = [(Some("dgx".to_string()), "/home/person".to_string())];
        let place = place_of(Some("dgx"), "/home/person/projects/tea", &roots);
        assert_eq!(place.workspace, "/home/person/projects/tea");
    }

    #[gpui::test]
    async fn test_programs_are_placed_by_workspace_and_worktree(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            super::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/project"), serde_json::json!({}))
            .await;
        let project = Project::test(fs, [util::path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let place = |worktree: Option<&str>| Place {
            machine: Some("dgx".to_string()),
            workspace: "/home/person/tea".to_string(),
            worktree: worktree.map(str::to_string),
        };

        workspace.update_in(cx, |workspace, window, cx| {
            place_programs(
                workspace,
                vec![
                    ("dgx/t1".to_string(), place(None)),
                    ("dgx/t2".to_string(), place(Some("menu"))),
                    ("dgx/t3".to_string(), place(None)),
                ],
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let layouts = workspace
                .layouts()
                .iter()
                .map(|layout| {
                    let tabs = workspace
                        .layout_panes(layout.id())
                        .iter()
                        .map(|pane| pane.read(cx).items_len())
                        .sum::<usize>();
                    (
                        layout.name().to_string(),
                        layout.group().to_string(),
                        layout.scope().map(|scope| scope.folder.clone()),
                        tabs,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                layouts,
                vec![
                    ("Main".to_string(), String::new(), None, 0),
                    (
                        "Overview".to_string(),
                        "tea".to_string(),
                        Some("/home/person/tea".to_string()),
                        1
                    ),
                    (
                        "main".to_string(),
                        "tea".to_string(),
                        Some("/home/person/tea".to_string()),
                        2
                    ),
                    (
                        "menu".to_string(),
                        "tea".to_string(),
                        Some("/home/person/tea/.worktrees/menu".to_string()),
                        1
                    ),
                ]
            );
            assert_eq!(workspace.active_layout(), workspace.layouts()[0].id());
        });
    }

    #[gpui::test]
    async fn test_a_workspace_opens_a_table_while_it_is_updated(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            super::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/project"), serde_json::json!({}))
            .await;
        let project = Project::test(fs, [util::path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            open_processes(workspace, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_item_as::<HerdrProcesses>(cx).is_some());
        });
    }

    #[test]
    fn a_terminal_runs_in_a_project_by_its_folder() {
        assert!(is_within("/home/ada/acme", "/home/ada/acme"));
        assert!(is_within(
            "/home/ada/acme/.worktrees/fix",
            "/home/ada/acme/"
        ));
        assert!(!is_within("/home/ada/acme-web", "/home/ada/acme"));
        assert!(!is_within("/home/ada", "/home/ada/acme"));
        assert!(is_within("C:\\work\\acme\\src", "C:\\work\\acme"));
    }

    #[test]
    fn a_home_folder_shows_as_a_tilde_on_any_machine() {
        assert_eq!(folder_components("/home/ada/acme/"), ["~", "acme"]);
        assert_eq!(folder_components("/Users/ada"), ["~"]);
        assert_eq!(folder_components("C:\\Users\\Ada\\acme"), ["~", "acme"]);
        assert_eq!(folder_components("/opt/tools"), ["/", "opt", "tools"]);
        assert!(folder_components("").is_empty());
    }

    #[test]
    fn a_terminal_works_in_the_worktree_of_its_folder() {
        let roots = [(None, "/home/ada/acme".to_string())];
        let place = place_of(None, "/home/ada/acme/.worktrees/fix/src", &roots);
        assert_eq!(place.workspace, "/home/ada/acme");
        assert_eq!(place.worktree.as_deref(), Some("fix"));

        let place = place_of(None, "/home/ada/acme/crates/core", &roots);
        assert_eq!(place.workspace, "/home/ada/acme");
        assert_eq!(place.worktree, None);

        // The folder of the project on this machine says nothing of a
        // folder of the same name on another machine.
        let place = place_of(Some("dgx"), "/home/ada/acme/crates", &roots);
        assert_eq!(place.workspace, "/home/ada/acme/crates");

        let place = place_of(None, "C:\\work\\acme\\.worktrees\\fix", &[]);
        assert_eq!(place.workspace, "C:\\work\\acme");
        assert_eq!(place.worktree.as_deref(), Some("fix"));
    }

    #[test]
    fn a_folder_reads_from_the_home_folder() {
        assert_eq!(display_folder("/home/ada/acme/"), "~/acme");
        assert_eq!(display_folder("/opt/tools"), "/opt/tools");
        assert_eq!(folder_name("/home/ada"), "~");
        assert_eq!(folder_name("C:\\Users\\Ada\\acme"), "acme");
    }

    #[test]
    fn a_folder_is_shortened_to_its_last_two_names() {
        assert_eq!(
            short_folder("/home/ada/acme/.worktrees/fix/"),
            ".worktrees/fix"
        );
        assert_eq!(short_folder("/acme"), "acme");
        assert_eq!(short_folder("C:\\work\\acme"), "work/acme");
        assert_eq!(short_folder("/"), "/");
    }
}
