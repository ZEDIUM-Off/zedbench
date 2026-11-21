//! A client of Herdr Core (`hc`), the runtime that keeps terminals and coding
//! agents alive outside of any window. The store mirrors what the core knows:
//! its terminals, here and on the machines it added, and how each one is
//! doing. Zedbench shows terminals; the core owns their programs.

mod connection;

use anyhow::{Context as _, Result, anyhow};
use collections::BTreeMap;
use futures::{FutureExt as _, select_biased};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use util::ResultExt as _;

pub use connection::{Connection, RpcError, Stream, StreamEvent, StreamHandle};

const FIRST_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const LONGEST_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const MACHINE_POLL_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CoreInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Hello {
    pub version: String,
    pub core: CoreInfo,
    pub framed: bool,
    #[serde(default)]
    pub features: Vec<String>,
}

/// What a program says about itself through OSC 7501. The core never guesses
/// it from the screen, so a program that does not report has no status.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Status {
    pub state: String,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// For a blocked program: permission, question or auth.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub progress: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentState {
    /// A person is needed: a permission or a question.
    Blocked,
    Error,
    Working,
    Done,
    Idle,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Blocked => "Needs you",
            AgentState::Error => "Error",
            AgentState::Working => "Working",
            AgentState::Done => "Done",
            AgentState::Idle => "Idle",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Foreground {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct TerminalRecord {
    /// `t7` on the core of this machine, `machine/t7` on a machine it added.
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    /// The folder the terminal started in.
    #[serde(default)]
    pub cwd: String,
    /// `live` or `finished`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub reported: bool,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub foreground: Option<Foreground>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub status_ms: Option<u64>,
    #[serde(default)]
    pub finished_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub pid: Option<u32>,
}

impl TerminalRecord {
    /// The machine the terminal runs on, as this core names it. `None` for
    /// the machine of the core itself.
    pub fn machine(&self) -> Option<&str> {
        self.address.split_once('/').map(|(machine, _)| machine)
    }

    pub fn is_live(&self) -> bool {
        self.state == "live"
    }

    /// The state of the agent in the terminal, or `None` when the terminal
    /// runs a plain program.
    pub fn agent_state(&self) -> Option<AgentState> {
        let status = self.status.as_ref()?;
        Some(match status.state.as_str() {
            "blocked" => AgentState::Blocked,
            "error" => AgentState::Error,
            "working" => AgentState::Working,
            "done" => AgentState::Done,
            _ => AgentState::Idle,
        })
    }

    /// The folder its program is in now, when the core could read it.
    pub fn current_folder(&self) -> &str {
        match &self.foreground {
            Some(foreground) if !foreground.cwd.is_empty() => &foreground.cwd,
            _ => &self.cwd,
        }
    }

    /// The name of the program in front: `pi`, `zsh`, `cargo`.
    pub fn program(&self) -> &str {
        if let Some(app) = self
            .status
            .as_ref()
            .and_then(|status| status.app.as_deref())
            && !app.is_empty()
        {
            return app;
        }
        if let Some(foreground) = &self.foreground
            && !foreground.name.is_empty()
        {
            return &foreground.name;
        }
        self.argv.first().map_or("", String::as_str)
    }

    /// What to call the terminal: the name it was given, else what its
    /// program or its owner says, else its folder.
    pub fn display_name(&self) -> String {
        if let Some(name) = self.name.as_deref().filter(|name| !name.is_empty()) {
            return name.to_string();
        }
        if let Some(title) = self
            .status
            .as_ref()
            .and_then(|status| status.title.as_deref())
            .filter(|title| !title.is_empty())
        {
            return title.to_string();
        }
        if let Some(place) = self.labels.get("where").filter(|place| !place.is_empty()) {
            return place.clone();
        }
        let folder = self.current_folder();
        let folder = folder
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(folder);
        let program = self.program();
        match (program.is_empty(), folder.is_empty()) {
            (false, false) => format!("{program} · {folder}"),
            (false, true) => program.to_string(),
            (true, false) => folder.to_string(),
            (true, true) => self.address.clone(),
        }
    }
}

/// A machine the core added, and whether it reaches it now.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Machine {
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub target: String,
    /// connecting, connected, needs_login or unreachable.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub why: Option<String>,
}

impl Machine {
    pub fn is_connected(&self) -> bool {
        self.state == "connected"
    }
}

/// A ready-made role of the core: a command and its instructions.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// How the viewer's terminal looks, so the core can answer the queries of a
/// program before any viewer does.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Display {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub cursor: Option<String>,
    pub cell: Option<[u32; 2]>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttachParams {
    pub terminal: String,
    pub cols: u16,
    pub rows: u16,
    /// Lines of scrollback to paint first.
    pub history: usize,
    pub display: Display,
}

#[derive(Clone, Debug, Serialize)]
pub struct InitialSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunParams {
    /// A program and its arguments, or a template and its task. Empty with
    /// no `line`: the shell of that machine.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Another machine of the core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Replace a lost program with a shell instead of closing the terminal.
    pub keep: bool,
    pub no_parent: bool,
    pub no_notify: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<InitialSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
}

/// Why a terminal stream ended.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct CloseReason {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
}

pub struct Attachment {
    pub terminal: TerminalRecord,
    pub stream: Stream,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Connecting,
    Connected {
        core: CoreInfo,
        version: SharedString,
    },
    Disconnected {
        error: SharedString,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum HerdrEvent {
    Connected,
    Disconnected,
    /// The store fell behind and took a new snapshot: any terminal may have
    /// changed, restarted or gone without an event of its own.
    Resynced,
    Created(String),
    /// A kept terminal has a new shell. Its old streams ended.
    Restarted(String),
    Removed(String),
    /// Something else about a terminal changed: its title, its folder, the
    /// program in front, or its program ended.
    Changed(String),
    StatusChanged {
        address: String,
        previous: Option<AgentState>,
    },
}

struct GlobalHerdrStore(Entity<HerdrStore>);

impl Global for GlobalHerdrStore {}

pub struct HerdrStore {
    state: ConnectionState,
    connection: Option<Arc<Connection>>,
    terminals: BTreeMap<String, TerminalRecord>,
    machines: Vec<Machine>,
    templates: Vec<Template>,
    /// Events up to this sequence are already in the snapshot.
    snapshot_sequence: u64,
    _session: Task<()>,
}

impl EventEmitter<HerdrEvent> for HerdrStore {}

impl HerdrStore {
    /// The store of this application. The first call connects to the core of
    /// this machine, which starts it if needed.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalHerdrStore>() {
            return global.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = Self {
                state: ConnectionState::Connecting,
                connection: None,
                terminals: BTreeMap::default(),
                machines: Vec::new(),
                templates: Vec::new(),
                snapshot_sequence: 0,
                _session: Task::ready(()),
            };
            // A test has no core, and a real process would stall its scheduler.
            if cx.is_test() {
                store.state = ConnectionState::Disconnected {
                    error: "Herdr Core is not used in tests".into(),
                };
            } else {
                store.reconnect(cx);
            }
            store
        });
        cx.set_global(GlobalHerdrStore(store.clone()));
        store
    }

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, ConnectionState::Connected { .. })
    }

    /// The terminals of the core and of its machines, oldest first.
    pub fn terminals(&self) -> Vec<&TerminalRecord> {
        let mut terminals = self.terminals.values().collect::<Vec<_>>();
        terminals.sort_by_key(|terminal| terminal.created_ms);
        terminals
    }

    pub fn terminal(&self, address: &str) -> Option<&TerminalRecord> {
        self.terminals.get(address)
    }

    pub fn machines(&self) -> &[Machine] {
        &self.machines
    }

    pub fn templates(&self) -> &[Template] {
        &self.templates
    }

    /// Drops the connection and connects again, now.
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection = None;
        self.state = ConnectionState::Connecting;
        cx.notify();
        self._session = cx.spawn(async move |this, cx| {
            let mut delay = FIRST_RECONNECT_DELAY;
            loop {
                let started = std::time::Instant::now();
                let error = match Self::run_session(&this, cx).await {
                    Ok(()) => return,
                    Err(error) => error,
                };
                log::info!("Herdr Core session ended: {error:#}");
                let updated = this.update(cx, |this, cx| {
                    this.connection = None;
                    this.state = ConnectionState::Disconnected {
                        error: format!("{error:#}").into(),
                    };
                    cx.emit(HerdrEvent::Disconnected);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
                if started.elapsed() > LONGEST_RECONNECT_DELAY {
                    delay = FIRST_RECONNECT_DELAY;
                }
                cx.background_executor().timer(delay).await;
                delay = (delay * 2).min(LONGEST_RECONNECT_DELAY);
            }
        });
    }

    /// Runs one connection until it ends. `Ok` means the store is gone.
    async fn run_session(this: &gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp) -> Result<()> {
        let executor = cx.background_executor().clone();
        let (connection, hello) = Connection::connect(&executor).await?;
        let (snapshot, mut events) = connection.subscribe().await?;
        let machines = connection
            .request::<MachineList>("machine.list", json!({}))
            .await
            .log_err()
            .unwrap_or_default();
        let templates = connection
            .request::<TemplateList>("templates.list", json!({}))
            .await
            .log_err()
            .unwrap_or_default();

        let connected = this.update(cx, |this, cx| {
            this.connection = Some(connection.clone());
            this.state = ConnectionState::Connected {
                core: hello.core.clone(),
                version: hello.version.clone().into(),
            };
            this.replace_terminals(&snapshot);
            this.machines = machines.machines;
            this.templates = templates.templates;
            cx.emit(HerdrEvent::Connected);
            cx.notify();
        });
        if connected.is_err() {
            return Ok(());
        }

        let mut pending = Vec::new();
        loop {
            let mut poll = executor.timer(MACHINE_POLL_INTERVAL).fuse();
            let event = select_biased! {
                event = events.next().fuse() => Some(event),
                _ = poll => None,
            };
            match event {
                Some(Some(StreamEvent::Data(bytes))) => {
                    events.consumed(bytes.len());
                    pending.extend_from_slice(&bytes);
                    let mut notifications = Vec::new();
                    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                        let line = pending.drain(..=end).collect::<Vec<_>>();
                        if let Some(notification) = serde_json::from_slice::<Value>(&line)
                            .context("the core sent an event that is not JSON")
                            .log_err()
                        {
                            notifications.push(notification);
                        }
                    }
                    let applied = this.update(cx, |this, cx| {
                        for notification in &notifications {
                            this.apply_notification(notification, cx);
                        }
                    });
                    if applied.is_err() {
                        return Ok(());
                    }
                }
                Some(Some(StreamEvent::Resize { .. })) => {}
                Some(Some(StreamEvent::Closed(_))) | Some(None) => {
                    return Err(anyhow!(
                        "{}",
                        connection
                            .closed()
                            .unwrap_or_else(|| "the core ended the event stream".to_string())
                    ));
                }
                None => {
                    // The core sends no event when the line to a machine changes.
                    let machines = connection
                        .request::<MachineList>("machine.list", json!({}))
                        .await?;
                    let updated = this.update(cx, |this, cx| {
                        if this.machines != machines.machines {
                            this.machines = machines.machines;
                            cx.notify();
                        }
                    });
                    if updated.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn replace_terminals(&mut self, snapshot: &Value) {
        self.snapshot_sequence = snapshot.get("seq").and_then(Value::as_u64).unwrap_or(0);
        self.terminals.clear();
        let Some(terminals) = snapshot.get("terminals").and_then(Value::as_array) else {
            return;
        };
        for terminal in terminals {
            if let Some(terminal) = parse_terminal(terminal) {
                self.terminals.insert(terminal.address.clone(), terminal);
            }
        }
    }

    fn apply_notification(&mut self, notification: &Value, cx: &mut Context<Self>) {
        let Some(params) = notification.get("params") else {
            return;
        };
        match notification.get("method").and_then(Value::as_str) {
            Some("missed") => {
                self.replace_terminals(params);
                cx.emit(HerdrEvent::Resynced);
                cx.notify();
            }
            Some("event") => {
                let sequence = params
                    .get("seq")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX);
                if sequence <= self.snapshot_sequence {
                    return;
                }
                for event in apply_event(&mut self.terminals, params) {
                    cx.emit(event);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn connection(&self) -> Result<Arc<Connection>> {
        self.connection
            .clone()
            .context("not connected to Herdr Core")
    }

    fn request<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        method: &'static str,
        params: Value,
        cx: &App,
    ) -> Task<Result<T>> {
        match self.connection() {
            Ok(connection) => {
                cx.background_spawn(async move { connection.request::<T>(method, params).await })
            }
            Err(error) => Task::ready(Err(error)),
        }
    }

    /// Starts a program in a new terminal of the core.
    pub fn run(&self, params: RunParams, cx: &App) -> Task<Result<TerminalRecord>> {
        match serde_json::to_value(&params) {
            Ok(params) => self.request("terminal.run", params, cx),
            Err(error) => Task::ready(Err(error.into())),
        }
    }

    /// Ends the program of a terminal.
    pub fn kill(&self, address: &str, cx: &App) -> Task<Result<()>> {
        let request = self.request::<Value>("terminal.kill", json!({"terminal": address}), cx);
        cx.background_spawn(async move { request.await.map(|_| ()) })
    }

    /// Forgets a finished terminal.
    pub fn remove(&self, address: &str, cx: &App) -> Task<Result<()>> {
        let request = self.request::<Value>("terminal.rm", json!({"terminal": address}), cx);
        cx.background_spawn(async move { request.await.map(|_| ()) })
    }

    /// Opens the stream of a live terminal: its screen, then what it prints.
    pub fn attach(&self, params: AttachParams, cx: &App) -> Task<Result<Attachment>> {
        match self.connection() {
            Ok(connection) => cx.background_spawn(async move { connection.attach(params).await }),
            Err(error) => Task::ready(Err(error)),
        }
    }
}

#[derive(Default, Deserialize)]
struct MachineList {
    #[serde(default)]
    machines: Vec<Machine>,
}

#[derive(Default, Deserialize)]
struct TemplateList {
    #[serde(default)]
    templates: Vec<Template>,
}

fn parse_terminal(value: &Value) -> Option<TerminalRecord> {
    serde_json::from_value(value.clone())
        .context("the core sent a terminal this client cannot read")
        .log_err()
}

fn set<T: serde::de::DeserializeOwned>(target: &mut T, data: &Value, key: &str) {
    if let Some(value) = data.get(key)
        && let Some(value) = serde_json::from_value(value.clone()).log_err()
    {
        *target = value;
    }
}

/// Applies one event of the core to the terminals, as the core's API
/// document says each one changes a snapshot.
fn apply_event(
    terminals: &mut BTreeMap<String, TerminalRecord>,
    params: &Value,
) -> Vec<HerdrEvent> {
    let Some(kind) = params.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(address) = params.get("terminal").and_then(Value::as_str) else {
        return Vec::new();
    };
    let null = Value::Null;
    let data = params.get("data").unwrap_or(&null);

    match kind {
        "created" | "restarted" => {
            let Some(terminal) = parse_terminal(data) else {
                return Vec::new();
            };
            terminals.insert(address.to_string(), terminal);
            return vec![if kind == "created" {
                HerdrEvent::Created(address.to_string())
            } else {
                HerdrEvent::Restarted(address.to_string())
            }];
        }
        "removed" => {
            terminals.remove(address);
            return vec![HerdrEvent::Removed(address.to_string())];
        }
        "labels" => {
            if let Some(record) = data.get("record")
                && let Some(terminal) = parse_terminal(record)
            {
                terminals.insert(address.to_string(), terminal);
                return vec![HerdrEvent::Created(address.to_string())];
            }
        }
        _ => {}
    }

    let Some(terminal) = terminals.get_mut(address) else {
        return Vec::new();
    };
    match kind {
        "lost" => {
            set(&mut terminal.reason, data, "reason");
            set(&mut terminal.exit_code, data, "exit_code");
            terminal.pid = None;
            terminal.foreground = None;
        }
        "finished" => {
            terminal.state = "finished".to_string();
            set(&mut terminal.reason, data, "reason");
            set(&mut terminal.exit_code, data, "exit_code");
            set(&mut terminal.finished_ms, data, "finished_ms");
            terminal.foreground = None;
        }
        "keep" => set(&mut terminal.keep, data, "keep"),
        "status" => {
            let previous = terminal.agent_state();
            set(&mut terminal.status, data, "status");
            set(&mut terminal.reported, data, "reported");
            if terminal.agent_state() != previous {
                return vec![HerdrEvent::StatusChanged {
                    address: address.to_string(),
                    previous,
                }];
            }
        }
        "turn" => set(&mut terminal.turn, data, "turn"),
        "title" => set(&mut terminal.title, data, "title"),
        "labels" => set(&mut terminal.labels, data, "labels"),
        "size" => {
            set(&mut terminal.cols, data, "cols");
            set(&mut terminal.rows, data, "rows");
            return Vec::new();
        }
        "foreground" => {
            terminal.foreground = serde_json::from_value(data.clone()).ok().flatten();
        }
        _ => return Vec::new(),
    }
    vec![HerdrEvent::Changed(address.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, terminal: &str, data: Value) -> Value {
        json!({"seq": 1, "type": kind, "terminal": terminal, "data": data})
    }

    #[test]
    fn events_change_the_snapshot() {
        let mut terminals = BTreeMap::default();
        let created = apply_event(
            &mut terminals,
            &event(
                "created",
                "dgx/t7",
                json!({"address": "dgx/t7", "argv": ["pi"], "cwd": "/srv/app", "state": "live",
                       "labels": {}, "created_ms": 5, "cols": 80, "rows": 24, "screen": "exact"}),
            ),
        );
        assert_eq!(created, vec![HerdrEvent::Created("dgx/t7".to_string())]);
        let terminal = &terminals["dgx/t7"];
        assert_eq!(terminal.machine(), Some("dgx"));
        assert_eq!(terminal.agent_state(), None);
        assert_eq!(terminal.display_name(), "pi · app");

        let changed = apply_event(
            &mut terminals,
            &event(
                "status",
                "dgx/t7",
                json!({"status": {"state": "blocked", "app": "pi", "kind": "question"},
                       "records": {}, "reported": true}),
            ),
        );
        assert_eq!(
            changed,
            vec![HerdrEvent::StatusChanged {
                address: "dgx/t7".to_string(),
                previous: None
            }]
        );
        assert_eq!(terminals["dgx/t7"].agent_state(), Some(AgentState::Blocked));

        apply_event(
            &mut terminals,
            &event(
                "foreground",
                "dgx/t7",
                json!({"pid": 9, "name": "node", "argv": ["node"], "cwd": "/srv/api"}),
            ),
        );
        assert_eq!(terminals["dgx/t7"].current_folder(), "/srv/api");
        apply_event(
            &mut terminals,
            &event("size", "dgx/t7", json!({"cols": 100, "rows": 30})),
        );
        assert_eq!(
            (terminals["dgx/t7"].cols, terminals["dgx/t7"].rows),
            (100, 30)
        );

        apply_event(
            &mut terminals,
            &event(
                "status",
                "dgx/t7",
                json!({"status": null, "records": {}, "reported": true}),
            ),
        );
        apply_event(
            &mut terminals,
            &event(
                "finished",
                "dgx/t7",
                json!({"reason": "exited", "exit_code": 0, "signal": null, "finished_ms": 9}),
            ),
        );
        let terminal = &terminals["dgx/t7"];
        assert!(!terminal.is_live());
        assert_eq!(terminal.agent_state(), None);
        assert_eq!(terminal.exit_code, Some(0));
        assert_eq!(terminal.foreground, None);

        let removed = apply_event(&mut terminals, &event("removed", "dgx/t7", json!({})));
        assert_eq!(removed, vec![HerdrEvent::Removed("dgx/t7".to_string())]);
        assert!(terminals.is_empty());
    }

    #[test]
    fn an_event_for_an_unknown_terminal_changes_nothing() {
        let mut terminals = BTreeMap::default();
        assert!(
            apply_event(&mut terminals, &event("title", "t9", json!({"title": "x"}))).is_empty()
        );
        assert!(terminals.is_empty());
    }
}
