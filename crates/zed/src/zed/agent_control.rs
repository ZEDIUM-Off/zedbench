//! On-demand control of the running Zedbench by an agent, through the CLI.
//!
//! Commands reach the app over the channel the `zed` CLI already uses to open
//! files, so Zedbench never listens on a port for them. Every command other than
//! `connect` fails until a session is open, and the session ends on `close`, after
//! [`IDLE_TIMEOUT`] without commands, or when Zedbench quits.

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{
    AnyWindowHandle, App, Global, Keystroke, Task,
    accesskit::{self, NodeId},
};
use serde_json::Value;
use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};
use workspace::{MultiWorkspace, Toast, notifications::NotificationId};

const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const USAGE: &str = "\
usage: zedbench --agent [-w WINDOW] COMMAND
  connect                 open a session (ends after 15 min without commands)
  close                   end the session
  status                  show whether a session is open
  windows                 list windows: index, * for the active one, project roots
  actions [--all] [TEXT]  actions available in the focused context, with key bindings
  run ACTION [JSON]       dispatch ACTION to the focused element
  press KEYS              type key strokes, such as `ctrl-shift-p` or `ctrl-k ctrl-s`
  snapshot [-i]           the window as an accessibility tree; -i keeps interactive nodes
  click REF               click a node from the last snapshot, such as `e12`
  focus REF               focus a node from the last snapshot
  hover REF               move the pointer over a node from the last snapshot
  menu REF                open the context menu of a node from the last snapshot";

struct AgentSession {
    started: Instant,
    commands: usize,
    /// The nodes of the last snapshot, so that `e3` names the fourth one.
    refs: Vec<(AnyWindowHandle, NodeId)>,
    _idle_timeout: Task<()>,
}

impl Global for AgentSession {}

/// Runs one agent command and returns what to print on its standard output.
pub fn run(args: Vec<String>, cx: &mut App) -> Result<String> {
    let mut args = args.into_iter().peekable();
    let mut window_index = None;
    if args
        .peek()
        .is_some_and(|arg| arg == "-w" || arg == "--window")
    {
        args.next();
        let index = args.next().context("-w expects a window index")?;
        window_index = Some(
            index
                .parse::<usize>()
                .with_context(|| format!("invalid window index {index:?}"))?,
        );
    }
    let command = args.next().unwrap_or_default();
    let rest = args.collect::<Vec<_>>();

    match command.as_str() {
        "connect" => return Ok(connect(cx)),
        "status" => return Ok(status(cx)),
        "close" => {
            return Ok(if end_session("closed by the agent", cx) {
                "closed".into()
            } else {
                "no session".into()
            });
        }
        "" | "help" | "--help" | "-h" => return Ok(USAGE.into()),
        _ => {}
    }

    touch_session(cx)?;
    match command.as_str() {
        "windows" => Ok(list_windows(cx)),
        "actions" => {
            let all = rest.first().is_some_and(|arg| arg == "--all");
            let filter = rest.get(usize::from(all)).map(|text| text.to_lowercase());
            list_actions(target_window(window_index, cx)?, all, filter, cx)
        }
        "run" => {
            let name = rest.first().context("run expects an action name")?;
            let data = rest
                .get(1)
                .map(|json| serde_json::from_str(json))
                .transpose()
                .context("the action argument must be JSON")?;
            run_action(target_window(window_index, cx)?, name, data, cx)
        }
        "press" => {
            anyhow::ensure!(!rest.is_empty(), "press expects key strokes");
            press(target_window(window_index, cx)?, &rest.join(" "), cx)
        }
        "snapshot" => {
            let interactive = rest.first().is_some_and(|arg| arg == "-i");
            snapshot(target_window(window_index, cx)?, interactive, cx)
        }
        "click" => perform(rest.first(), accesskit::Action::Click, cx),
        "focus" => perform(rest.first(), accesskit::Action::Focus, cx),
        "hover" => perform(rest.first(), accesskit::Action::ShowTooltip, cx),
        "menu" => perform(rest.first(), accesskit::Action::ShowContextMenu, cx),
        _ => Err(anyhow!("unknown command {command:?}\n{USAGE}")),
    }
}

fn connect(cx: &mut App) -> String {
    let resumed = cx.has_global::<AgentSession>();
    let commands = cx
        .try_global::<AgentSession>()
        .map_or(0, |session| session.commands);
    let started = cx
        .try_global::<AgentSession>()
        .map_or_else(Instant::now, |session| session.started);
    let timeout = idle_timeout(cx);
    cx.set_global(AgentSession {
        started,
        commands,
        refs: Vec::new(),
        _idle_timeout: timeout,
    });
    set_inspection(true, cx);
    if resumed {
        "connected (existing session)".into()
    } else {
        notify(
            "An agent is controlling Zedbench. The session ends after 15 minutes without commands.",
            cx,
        );
        format!(
            "connected; idle timeout {} min",
            IDLE_TIMEOUT.as_secs() / 60
        )
    }
}

fn status(cx: &App) -> String {
    match cx.try_global::<AgentSession>() {
        Some(session) => format!(
            "connected for {} s, {} commands",
            session.started.elapsed().as_secs(),
            session.commands
        ),
        None => "no session".into(),
    }
}

fn touch_session(cx: &mut App) -> Result<()> {
    if !cx.has_global::<AgentSession>() {
        bail!("no agent session: run `--agent connect` first");
    }
    // Replacing the task cancels the previous countdown.
    let timeout = idle_timeout(cx);
    let session = cx.global_mut::<AgentSession>();
    session.commands += 1;
    session._idle_timeout = timeout;
    Ok(())
}

fn idle_timeout(cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(IDLE_TIMEOUT).await;
        cx.update(|cx| end_session("ended after 15 minutes without commands", cx));
    })
}

fn end_session(reason: &str, cx: &mut App) -> bool {
    if !cx.has_global::<AgentSession>() {
        return false;
    }
    cx.remove_global::<AgentSession>();
    set_inspection(false, cx);
    notify(&format!("The agent session {reason}."), cx);
    true
}

fn notify(message: &str, cx: &mut App) {
    for window in cx.windows() {
        let Some(multi_workspace) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let message = message.to_string();
        multi_workspace
            .update(cx, |multi_workspace, _, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<AgentSession>(), message),
                        cx,
                    );
                });
            })
            .ok();
    }
}

fn list_windows(cx: &App) -> String {
    let active = cx.active_window();
    let mut output = String::new();
    for (index, window) in cx.windows().into_iter().enumerate() {
        let marker = if Some(window) == active { "*" } else { " " };
        let roots = window
            .downcast::<MultiWorkspace>()
            .and_then(|multi_workspace| multi_workspace.read(cx).ok())
            .map(|multi_workspace| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .root_paths(cx)
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        writeln!(output, "{index}{marker} {roots}").ok();
    }
    output.trim_end().to_string()
}

fn target_window(index: Option<usize>, cx: &App) -> Result<AnyWindowHandle> {
    let windows = cx.windows();
    match index {
        Some(index) => windows
            .get(index)
            .copied()
            .with_context(|| format!("no window {index}; see `windows`")),
        None => cx
            .active_window()
            .or_else(|| windows.first().copied())
            .context("Zedbench has no window"),
    }
}

fn list_actions(
    window: AnyWindowHandle,
    all: bool,
    filter: Option<String>,
    cx: &mut App,
) -> Result<String> {
    let os = cx.platform_info().os;
    window.update(cx, |_, window, cx| {
        let actions = if all {
            cx.all_action_names()
                .iter()
                .filter_map(|name| cx.build_action(name, None).ok())
                .collect::<Vec<_>>()
        } else {
            window.available_actions(cx)
        };
        let mut lines = actions
            .iter()
            .filter(|action| {
                filter
                    .as_ref()
                    .is_none_or(|filter| action.name().to_lowercase().contains(filter))
            })
            .map(|action| {
                let keys = window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .map(|binding| {
                        binding
                            .keystrokes()
                            .iter()
                            .map(|keystroke| keystroke.unparse(os))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if keys.is_empty() {
                    action.name().to_string()
                } else {
                    format!("{}\t{keys}", action.name())
                }
            })
            .collect::<Vec<_>>();
        lines.sort();
        lines.dedup();
        lines.join("\n")
    })
}

fn run_action(
    window: AnyWindowHandle,
    name: &str,
    data: Option<serde_json::Value>,
    cx: &mut App,
) -> Result<String> {
    let action = cx
        .build_action(name, data)
        .map_err(|error| anyhow!("{error}"))?;
    window.update(cx, |_, window, cx| {
        // Dispatching an action nobody handles succeeds silently, so the agent
        // would believe it worked.
        anyhow::ensure!(
            window.is_action_available(action.as_ref(), cx),
            "no handler for {name} in the focused context; see `actions`"
        );
        window.dispatch_action(action, cx);
        Ok(format!("dispatched {name}"))
    })?
}

fn press(window: AnyWindowHandle, keys: &str, cx: &mut App) -> Result<String> {
    let os = cx.platform_info().os;
    let keystrokes = keys
        .split_whitespace()
        .map(|source| {
            Keystroke::parse_with_os(source, os)
                .map_err(|error| anyhow!("invalid key stroke {source:?}: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    window.update(cx, |_, window, cx| {
        let handled = keystrokes
            .into_iter()
            .map(|keystroke| window.dispatch_keystroke(keystroke, cx))
            .fold(false, |handled, current| handled || current);
        if handled {
            format!("pressed {keys}")
        } else {
            format!("pressed {keys} (not handled)")
        }
    })
}

/// Builds each window's accessibility tree while a session is open, since
/// nothing else does without a screen reader.
fn set_inspection(requested: bool, cx: &mut App) {
    for window in cx.windows() {
        window
            .update(cx, |_, window, _| window.set_a11y_inspection(requested))
            .ok();
    }
}

fn snapshot(window: AnyWindowHandle, interactive: bool, cx: &mut App) -> Result<String> {
    let tree = window.update(cx, |_, window, _| {
        // A window opened during the session has not built its tree yet.
        window.set_a11y_inspection(true);
        window.debug_a11y_tree_json()
    })?;
    let tree = tree.context("the window has not drawn its tree yet; retry")?;
    let tree: Value = serde_json::from_str(&tree).context("parsing the accessibility tree")?;
    let nodes = tree["nodes"].as_object().context("no nodes in the tree")?;
    let root = tree["root"].as_str().context("no root in the tree")?;
    let focus = tree["gpui_focus"].as_str();

    let mut refs = Vec::new();
    let mut output = String::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((key, depth)) = stack.pop() {
        let Some(node) = nodes.get(key) else {
            continue;
        };
        let aria = &node["aria"];
        let actions = aria["on_action"]
            .as_array()
            .map(|actions| actions.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let actionable = actions
            .iter()
            .any(|action| *action == "Click" || *action == "Focus");
        let label = aria["label"].as_str().or(aria["placeholder"].as_str());
        let value = aria["value"].as_str();
        // Unlabeled containers only add indentation, so their children take
        // their place.
        let shown = if interactive {
            actionable
        } else {
            actionable || label.is_some() || value.is_some()
        };
        let child_depth = if shown { depth + 1 } else { depth };
        if shown {
            let id = node["accesskit_id"]
                .as_str()
                .and_then(|id| id.parse().ok())
                .map(NodeId)
                .context("node without an AccessKit id")?;
            let reference = format!("e{}", refs.len());
            refs.push((window, id));
            let role = aria["role"].as_str().unwrap_or("Unknown");
            write!(output, "{}- {role}", "  ".repeat(depth)).ok();
            if let Some(label) = label {
                write!(output, " {label:?}").ok();
            }
            if let Some(value) = value.filter(|value| Some(*value) != label) {
                write!(output, " value={value:?}").ok();
            }
            for state in ["toggled", "selected", "expanded"] {
                if let Some(state_value) = aria.get(state) {
                    write!(output, " {state}={state_value}").ok();
                }
            }
            if focus == Some(key) {
                output.push_str(" focused");
            }
            writeln!(output, " [ref={reference}]").ok();
        }
        if let Some(children) = node["children"].as_array() {
            for child in children.iter().rev().filter_map(Value::as_str) {
                stack.push((child, child_depth));
            }
        }
    }
    if cx.has_global::<AgentSession>() {
        cx.global_mut::<AgentSession>().refs = refs;
    }
    Ok(if output.is_empty() {
        "no nodes; the window exposes no accessible elements".into()
    } else {
        output.trim_end().to_string()
    })
}

fn perform(reference: Option<&String>, action: accesskit::Action, cx: &mut App) -> Result<String> {
    let reference = reference.context("expects a ref from `snapshot`, such as e3")?;
    let index = reference
        .trim_start_matches('@')
        .strip_prefix('e')
        .and_then(|index| index.parse::<usize>().ok())
        .with_context(|| format!("invalid ref {reference:?}"))?;
    let (window, node) = cx
        .try_global::<AgentSession>()
        .and_then(|session| session.refs.get(index).copied())
        .with_context(|| format!("no {reference} in the last snapshot; take a new one"))?;
    window.update(cx, |_, window, cx| {
        window.perform_a11y_action(
            accesskit::ActionRequest {
                action,
                target_tree: accesskit::TreeId::ROOT,
                target_node: node,
                data: None,
            },
            cx,
        );
    })?;
    Ok(format!(
        "{action:?} {reference}; take a new snapshot to check the result"
    ))
}
