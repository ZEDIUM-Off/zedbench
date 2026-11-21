//! A tab on a terminal of Herdr Core. The core owns the program; the tab is
//! one of its viewers. Closing the tab ends the stream, never the program.

use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    Rgba, SharedString, Subscription, Task, WeakEntity, Window,
};
use herdr::{
    AgentState, AttachParams, CloseReason, ConnectionState, Display, HerdrEvent, HerdrStore,
    StreamEvent, StreamHandle, TerminalRecord,
};
use parking_lot::Mutex;
use project::Project;
use settings::Settings as _;
use std::{borrow::Cow, sync::Arc};
use terminal::{
    Terminal, TerminalBuilder, TerminalConnection, terminal_settings::TerminalSettings,
};
use terminal_view::TerminalView;
use theme::ActiveTheme as _;
use ui::prelude::*;
use util::{ResultExt as _, paths::PathStyle};
use workspace::{
    Item, ItemId, SerializableItem, Workspace, WorkspaceDb, WorkspaceId, delete_unloaded_items,
    item::{ItemEvent, TabContentParams},
    searchable::SearchableItemHandle,
};

/// The lines above the screen that the first paint brings.
const HISTORY_LINES: usize = 2000;

struct LinkState {
    stream: Option<StreamHandle>,
    size: Option<(u16, u16)>,
    focused: bool,
}

/// What the emulator of the tab sends to the program. Before the stream is
/// open there is nobody to send to: only the size is kept.
struct Link {
    state: Mutex<LinkState>,
    sized: mpsc::UnboundedSender<()>,
}

impl TerminalConnection for Link {
    fn input(&self, bytes: Cow<'static, [u8]>) {
        if let Some(stream) = &self.state.lock().stream {
            stream.input(bytes.into_owned());
        }
    }

    fn automatic_reply(&self, bytes: Cow<'static, [u8]>) {
        if let Some(stream) = &self.state.lock().stream {
            stream.automatic_input(bytes.into_owned());
        }
    }

    fn resize(&self, columns: u16, rows: u16) {
        let mut state = self.state.lock();
        if state.size == Some((columns, rows)) {
            return;
        }
        state.size = Some((columns, rows));
        match &state.stream {
            Some(stream) => stream.resize(columns, rows),
            // The tab has a size for the first time: it can attach.
            None => {
                if self.sized.unbounded_send(()).is_err() {
                    log::debug!("a Herdr terminal was sized after its tab closed");
                }
            }
        }
    }

    fn focus_changed(&self, focused: bool) {
        let mut state = self.state.lock();
        state.focused = focused;
        if let Some(stream) = &state.stream {
            stream.focus(focused);
        }
    }
}

enum Phase {
    /// The tab was never on screen: it has no size to attach with. A tab of a
    /// layout nobody looks at stays here and costs the core nothing.
    Waiting,
    Attaching,
    Attached,
    /// The program ended, or the core refused the stream.
    Ended(SharedString),
    /// The core cannot be reached. The tab attaches again when it can.
    Offline(SharedString),
}

pub struct HerdrTerminal {
    address: SharedString,
    /// When the core created the terminal. An address names another terminal
    /// once a machine is added again under the name of one that was removed,
    /// or once the state of a core is wiped: the tab must not type into it.
    created_ms: Option<u64>,
    store: Entity<HerdrStore>,
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
    link: Arc<Link>,
    phase: Phase,
    /// Whether the emulator already shows a paint of the core.
    painted: bool,
    workspace: WeakEntity<Workspace>,
    workspace_id: Option<WorkspaceId>,
    project: WeakEntity<Project>,
    needs_serialize: bool,
    attach_task: Task<()>,
    _attach_when_sized: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl HerdrTerminal {
    pub fn new(
        address: impl Into<SharedString>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = HerdrStore::global(cx);
        let (sized, mut sized_receiver) = mpsc::unbounded();
        let link = Arc::new(Link {
            state: Mutex::new(LinkState {
                stream: None,
                size: None,
                focused: false,
            }),
            sized,
        });

        let settings = TerminalSettings::get_global(cx);
        let cursor_shape = settings.cursor_shape;
        let alternate_scroll = settings.alternate_scroll;
        let max_scroll_history_lines = settings.max_scroll_history_lines;
        let path_style = project.upgrade().map_or(PathStyle::local(), |project| {
            project.read(cx).path_style(cx)
        });
        let window_id = window.window_handle().window_id().as_u64();
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_connected(
                link.clone(),
                cursor_shape,
                alternate_scroll,
                max_scroll_history_lines,
                window_id,
                cx.background_executor(),
                path_style,
            )
            .subscribe(cx)
        });
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.clone(),
                workspace_id,
                project.clone(),
                window,
                cx,
            )
        });

        let attach_when_sized = cx.spawn(async move |this, cx| {
            while sized_receiver.next().await.is_some() {
                let attached = this.update(cx, |this, cx| {
                    if matches!(this.phase, Phase::Waiting) {
                        this.attach(cx);
                    }
                });
                if attached.is_err() {
                    return;
                }
            }
        });

        let address = address.into();
        let created_ms = store
            .read(cx)
            .terminal(&address)
            .map(|record| record.created_ms)
            .filter(|created_ms| *created_ms != 0);
        Self {
            address,
            created_ms,
            _subscriptions: vec![cx.subscribe(&store, Self::handle_store_event)],
            store,
            terminal,
            view,
            link,
            phase: Phase::Waiting,
            painted: false,
            workspace,
            workspace_id,
            project,
            needs_serialize: true,
            attach_task: Task::ready(()),
            _attach_when_sized: attach_when_sized,
        }
    }

    pub fn address(&self) -> &SharedString {
        &self.address
    }

    /// What the core holds under the address, which may be another terminal
    /// than the one of the tab.
    fn record_at_address<'a>(&self, cx: &'a App) -> Option<&'a TerminalRecord> {
        self.store.read(cx).terminal(&self.address)
    }

    /// The terminal of the tab, as long as the core still has it.
    pub fn record<'a>(&self, cx: &'a App) -> Option<&'a TerminalRecord> {
        self.record_at_address(cx).filter(|_| !self.is_replaced(cx))
    }

    /// Whether the address now names a terminal other than the one of the tab.
    fn is_replaced(&self, cx: &App) -> bool {
        names_another_terminal(
            self.created_ms,
            self.record_at_address(cx).map(|record| record.created_ms),
        )
    }

    /// Whether the program of the tab is gone: ended, removed from the core,
    /// or replaced under its address. Without a connection nothing says so:
    /// only a core that answers does.
    pub fn is_ended(&self, cx: &App) -> bool {
        match self.record_at_address(cx) {
            Some(record) => !record.is_live() || self.is_replaced(cx),
            None => self.store.read(cx).is_connected(),
        }
    }

    fn handle_store_event(
        &mut self,
        _: Entity<HerdrStore>,
        event: &HerdrEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            HerdrEvent::Connected => {
                if matches!(self.phase, Phase::Offline(_)) {
                    self.attach(cx);
                }
            }
            HerdrEvent::Disconnected => {
                self.detach();
                self.phase = Phase::Offline("The connection to Herdr Core was lost.".into());
            }
            HerdrEvent::Restarted(address) if *address == *self.address => self.attach(cx),
            // A restart or a removal may be among the events the store missed.
            HerdrEvent::Resynced => match self.record_at_address(cx).map(|record| record.is_live())
            {
                Some(true) if matches!(self.phase, Phase::Ended(_)) => self.attach(cx),
                None => {
                    self.detach();
                    self.phase = Phase::Ended("This terminal was removed from Herdr Core.".into());
                }
                _ => {}
            },
            HerdrEvent::Removed(address) if *address == *self.address => {
                self.detach();
                self.phase = Phase::Ended("This terminal was removed from Herdr Core.".into());
            }
            HerdrEvent::Created(address)
            | HerdrEvent::Changed(address)
            | HerdrEvent::StatusChanged { address, .. }
                if *address == *self.address => {}
            _ => return,
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn detach(&mut self) {
        self.link.state.lock().stream = None;
        // Dropping the task drops the stream, which tells the core to close it.
        self.attach_task = Task::ready(());
    }

    /// Opens the stream of the terminal: the core paints the screen as it is,
    /// then sends what the program prints.
    fn attach(&mut self, cx: &mut Context<Self>) {
        self.detach();
        let Some((columns, rows)) = self.link.state.lock().size else {
            self.phase = Phase::Waiting;
            return;
        };
        if !self.store.read(cx).is_connected() {
            self.phase = Phase::Offline(match self.store.read(cx).state() {
                ConnectionState::Disconnected { error } => error.clone(),
                _ => "Connecting to Herdr Core…".into(),
            });
            cx.notify();
            return;
        }
        if self.is_replaced(cx) {
            self.phase =
                Phase::Ended("Another terminal has this address in Herdr Core now.".into());
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
            return;
        }
        if self.created_ms.is_none()
            && let Some(created_ms) = self
                .record(cx)
                .map(|record| record.created_ms)
                .filter(|created_ms| *created_ms != 0)
        {
            self.created_ms = Some(created_ms);
            if let Some(save) = self.save_address(cx) {
                save.detach_and_log_err(cx);
            }
        }
        self.phase = Phase::Attaching;
        cx.notify();

        let attach = self.store.read(cx).attach(
            AttachParams {
                terminal: self.address.to_string(),
                cols: columns,
                rows,
                history: HISTORY_LINES,
                display: display_facts(cx),
            },
            cx,
        );
        self.attach_task = cx.spawn(async move |this, cx| {
            let mut stream = match attach.await {
                Ok(attachment) => attachment.stream,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.phase = Phase::Ended(format!("{error:#}").into());
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            let handle = stream.handle();
            let opened = this.update(cx, |this, cx| {
                {
                    let mut state = this.link.state.lock();
                    if let Some((current_columns, current_rows)) = state.size
                        && (current_columns, current_rows) != (columns, rows)
                    {
                        handle.resize(current_columns, current_rows);
                    }
                    if state.focused {
                        handle.focus(true);
                    }
                    state.stream = Some(handle);
                }
                if this.painted {
                    // The core paints the whole screen again: what the tab
                    // showed before would stay above it.
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.write_raw_output(b"\x1bc", cx);
                    });
                }
                this.painted = true;
                this.phase = Phase::Attached;
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            });
            if opened.is_err() {
                return;
            }

            let mut close_reason = None;
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Data(bytes) => {
                        let written = this.update(cx, |this, cx| {
                            this.terminal.update(cx, |terminal, cx| {
                                terminal.write_raw_output(&bytes, cx);
                            });
                        });
                        if written.is_err() {
                            return;
                        }
                        stream.consumed(bytes.len());
                    }
                    // Only a viewer that asked to observe follows the size of
                    // others. This one sets the size.
                    StreamEvent::Resize { .. } => {}
                    StreamEvent::Closed(reason) => {
                        close_reason = reason;
                        break;
                    }
                }
            }
            this.update(cx, |this, cx| this.stream_closed(close_reason, cx))
                .log_err();
        });
    }

    fn stream_closed(&mut self, reason: Option<CloseReason>, cx: &mut Context<Self>) {
        self.link.state.lock().stream = None;
        self.phase = match reason {
            Some(reason) => Phase::Ended(describe_close(&reason)),
            None if self.store.read(cx).is_connected() => {
                Phase::Ended("Herdr Core closed the stream of this terminal.".into())
            }
            None => Phase::Offline("The connection to Herdr Core was lost.".into()),
        };
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn save_address(&mut self, cx: &mut Context<Self>) -> Option<Task<Result<()>>> {
        let workspace_id = self.workspace_id?;
        let item_id = cx.entity_id().as_u64();
        let address = self.address.to_string();
        let created_ms = self.created_ms;
        self.needs_serialize = false;
        let db = HerdrTerminalDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_terminal(item_id, workspace_id, address, created_ms)
                .await
        }))
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (message, color, action) = match &self.phase {
            Phase::Waiting | Phase::Attached => return None,
            Phase::Attaching => ("Attaching…".into(), Color::Muted, None),
            // A program that ended has no stream to open: the core would only
            // refuse it again.
            Phase::Ended(message) => (
                message.clone(),
                Color::Muted,
                self.record(cx)
                    .is_some_and(|record| record.is_live())
                    .then_some("Attach Again"),
            ),
            Phase::Offline(message) => (message.clone(), Color::Warning, Some("Reconnect")),
        };
        let is_offline = matches!(self.phase, Phase::Offline(_));
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().panel_background)
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(color)
                        .truncate(),
                )
                .child(div().flex_1())
                .when_some(action, |this, action| {
                    this.child(
                        Button::new("herdr-terminal-retry", action)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if is_offline {
                                    this.store.update(cx, |store, cx| store.reconnect(cx));
                                } else {
                                    this.attach(cx);
                                }
                            })),
                    )
                })
                .into_any_element(),
        )
    }
}

fn describe_close(reason: &CloseReason) -> SharedString {
    if let Some(code) = reason.exit_code {
        return format!("The program ended with code {code}.").into();
    }
    if let Some(signal) = reason.signal {
        return format!("The program was ended by signal {signal}.").into();
    }
    match reason.reason.as_deref() {
        Some(reason) if !reason.is_empty() => format!("The stream ended: {reason}.").into(),
        _ => "The program ended.".into(),
    }
}

fn hex_color(color: Hsla) -> String {
    let color = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(color.r),
        channel(color.g),
        channel(color.b)
    )
}

/// The colors of the tab, so the core answers a program that asks for them
/// before any viewer does.
fn display_facts(cx: &App) -> Display {
    let colors = cx.theme().colors();
    Display {
        fg: Some(hex_color(colors.terminal_foreground)),
        bg: Some(hex_color(colors.terminal_background)),
        cursor: Some(hex_color(cx.theme().players().local().cursor)),
        cell: None,
    }
}

/// Whether a terminal runs a program that reports what it does, which makes
/// it an agent rather than a plain terminal.
pub fn is_agent(record: &TerminalRecord) -> bool {
    record.reported || record.status.is_some()
}

pub fn agent_color(state: AgentState) -> Color {
    match state {
        AgentState::Blocked | AgentState::Error => Color::Error,
        AgentState::Working => Color::Warning,
        AgentState::Done => Color::Success,
        AgentState::Idle => Color::Muted,
    }
}

/// A core that does not date its terminals reports zero: nothing can be told
/// from it.
fn names_another_terminal(saved_created_ms: Option<u64>, current_created_ms: Option<u64>) -> bool {
    match (saved_created_ms, current_created_ms) {
        (Some(saved), Some(current)) => current != 0 && current != saved,
        _ => false,
    }
}

pub fn terminal_icon(record: Option<&TerminalRecord>) -> (IconName, Color) {
    match record {
        Some(record) if is_agent(record) => (
            IconName::ZedAgent,
            record.agent_state().map_or(Color::Muted, agent_color),
        ),
        Some(record) if record.is_live() => (IconName::Terminal, Color::Muted),
        _ => (IconName::Terminal, Color::Disabled),
    }
}

/// The tab of `address` in the window, in any of its layouts. A tab whose
/// address went to another terminal is not a tab of that terminal.
pub fn find_herdr_terminal(
    workspace: &Workspace,
    address: &str,
    cx: &App,
) -> Option<Entity<HerdrTerminal>> {
    workspace.panes().iter().find_map(|pane| {
        pane.read(cx).items().find_map(|item| {
            item.downcast::<HerdrTerminal>().filter(|terminal| {
                let terminal = terminal.read(cx);
                &**terminal.address() == address && !terminal.is_replaced(cx)
            })
        })
    })
}

/// Shows the tab of a terminal, in the layout that has it, or opens one in
/// the layout on screen.
pub fn open_herdr_terminal(
    workspace: &mut Workspace,
    address: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(item) = find_herdr_terminal(workspace, address, cx) {
        workspace.activate_item(&item, true, true, window, cx);
        return;
    }
    let weak_workspace = cx.entity().downgrade();
    let workspace_id = workspace.database_id();
    let project = workspace.project().downgrade();
    let address = SharedString::from(address.to_string());
    let item =
        cx.new(|cx| HerdrTerminal::new(address, weak_workspace, workspace_id, project, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

impl EventEmitter<ItemEvent> for HerdrTerminal {}

impl Focusable for HerdrTerminal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for HerdrTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .children(self.render_banner(cx))
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}

impl Item for HerdrTerminal {
    type Event = ItemEvent;

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        let (icon, color) = match self.phase {
            Phase::Ended(_) | Phase::Offline(_) => {
                (terminal_icon(self.record(cx)).0, Color::Disabled)
            }
            _ => terminal_icon(self.record(cx)),
        };
        h_flex()
            .gap_1()
            .child(Icon::new(icon).color(color).size(IconSize::Small))
            .child(
                Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
                    .single_line()
                    .color(params.text_color()),
            )
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.record(cx) {
            Some(record) => record.display_name().into(),
            None => self.address.clone(),
        }
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let record = self.record(cx)?;
        let state = record
            .agent_state()
            .map(|state| format!(" · {}", state.label()))
            .unwrap_or_default();
        Some(format!("{} · {}{state}", self.address, record.current_folder()).into())
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

    /// A second viewer of the same terminal, not a second program.
    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let address = self.address.clone();
        let created_ms = self.created_ms;
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        Task::ready(Some(cx.new(|cx| {
            let mut terminal =
                HerdrTerminal::new(address, workspace, workspace_id, project, window, cx);
            terminal.created_ms = created_ms;
            terminal
        })))
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.view.clone()))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_id = workspace.database_id();
        // A restored tab has a new id: without this row, the next start would
        // not know which terminal it shows.
        if let Some(save) = self.save_address(cx) {
            save.detach_and_log_err(cx);
        }
    }
}

impl SerializableItem for HerdrTerminal {
    fn serialized_item_kind() -> &'static str {
        "HerdrTerminal"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = HerdrTerminalDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "herdr_terminals", &db, cx)
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = HerdrTerminalDb::global(cx);
        let saved = db
            .terminal(item_id, workspace_id)
            .and_then(|saved| saved.context("no Herdr terminal was saved for this tab"));
        Task::ready(saved.map(|(address, created_ms)| {
            cx.new(|cx| {
                let mut terminal = HerdrTerminal::new(
                    address,
                    workspace,
                    Some(workspace_id),
                    project.downgrade(),
                    window,
                    cx,
                );
                // What the store holds under the address at this point may be
                // another terminal: only what was saved says which one it was.
                terminal.created_ms = created_ms;
                terminal
            })
        }))
    }

    fn serialize(
        &mut self,
        _: &mut Workspace,
        _: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        if !self.needs_serialize {
            return None;
        }
        self.save_address(cx)
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        self.needs_serialize
    }
}

pub struct HerdrTerminalDb(ThreadSafeConnection);

impl Domain for HerdrTerminalDb {
    const NAME: &str = stringify!(HerdrTerminalDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE herdr_terminals (
                workspace_id INTEGER,
                item_id INTEGER,
                address TEXT NOT NULL,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        sql!(
            ALTER TABLE herdr_terminals ADD COLUMN created_ms INTEGER;
        ),
    ];
}

db::static_connection!(HerdrTerminalDb, [WorkspaceDb]);

impl HerdrTerminalDb {
    query! {
        pub async fn save_terminal(
            item_id: ItemId,
            workspace_id: WorkspaceId,
            address: String,
            created_ms: Option<u64>
        ) -> Result<()> {
            INSERT INTO herdr_terminals(item_id, workspace_id, address, created_ms)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT DO UPDATE SET
                address = ?3,
                created_ms = ?4
        }
    }

    query! {
        pub fn terminal(
            item_id: ItemId,
            workspace_id: WorkspaceId
        ) -> Result<Option<(String, Option<u64>)>> {
            SELECT address, created_ms
            FROM herdr_terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_given_to_a_newer_terminal_names_another_terminal() {
        assert!(names_another_terminal(Some(5), Some(9)));
        assert!(!names_another_terminal(Some(5), Some(5)));
    }

    #[test]
    fn nothing_is_told_without_both_dates() {
        assert!(!names_another_terminal(None, Some(9)));
        assert!(!names_another_terminal(Some(5), None));
        assert!(!names_another_terminal(Some(5), Some(0)));
    }
}
