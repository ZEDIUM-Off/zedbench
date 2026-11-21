//! The layouts of a window as vertical tabs, and under them what Herdr Core
//! keeps alive: agents and terminals, on this machine and on the others.
//! A layout arranges views; the core owns the programs. Nothing here starts
//! or ends a program unless the person asks for exactly that.

mod herdr_terminal;

use anyhow::Result;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, actions::SelectAll};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, AsyncWindowContext, Axis, ClickEvent,
    Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, PromptLevel, Role, ScrollHandle,
    SharedString, Subscription, WeakEntity, Window, actions, px,
};
use herdr::{AgentState, ConnectionState, HerdrStore, RunParams, TerminalRecord};
use project::Project;
use serde::{Deserialize, Serialize};
use theme::ActiveTheme as _;
use ui::{
    ContextMenu, CountBadge, IconButtonShape, Indicator, PopoverMenu, Tooltip, prelude::*,
    right_click_menu, tooltip_container,
};
use util::ResultExt as _;
use workspace::{
    LayoutId, LayoutOutline, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
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
const DEFAULT_WIDTH: Pixels = px(248.);
const NARROW_WIDTH: Pixels = px(44.);
/// Under this width the panel shows icons only.
const NARROW_LIMIT: Pixels = px(120.);
const PREVIEW_WIDTH: Pixels = px(300.);
const PREVIEW_HEIGHT: Pixels = px(108.);
const PREVIEW_ROWS: usize = 6;

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<HerdrTerminal>(cx);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    Agents,
    Terminals,
    Ended,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Agents => "Agents",
            Section::Terminals => "Terminals",
            Section::Ended => "Ended",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Section::Agents => "agents",
            Section::Terminals => "terminals",
            Section::Ended => "ended",
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedLayoutsPanel {
    #[serde(default)]
    folded_groups: Vec<String>,
    #[serde(default)]
    folded_sections: Vec<String>,
    #[serde(default)]
    project_only: bool,
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
    active: bool,
    tabs: usize,
    agents: usize,
    terminals: usize,
    /// Tabs on a terminal that Herdr Core ended or no longer has.
    ended: usize,
    blocked: usize,
    working: usize,
}

impl LayoutSummary {
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
        if parts.is_empty() {
            "Empty".to_string()
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
    folded_sections: HashSet<Section>,
    /// Lists only what runs in a folder of the project of the window.
    project_only: bool,
    /// The width to give back when the panel leaves its narrow form.
    wide_width: Pixels,
    editing: Option<Editing>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl LayoutsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
        let serialized = cx
            .background_spawn(async move { kvp.read_kvp(PANEL_KEY) })
            .await
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedLayoutsPanel>(&json).log_err())
            .unwrap_or_default();
        workspace.update_in(&mut cx, |workspace, _, cx| {
            let workspace_entity = cx.entity();
            let project = workspace.project().clone();
            cx.new(|cx| Self::new(workspace_entity, project, serialized, cx))
        })
    }

    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        serialized: SerializedLayoutsPanel,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = HerdrStore::global(cx);
        let folded_sections = [Section::Agents, Section::Terminals, Section::Ended]
            .into_iter()
            .filter(|section| {
                serialized
                    .folded_sections
                    .iter()
                    .any(|key| key == section.key())
            })
            .collect();
        Self {
            _subscriptions: vec![
                cx.observe(&store, |_, _, cx| cx.notify()),
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
            folded_sections,
            project_only: serialized.project_only,
            wide_width: serialized.wide_width.map_or(DEFAULT_WIDTH, px),
            editing: None,
            error: None,
        }
    }

    fn serialize(&self, cx: &mut Context<Self>) {
        let serialized = SerializedLayoutsPanel {
            folded_groups: self
                .folded_groups
                .iter()
                .map(|group| group.to_string())
                .collect(),
            folded_sections: self
                .folded_sections
                .iter()
                .map(|section| section.key().to_string())
                .collect(),
            project_only: self.project_only,
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

    fn layout_summaries(&self, workspace: &Workspace, cx: &App) -> Vec<LayoutSummary> {
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
                    active: layout.id() == active,
                    tabs: 0,
                    agents: 0,
                    terminals: 0,
                    ended: 0,
                    blocked: 0,
                    working: 0,
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

    /// For each terminal with a tab, the name of the layout that shows it.
    fn layouts_by_address(
        &self,
        workspace: &Workspace,
        cx: &App,
    ) -> HashMap<SharedString, SharedString> {
        let mut layouts = HashMap::default();
        for layout in workspace.layouts() {
            for pane in workspace.layout_panes(layout.id()) {
                for item in pane.read(cx).items() {
                    if let Some(terminal) = item.downcast::<HerdrTerminal>()
                        && terminal.read(cx).record(cx).is_some()
                    {
                        layouts
                            .entry(terminal.read(cx).address().clone())
                            .or_insert_with(|| layout.name().clone());
                    }
                }
            }
        }
        layouts
    }

    fn project_roots(&self, cx: &App) -> Vec<String> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect()
    }

    fn terminal_rows(&self, workspace: &Workspace, cx: &App) -> Vec<TerminalRow> {
        let layouts = self.layouts_by_address(workspace, cx);
        let roots = self.project_roots(cx);
        let mut rows = self
            .store
            .read(cx)
            .terminals()
            .into_iter()
            // A terminal with a tab in this window stays in sight: the
            // shell started on another machine from here runs in no folder
            // of the project.
            .filter(|record| {
                !self.project_only
                    || runs_in(record, &roots)
                    || layouts.contains_key(record.address.as_str())
            })
            .map(|record| {
                let address = SharedString::from(record.address.clone());
                let mut detail = Vec::new();
                if let Some(machine) = record.machine() {
                    detail.push(machine.to_string());
                }
                detail.push(short_folder(record.current_folder()));
                if let Some(layout) = layouts.get(&address) {
                    detail.push(format!("in {layout}"));
                }
                let mut tooltip = vec![format!("{} · {}", record.address, record.program())];
                tooltip.push(record.current_folder().to_string());
                if let Some(message) = record
                    .status
                    .as_ref()
                    .and_then(|status| status.msg.as_deref())
                    .filter(|message| !message.is_empty())
                {
                    tooltip.push(message.to_string());
                }
                TerminalRow {
                    address,
                    name: record.display_name().into(),
                    detail: detail.join(" · ").into(),
                    tooltip: tooltip.join("\n").into(),
                    state: record.agent_state(),
                    is_agent: is_agent(record),
                    is_live: record.is_live(),
                }
            })
            .collect::<Vec<_>>();
        // Who needs a person comes first; then who works.
        rows.sort_by_key(|row| row.state.unwrap_or(AgentState::Idle));
        rows
    }

    fn run_targets(&self, cx: &App) -> Vec<RunTarget> {
        let project = self.project.read(cx);
        let root = self.project_roots(cx).into_iter().next();
        let is_local = project.is_local();
        let place = |machine: &str, folder: Option<&String>| -> SharedString {
            match folder {
                Some(folder) => format!("{machine} · {}", short_folder(folder)).into(),
                None => format!("{machine} · home folder").into(),
            }
        };
        let here = RunTarget {
            machine: None,
            folder: root.clone().filter(|_| is_local),
            label: place("This machine", root.as_ref().filter(|_| is_local)),
        };
        let machines = self
            .store
            .read(cx)
            .machines()
            .iter()
            .filter(|machine| machine.is_connected())
            .map(|machine| {
                let folder = root.clone().filter(|_| !is_local);
                RunTarget {
                    machine: Some(machine.name.clone()),
                    label: place(&machine.name, folder.as_ref()),
                    folder,
                }
            });
        // The place of the project comes first.
        if is_local {
            std::iter::once(here).chain(machines).collect()
        } else {
            machines.chain(std::iter::once(here)).collect()
        }
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
        let run = self.store.read(cx).run(
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
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            let record = this.update(cx, |this, cx| this.report(result, cx))?;
            if let Some(record) = record {
                workspace.update_in(cx, |workspace, window, cx| {
                    open_herdr_terminal(workspace, &record.address, window, cx);
                })?;
            }
            anyhow::Ok(())
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
            let layout = new_layout(workspace, window, cx);
            workspace.rename_layout(layout, name, window, cx);
            let weak_workspace = cx.entity().downgrade();
            let workspace_id = workspace.database_id();
            let project = workspace.project().downgrade();
            let item = cx.new(|cx| {
                HerdrTerminal::new(address, weak_workspace, workspace_id, project, window, cx)
            });
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        });
    }

    fn end_program(
        &mut self,
        address: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("End the program of “{name}”?"),
            Some("Herdr Core hangs up on it, then terminates it. What it did not save is lost."),
            &["End Program", "Cancel"],
            cx,
        );
        let address = address.to_string();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            let kill = this.update(cx, |this, cx| this.store.read(cx).kill(&address, cx))?;
            let result = kill.await;
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

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.folded_sections.remove(&section) {
            self.folded_sections.insert(section);
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
            .child(
                IconButton::new("layouts-new", IconName::Plus)
                    .aria_label("New Layout")
                    .icon_size(IconSize::Small)
                    .shape(IconButtonShape::Square)
                    .tooltip(|_, cx| Tooltip::for_action("New Layout", &NewLayout, cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.update_workspace(window, cx, |workspace, window, cx| {
                            new_layout(workspace, window, cx);
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
        let is_only_layout = index == 0 && groups.is_empty() && self.is_single_layout(cx);
        right_click_menu(("layout-menu", index))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let groups = groups.clone();
                let group = group.clone();
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
                list = list.child(self.render_layout(index, layout, &groups, narrow, cx));
            }
        }
        for (group_index, group) in groups.iter().enumerate() {
            let is_folded = self.folded_groups.contains(group);
            let members = layouts
                .iter()
                .enumerate()
                .filter(|(_, layout)| layout.group == *group)
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
                    list = list.child(self.render_layout(index, layout, &groups, narrow, cx));
                }
            }
        }
        list.into_any_element()
    }

    fn render_section(
        &self,
        section: Section,
        rows: &[&TerminalRow],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if rows.is_empty() {
            return None;
        }
        let is_folded = self.folded_sections.contains(&section);
        let blocked = rows
            .iter()
            .filter(|row| row.state == Some(AgentState::Blocked))
            .count();
        let header = h_flex()
            .id(section.key())
            .role(Role::Button)
            .aria_label(format!("{} · {}", section.title(), rows.len()))
            .aria_expanded(!is_folded)
            .w_full()
            .gap_1p5()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                Label::new(section.title())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(blocked > 0, |header| header.child(CountBadge::new(blocked)))
            .child(
                Label::new(rows.len().to_string())
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
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)));
        Some(
            v_flex()
                .w_full()
                .gap_px()
                .child(header)
                .when(!is_folded, |list| {
                    list.children(
                        rows.iter()
                            .map(|row| self.render_terminal(section, row, cx)),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_terminal(
        &self,
        section: Section,
        row: &TerminalRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let address = row.address.clone();
        let name = row.name.clone();
        let is_live = row.is_live;
        let element = h_flex()
            .id(SharedString::from(format!(
                "herdr-{}-{}",
                section.key(),
                row.address
            )))
            .role(Role::Button)
            .aria_label(match row.state.filter(|_| row.is_live) {
                Some(state) => format!("{} · {} · {}", row.name, row.detail, state.label()),
                None => format!("{} · {}", row.name, row.detail),
            })
            .w_full()
            .gap_2()
            .px_1p5()
            .py_1()
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
                    .child(
                        Label::new(row.detail.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
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
            "herdr-menu-{}-{}",
            section.key(),
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

    fn render_inventory(&self, rows: &[TerminalRow], cx: &mut Context<Self>) -> AnyElement {
        let agents = rows
            .iter()
            .filter(|row| row.is_live && row.is_agent)
            .collect::<Vec<_>>();
        let terminals = rows
            .iter()
            .filter(|row| row.is_live && !row.is_agent)
            .collect::<Vec<_>>();
        let ended = rows.iter().filter(|row| !row.is_live).collect::<Vec<_>>();
        let is_empty = rows.is_empty();
        let is_connected = self.store.read(cx).is_connected();

        v_flex()
            .w_full()
            .px_1()
            .gap_1p5()
            .child(
                h_flex()
                    .w_full()
                    .px_1p5()
                    .gap_1()
                    .child(
                        Label::new("Herdr Core")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("herdr-project-only", IconName::Filter)
                            .aria_label("Show This Project Only")
                            .icon_size(IconSize::Small)
                            .shape(IconButtonShape::Square)
                            .toggle_state(self.project_only)
                            .tooltip(Tooltip::text(if self.project_only {
                                "Showing this project and the tabs of this window. Show every machine and folder."
                            } else {
                                "Showing every machine and folder. Show this project only."
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.project_only = !this.project_only;
                                this.serialize(cx);
                                cx.notify();
                            })),
                    )
                    .when(is_connected, |header| {
                        header.child(self.render_new_terminal_menu(cx))
                    }),
            )
            .children(self.render_section(Section::Agents, &agents, cx))
            .children(self.render_section(Section::Terminals, &terminals, cx))
            .children(self.render_section(Section::Ended, &ended, cx))
            .when(is_empty && is_connected, |list| {
                list.child(
                    div().px_1p5().child(
                        Label::new(if self.project_only {
                            "Nothing runs in this project."
                        } else {
                            "Nothing runs in Herdr Core."
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_new_terminal_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let targets = self.run_targets(cx);
        let panel = cx.entity().downgrade();
        PopoverMenu::new("herdr-new-terminal")
            .trigger_with_tooltip(
                IconButton::new("herdr-new-terminal-trigger", IconName::Plus)
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
    fn render_narrow_inventory(&self, rows: &[TerminalRow], cx: &mut Context<Self>) -> AnyElement {
        let blocked = rows
            .iter()
            .filter(|row| row.is_live && row.state == Some(AgentState::Blocked))
            .count();
        let working = rows
            .iter()
            .filter(|row| row.is_live && row.state == Some(AgentState::Working))
            .count();
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

/// Whether a terminal runs in one of the folders of a project.
fn runs_in(record: &TerminalRecord, roots: &[String]) -> bool {
    roots
        .iter()
        .any(|root| is_within(record.current_folder(), root) || is_within(&record.cwd, root))
}

fn is_within(folder: &str, root: &str) -> bool {
    let root = root.trim_end_matches(['/', '\\']);
    match folder.strip_prefix(root) {
        Some(rest) => rest.is_empty() || rest.starts_with(['/', '\\']),
        None => false,
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
        let (narrow, layouts, rows) = {
            let workspace = workspace.read(cx);
            (
                self.width(workspace, cx) < NARROW_LIMIT,
                self.layout_summaries(workspace, cx),
                self.terminal_rows(workspace, cx),
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
                    .child(self.render_layouts(&layouts, narrow, cx))
                    .child(div().mx_2().h_px().bg(cx.theme().colors().border_variant))
                    .child(if narrow {
                        self.render_narrow_inventory(&rows, cx)
                    } else {
                        self.render_inventory(&rows, cx)
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
