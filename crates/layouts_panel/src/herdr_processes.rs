//! A tab with every program of Herdr Core, as a table to filter and sort.
//! Each tab keeps its own filter and order, so that a layout can hold the
//! overview of one workspace.

use super::{
    HerdrTerminal, Place, display_folder, end_program, folder_name, is_agent, open_herdr_terminal,
    open_in_new_layout, place_of, project_roots,
};
use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Role, ScrollHandle, SharedString, Subscription, Task, WeakEntity, Window, px,
};
use herdr::{AgentState, HerdrStore, TerminalRecord};
use project::Project;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};
use theme::ActiveTheme as _;
use ui::{ContextMenu, IconButtonShape, Indicator, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    Item, ItemId, SerializableItem, Workspace, WorkspaceDb, WorkspaceId, delete_unloaded_items,
    item::ItemEvent,
};

use super::agent_color;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Column {
    #[default]
    Status,
    Name,
    Program,
    Machine,
    Workspace,
    Worktree,
    Layout,
    Started,
}

impl Column {
    const ALL: [Column; 8] = [
        Column::Status,
        Column::Name,
        Column::Program,
        Column::Machine,
        Column::Workspace,
        Column::Worktree,
        Column::Layout,
        Column::Started,
    ];

    fn title(self) -> &'static str {
        match self {
            Column::Status => "Status",
            Column::Name => "Name",
            Column::Program => "Program",
            Column::Machine => "Machine",
            Column::Workspace => "Workspace",
            Column::Worktree => "Worktree",
            Column::Layout => "Layout",
            Column::Started => "Started",
        }
    }

    /// A fixed width, or `None` for a column that shares what is left.
    fn width(self) -> Option<Pixels> {
        match self {
            Column::Status => Some(px(96.)),
            Column::Name | Column::Workspace => None,
            Column::Program => Some(px(110.)),
            Column::Machine => Some(px(120.)),
            Column::Worktree => Some(px(150.)),
            Column::Layout => Some(px(120.)),
            Column::Started => Some(px(72.)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    #[default]
    All,
    Agents,
    Terminals,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Kind::All => "All",
            Kind::Agents => "Agents",
            Kind::Terminals => "Terminals",
        }
    }
}

/// What a table shows and in what order. Saved with its tab.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProcessView {
    #[serde(default)]
    query: String,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    show_ended: bool,
    #[serde(default)]
    sort: Column,
    #[serde(default)]
    descending: bool,
}

struct ProcessRow {
    address: SharedString,
    name: SharedString,
    program: SharedString,
    machine: SharedString,
    place: Place,
    folder: SharedString,
    layout: Option<SharedString>,
    state: Option<AgentState>,
    is_agent: bool,
    is_live: bool,
    created_ms: u64,
    tooltip: SharedString,
}

impl ProcessRow {
    fn text(&self, column: Column) -> SharedString {
        match column {
            Column::Status => self.status().into(),
            Column::Name => self.name.clone(),
            Column::Program => self.program.clone(),
            Column::Machine => self.machine.clone(),
            Column::Workspace => folder_name(&self.place.workspace).into(),
            Column::Worktree => self.place.worktree.clone().unwrap_or_default().into(),
            Column::Layout => self.layout.clone().unwrap_or_default(),
            Column::Started => age(self.created_ms).into(),
        }
    }

    fn status(&self) -> &'static str {
        match (self.is_live, self.state) {
            (false, _) => "Ended",
            (true, Some(state)) => state.label(),
            (true, None) => "Running",
        }
    }

    /// Who needs a person first, then who works, and ended programs last.
    fn status_rank(&self) -> (bool, bool, Option<AgentState>) {
        (!self.is_live, self.state.is_none(), self.state)
    }

    fn compare(&self, other: &Self, column: Column) -> Ordering {
        let by_column = match column {
            Column::Status => self.status_rank().cmp(&other.status_rank()),
            // The newest first reads as the smallest age.
            Column::Started => other.created_ms.cmp(&self.created_ms),
            Column::Workspace => self
                .place
                .workspace
                .cmp(&other.place.workspace)
                .then_with(|| self.place.worktree.cmp(&other.place.worktree)),
            _ => self
                .text(column)
                .to_lowercase()
                .cmp(&other.text(column).to_lowercase()),
        };
        by_column.then_with(|| self.name.to_lowercase().cmp(&other.name.to_lowercase()))
    }

    fn matches(&self, words: &[String]) -> bool {
        let haystack = [
            &*self.name,
            &*self.program,
            &*self.machine,
            &self.place.workspace,
            self.place.worktree.as_deref().unwrap_or(""),
            &*self.folder,
            self.layout.as_deref().unwrap_or(""),
            &*self.address,
            self.status(),
        ]
        .join("\n")
        .to_lowercase();
        words.iter().all(|word| haystack.contains(word.as_str()))
    }
}

/// How long ago a program started, in its largest unit.
fn age(created_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_millis() as u64)
        .unwrap_or(created_ms);
    let minutes = now_ms.saturating_sub(created_ms) / 60_000;
    match minutes {
        0 => "now".to_string(),
        1..60 => format!("{minutes} min"),
        60..1440 => format!("{} h", minutes / 60),
        _ => format!("{} d", minutes / 1440),
    }
}

pub struct HerdrProcesses {
    workspace: WeakEntity<Workspace>,
    workspace_id: Option<WorkspaceId>,
    store: Entity<HerdrStore>,
    view: ProcessView,
    query_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl HerdrProcesses {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        view: ProcessView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = HerdrStore::global(cx);
        let query_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter by name, program, machine, folder…", window, cx);
            editor.set_text(view.query.clone(), window, cx);
            editor
        });
        let workspace_id = workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).database_id());
        let mut subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&query_editor, |this, editor, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    let query = editor.read(cx).text(cx);
                    this.change_view(cx, |view| view.query = query);
                }
            }),
        ];
        if let Some(workspace) = workspace.upgrade() {
            subscriptions
                .push(cx.subscribe(&workspace, |_, _, _: &workspace::Event, cx| cx.notify()));
        }
        Self {
            workspace,
            workspace_id,
            store,
            view,
            query_editor,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn change_view(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ProcessView)) {
        let before = self.view.clone();
        change(&mut self.view);
        if self.view == before {
            return;
        }
        if let Some(save) = self.save_view(cx) {
            save.detach_and_log_err(cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn save_view(&self, cx: &mut Context<Self>) -> Option<Task<Result<()>>> {
        let workspace_id = self.workspace_id?;
        let item_id = cx.entity_id().as_u64();
        let json = serde_json::to_string(&self.view).log_err()?;
        let db = HerdrProcessesDb::global(cx);
        Some(cx.background_spawn(async move { db.save_table(item_id, workspace_id, json).await }))
    }

    fn rows(&self, cx: &App) -> (Vec<ProcessRow>, usize) {
        let Some(workspace) = self.workspace.upgrade() else {
            return (Vec::new(), 0);
        };
        let workspace = workspace.read(cx);
        let roots = project_roots(workspace.project(), &self.store, cx);
        let mut layouts = collections::HashMap::<String, SharedString>::default();
        for layout in workspace.layouts() {
            for pane in workspace.layout_panes(layout.id()) {
                for item in pane.read(cx).items() {
                    if let Some(terminal) = item.downcast::<HerdrTerminal>()
                        && !terminal.read(cx).is_ended(cx)
                    {
                        layouts
                            .entry(terminal.read(cx).address().to_string())
                            .or_insert_with(|| layout.name().clone());
                    }
                }
            }
        }
        let words = self
            .view
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let records = self.store.read(cx).terminals();
        let total = records.len();
        let mut rows = records
            .into_iter()
            .map(|record| process_row(record, &roots, &layouts))
            .filter(|row| self.view.show_ended || row.is_live)
            .filter(|row| match self.view.kind {
                Kind::All => true,
                Kind::Agents => row.is_agent,
                Kind::Terminals => !row.is_agent,
            })
            .filter(|row| row.matches(&words))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            let order = left.compare(right, self.view.sort);
            if self.view.descending {
                order.reverse()
            } else {
                order
            }
        });
        (rows, total)
    }

    /// Runs `update` once this tab is done with its own update: the
    /// workspace reads its items when it shows another one.
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

    fn report(&mut self, result: Result<()>, cx: &mut Context<Self>) {
        if let Err(error) = result {
            self.error = Some(format!("{error:#}").into());
            cx.notify();
        }
    }

    fn sort_by(&mut self, column: Column, cx: &mut Context<Self>) {
        self.change_view(cx, |view| {
            if view.sort == column {
                view.descending = !view.descending;
            } else {
                view.sort = column;
                view.descending = false;
            }
        });
    }

    fn render_toolbar(&self, shown: usize, total: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.query_editor.clone())),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .children([Kind::All, Kind::Agents, Kind::Terminals].map(|kind| {
                        Button::new(SharedString::from(kind.title()), kind.title())
                            .label_size(LabelSize::Small)
                            .toggle_state(self.view.kind == kind)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change_view(cx, |view| view.kind = kind);
                            }))
                    })),
            )
            .child(
                Button::new("processes-show-ended", "Ended")
                    .label_size(LabelSize::Small)
                    .toggle_state(self.view.show_ended)
                    .tooltip(Tooltip::text(if self.view.show_ended {
                        "Hide the programs that ended"
                    } else {
                        "Show the programs that ended"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.change_view(cx, |view| view.show_ended = !view.show_ended);
                    })),
            )
            .child(
                Label::new(format!("{shown} of {total}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(Column::ALL.map(|column| {
                let is_sorted = self.view.sort == column;
                cell(column)
                    .id(SharedString::from(format!("processes-sort-{column:?}")))
                    .role(Role::Button)
                    .aria_label(format!("Sort by {}", column.title()))
                    .gap_0p5()
                    .cursor_pointer()
                    .child(
                        Label::new(column.title())
                            .size(LabelSize::Small)
                            .color(if is_sorted {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    )
                    .when(is_sorted, |cell| {
                        cell.child(
                            Icon::new(if self.view.descending {
                                IconName::ArrowUp
                            } else {
                                IconName::ArrowDown
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.sort_by(column, cx)))
            }))
            .into_any_element()
    }

    fn render_row(&self, index: usize, row: &ProcessRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let address = row.address.clone();
        let name = row.name.clone();
        let is_live = row.is_live;
        let element = h_flex()
            .id(("process", index))
            .role(Role::Row)
            .aria_label(format!("{} · {} · {}", row.name, row.status(), row.folder))
            .w_full()
            .px_2()
            .py_0p5()
            .gap_2()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .children(Column::ALL.map(|column| {
                let text = row.text(column);
                let color = if !row.is_live {
                    Color::Disabled
                } else if column == Column::Name {
                    Color::Default
                } else {
                    Color::Muted
                };
                cell(column)
                    .gap_1p5()
                    .when(column == Column::Status, |cell| {
                        cell.child(Indicator::dot().color(match row.state {
                            Some(state) if row.is_live => agent_color(state),
                            _ if row.is_live => Color::Muted,
                            _ => Color::Disabled,
                        }))
                    })
                    .child(
                        Label::new(text)
                            .size(LabelSize::Small)
                            .color(match (column, row.state) {
                                (Column::Status, Some(state)) if row.is_live => agent_color(state),
                                _ => color,
                            })
                            .truncate(),
                    )
            }))
            .tooltip(Tooltip::text(row.tooltip.clone()))
            .on_click(cx.listener({
                let address = address.clone();
                move |this, _, window, cx| {
                    if is_live {
                        let address = address.to_string();
                        this.update_workspace(window, cx, move |workspace, window, cx| {
                            open_herdr_terminal(workspace, &address, window, cx);
                        });
                    }
                }
            }));

        let table = cx.entity().downgrade();
        right_click_menu(("process-menu", index))
            .trigger(move |_, _, _| element)
            .menu(move |window, cx| {
                let table = table.clone();
                let address = address.clone();
                let name = name.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    if !is_live {
                        return menu.entry("Remove from Herdr Core", None, move |_, cx| {
                            table
                                .update(cx, |this, cx| {
                                    let remove = this.store.read(cx).remove(&address, cx);
                                    cx.spawn(async move |this, cx| {
                                        let result = remove.await;
                                        this.update(cx, |this, cx| this.report(result, cx))
                                    })
                                    .detach_and_log_err(cx);
                                })
                                .log_err();
                        });
                    }
                    menu.entry("Open", None, {
                        let table = table.clone();
                        let address = address.clone();
                        move |window, cx| {
                            let address = address.to_string();
                            table
                                .update(cx, |this, cx| {
                                    this.update_workspace(
                                        window,
                                        cx,
                                        move |workspace, window, cx| {
                                            open_herdr_terminal(workspace, &address, window, cx);
                                        },
                                    );
                                })
                                .log_err();
                        }
                    })
                    .entry("Open in a New Layout", None, {
                        let table = table.clone();
                        let address = address.clone();
                        let name = name.clone();
                        move |window, cx| {
                            let address = address.to_string();
                            let name = name.to_string();
                            table
                                .update(cx, |this, cx| {
                                    this.update_workspace(
                                        window,
                                        cx,
                                        move |workspace, window, cx| {
                                            open_in_new_layout(
                                                workspace, &address, &name, window, cx,
                                            );
                                        },
                                    );
                                })
                                .log_err();
                        }
                    })
                    .separator()
                    .entry("End Program…", None, move |window, cx| {
                        table
                            .update(cx, |this, cx| {
                                let end = end_program(&this.store, &address, &name, window, cx);
                                cx.spawn(async move |this, cx| {
                                    let result = end.await;
                                    this.update(cx, |this, cx| this.report(result, cx))
                                })
                                .detach_and_log_err(cx);
                            })
                            .log_err();
                    })
                })
            })
            .into_any_element()
    }
}

fn cell(column: Column) -> gpui::Stateful<gpui::Div> {
    let cell = h_flex()
        .id(SharedString::from(format!("cell-{column:?}")))
        .min_w_0()
        .overflow_hidden();
    match column.width() {
        Some(width) => cell.w(width).flex_none(),
        None => cell.flex_1(),
    }
}

fn process_row(
    record: &TerminalRecord,
    roots: &[(Option<String>, String)],
    layouts: &collections::HashMap<String, SharedString>,
) -> ProcessRow {
    let place = place_of(record.machine(), record.current_folder(), roots);
    let machine = record.machine().unwrap_or("This machine");
    let folder = display_folder(record.current_folder());
    let mut tooltip = vec![
        format!("{} · {}", record.address, record.program()),
        format!("{machine} · {folder}"),
    ];
    if let Some(message) = record
        .status
        .as_ref()
        .and_then(|status| status.msg.as_deref())
        .filter(|message| !message.is_empty())
    {
        tooltip.push(message.to_string());
    }
    ProcessRow {
        address: record.address.clone().into(),
        name: record.display_name().into(),
        program: record.program().to_string().into(),
        machine: machine.to_string().into(),
        place,
        folder: folder.into(),
        layout: layouts.get(&record.address).cloned(),
        state: record.agent_state(),
        is_agent: is_agent(record),
        is_live: record.is_live(),
        created_ms: record.created_ms,
        tooltip: tooltip.join("\n").into(),
    }
}

impl EventEmitter<ItemEvent> for HerdrProcesses {}

impl Focusable for HerdrProcesses {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HerdrProcesses {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, total) = self.rows(cx);
        let is_connected = self.store.read(cx).is_connected();
        v_flex()
            .id("herdr-processes")
            .key_context("HerdrProcesses")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(rows.len(), total, cx))
            .when_some(self.error.clone(), |table, error| {
                table.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(
                            div().flex_1().child(
                                Label::new(error).size(LabelSize::Small).color(Color::Error),
                            ),
                        )
                        .child(
                            IconButton::new("processes-dismiss-error", IconName::Close)
                                .aria_label("Dismiss")
                                .icon_size(IconSize::XSmall)
                                .shape(IconButtonShape::Square)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("herdr-processes-rows")
                    .role(Role::Table)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| self.render_row(index, row, cx)),
                    )
                    .when(rows.is_empty(), |list| {
                        list.child(
                            div().p_3().child(
                                Label::new(if !is_connected {
                                    "Herdr Core is not connected."
                                } else if total == 0 {
                                    "Nothing runs in Herdr Core."
                                } else {
                                    "No program matches the filter."
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                        )
                    }),
            )
    }
}

impl Item for HerdrProcesses {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        let query = self.view.query.trim();
        if query.is_empty() {
            "Herdr Programs".into()
        } else {
            format!("Herdr Programs · {query}").into()
        }
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let workspace = self.workspace.clone();
        let view = self.view.clone();
        Task::ready(Some(
            cx.new(|cx| HerdrProcesses::new(workspace, view, window, cx)),
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_id = workspace.database_id();
        // A restored tab has a new id: the row of the old one goes away with
        // the cleanup of the workspace.
        if let Some(save) = self.save_view(cx) {
            save.detach_and_log_err(cx);
        }
    }
}

impl SerializableItem for HerdrProcesses {
    fn serialized_item_kind() -> &'static str {
        "HerdrProcesses"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = HerdrProcessesDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "herdr_process_tables", &db, cx)
    }

    fn deserialize(
        _: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = HerdrProcessesDb::global(cx);
        let view = db
            .view(item_id, workspace_id)
            .and_then(|json| json.context("no table of Herdr programs was saved for this tab"))
            .and_then(|json| Ok(serde_json::from_str::<ProcessView>(&json)?));
        Task::ready(view.map(|view| cx.new(|cx| HerdrProcesses::new(workspace, view, window, cx))))
    }

    fn serialize(
        &mut self,
        _: &mut Workspace,
        _: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        self.save_view(cx)
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }
}

pub struct HerdrProcessesDb(ThreadSafeConnection);

impl Domain for HerdrProcessesDb {
    const NAME: &str = stringify!(HerdrProcessesDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE herdr_process_tables (
            workspace_id INTEGER,
            item_id INTEGER,
            view TEXT NOT NULL,
            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(HerdrProcessesDb, [WorkspaceDb]);

impl HerdrProcessesDb {
    query! {
        pub async fn save_table(
            item_id: ItemId,
            workspace_id: WorkspaceId,
            view: String
        ) -> Result<()> {
            INSERT INTO herdr_process_tables(item_id, workspace_id, view)
            VALUES (?1, ?2, ?3)
            ON CONFLICT DO UPDATE SET view = ?3
        }
    }

    query! {
        pub fn view(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT view
            FROM herdr_process_tables
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, state: Option<AgentState>, is_live: bool, created_ms: u64) -> ProcessRow {
        ProcessRow {
            address: name.to_string().into(),
            name: name.to_string().into(),
            program: "pi".into(),
            machine: "dgx".into(),
            place: Place {
                machine: Some("dgx".into()),
                workspace: "/home/ada/acme".into(),
                worktree: Some("fix".into()),
            },
            folder: "~/acme/.worktrees/fix".into(),
            layout: None,
            state,
            is_agent: true,
            is_live,
            created_ms,
            tooltip: SharedString::default(),
        }
    }

    #[test]
    fn who_needs_a_person_comes_first_and_ended_programs_last() {
        let ended = row("a", Some(AgentState::Blocked), false, 1);
        let blocked = row("b", Some(AgentState::Blocked), true, 1);
        let idle = row("c", Some(AgentState::Idle), true, 1);
        assert_eq!(blocked.compare(&idle, Column::Status), Ordering::Less);
        assert_eq!(idle.compare(&ended, Column::Status), Ordering::Less);
    }

    #[test]
    fn every_word_of_the_filter_must_match() {
        let row = row("review", None, true, 1);
        assert!(row.matches(&["acme".into(), "fix".into()]));
        assert!(row.matches(&["dgx".into(), "review".into()]));
        assert!(!row.matches(&["acme".into(), "beta".into()]));
    }

    #[test]
    fn a_saved_view_reads_back_with_defaults() {
        let view = serde_json::from_str::<ProcessView>(r#"{"query":"acme"}"#).unwrap_or_default();
        assert_eq!(view.query, "acme");
        assert_eq!(view.sort, Column::Status);
        assert!(!view.show_ended);
    }
}
