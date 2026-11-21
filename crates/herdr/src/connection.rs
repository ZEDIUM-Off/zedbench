//! The connection to a Herdr Core: JSON-RPC 2.0 over the standard streams of
//! `hc connect`, in framed mode so that terminal streams share the connection.
//!
//! A frame is `[channel u32 LE][kind u8][length u32 LE][payload]`. Channel 0
//! carries the JSON-RPC messages; every other channel is a stream the core
//! opened in answer to a call.

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    io::BufReader,
};
use gpui::{BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use util::ResultExt as _;

use crate::{AttachParams, Attachment, CloseReason, Hello, TerminalRecord};

const PROTOCOL_GENERATION: u32 = 1;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const CONTROL_CHANNEL: u32 = 0;

const KIND_DATA: u8 = 0;
const KIND_CREDIT: u8 = 1;
const KIND_CLOSE: u8 = 2;
const KIND_RESIZE: u8 = 3;
const KIND_FOCUS: u8 = 4;
const KIND_FACTS: u8 = 5;
const KIND_AUTOMATIC_INPUT: u8 = 6;

struct Frame {
    channel: u32,
    kind: u8,
    payload: Vec<u8>,
}

fn encode_frame(frame: &Frame) -> Result<Vec<u8>> {
    let length = u32::try_from(frame.payload.len()).context("frame payload too large")?;
    let mut bytes = Vec::with_capacity(9 + frame.payload.len());
    bytes.extend_from_slice(&frame.channel.to_le_bytes());
    bytes.push(frame.kind);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&frame.payload);
    Ok(bytes)
}

fn decode_header(header: &[u8; 9]) -> (u32, u8, usize) {
    let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let length = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
    (channel, header[4], length as usize)
}

/// What the core sends on a stream.
#[derive(Debug)]
pub enum StreamEvent {
    Data(Vec<u8>),
    /// The grid of the terminal changed; apply it before the data that follows.
    Resize {
        columns: u16,
        rows: u16,
    },
    /// The stream is over. A reason is given when the program ended.
    Closed(Option<CloseReason>),
}

/// The error of a call, with the stable `kind` the core gives it.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub kind: Option<String>,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Some(kind) if kind != &self.message => write!(formatter, "{} ({kind})", self.message),
            _ => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for RpcError {}

type Reply = Result<(Value, Option<mpsc::UnboundedReceiver<StreamEvent>>)>;

struct PendingCall {
    reply: oneshot::Sender<Reply>,
    /// The result names a channel the core is about to send on. The reader
    /// registers it before it reads the next frame, so no frame is lost.
    opens_stream: bool,
}

#[derive(Default)]
struct Shared {
    pending: HashMap<u64, PendingCall>,
    streams: HashMap<u32, mpsc::UnboundedSender<StreamEvent>>,
    closed: Option<String>,
}

pub struct Connection {
    outgoing: mpsc::UnboundedSender<Frame>,
    shared: Arc<Mutex<Shared>>,
    next_id: AtomicU64,
    child: Mutex<util::process::Child>,
    _tasks: Vec<Task<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.child.lock().kill().log_err();
    }
}

fn hc_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(if cfg!(windows) { "hc.exe" } else { "hc" })];
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let name = if cfg!(windows) { "hc.exe" } else { "hc" };
        candidates.push(PathBuf::from(home).join(".local").join("bin").join(name));
    }
    candidates
}

fn spawn_hc() -> Result<util::process::Child> {
    let mut last_error = None;
    for candidate in hc_candidates() {
        let mut command = util::command::new_std_command(&candidate);
        command.arg("connect");
        match util::process::Child::spawn(command, Stdio::piped(), Stdio::piped(), Stdio::piped()) {
            Ok(child) => return Ok(child),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no hc command to try")))
        .context("Herdr Core (hc) is not installed or not on the PATH")
}

impl Connection {
    /// Starts `hc connect`, which starts the core of this machine if needed,
    /// and switches the connection to framed mode.
    pub async fn connect(executor: &BackgroundExecutor) -> Result<(Arc<Self>, Hello)> {
        let mut child = spawn_hc()?;
        let mut stdin = child.stdin.take().context("hc connect has no stdin")?;
        let stdout = child.stdout.take().context("hc connect has no stdout")?;
        let stderr = child.stderr.take().context("hc connect has no stderr")?;
        let mut stdout = BufReader::new(stdout);

        let hello = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "hello",
            "params": {"generation": PROTOCOL_GENERATION, "framed": true},
        });
        let mut line = serde_json::to_vec(&hello)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;

        let mut reply = String::new();
        if stdout.read_line(&mut reply).await? == 0 {
            let mut diagnostics = String::new();
            BufReader::new(stderr)
                .read_line(&mut diagnostics)
                .await
                .log_err();
            bail!("hc connect closed before its hello: {}", diagnostics.trim());
        }
        let reply: Value = serde_json::from_str(&reply).context("hc hello is not JSON")?;
        if let Some(error) = reply.get("error") {
            bail!("hc refused the hello: {}", rpc_error(error));
        }
        let hello: Hello = serde_json::from_value(
            reply
                .get("result")
                .cloned()
                .context("hc hello has no result")?,
        )
        .context("unexpected hc hello")?;
        if !hello.framed {
            bail!("hc {} did not switch to framed mode", hello.version);
        }

        let shared = Arc::new(Mutex::new(Shared::default()));
        let (outgoing, mut outgoing_receiver) = mpsc::unbounded::<Frame>();

        let writer = executor.spawn(async move {
            while let Some(frame) = outgoing_receiver.next().await {
                let Some(bytes) = encode_frame(&frame).log_err() else {
                    continue;
                };
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let reader = executor.spawn({
            let shared = shared.clone();
            let outgoing = outgoing.clone();
            async move {
                let reason = match read_frames(&mut stdout, &shared, &outgoing).await {
                    Ok(()) => "hc connect closed the connection".to_string(),
                    Err(error) => format!("{error:#}"),
                };
                let mut shared = shared.lock();
                shared.closed = Some(reason.clone());
                for (_, call) in shared.pending.drain() {
                    call.reply.send(Err(anyhow!("{reason}"))).ok();
                }
                for (_, stream) in shared.streams.drain() {
                    stream.unbounded_send(StreamEvent::Closed(None)).ok();
                }
            }
        });

        let diagnostics = executor.spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                log::warn!("hc connect: {line}");
            }
        });

        Ok((
            Arc::new(Self {
                outgoing,
                shared,
                next_id: AtomicU64::new(1),
                child: Mutex::new(child),
                _tasks: vec![writer, reader, diagnostics],
            }),
            hello,
        ))
    }

    /// Why the connection ended, once it has.
    pub fn closed(&self) -> Option<String> {
        self.shared.lock().closed.clone()
    }

    fn call(
        &self,
        method: &str,
        params: Value,
        opens_stream: bool,
    ) -> impl Future<Output = Reply> + use<> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, receiver) = oneshot::channel();
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let sent = (|| {
            let payload = serde_json::to_vec(&message)?;
            let mut shared = self.shared.lock();
            if let Some(reason) = &shared.closed {
                bail!("{reason}");
            }
            shared.pending.insert(
                id,
                PendingCall {
                    reply,
                    opens_stream,
                },
            );
            drop(shared);
            self.outgoing
                .unbounded_send(Frame {
                    channel: CONTROL_CHANNEL,
                    kind: KIND_DATA,
                    payload,
                })
                .map_err(|_| anyhow!("the connection to Herdr Core is closed"))
        })();
        let method = method.to_string();
        async move {
            sent?;
            receiver
                .await
                .map_err(|_| anyhow!("the connection to Herdr Core closed during {method}"))?
        }
    }

    pub fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<T>> + use<T> {
        let call = self.call(method, params, false);
        let method = method.to_string();
        async move {
            let (value, _) = call.await?;
            serde_json::from_value(value).with_context(|| format!("unexpected result of {method}"))
        }
    }

    /// Subscribes to every change of the core. The stream carries JSON lines.
    pub fn subscribe(self: &Arc<Self>) -> impl Future<Output = Result<(Value, Stream)>> + use<> {
        let call = self.call("events.subscribe", json!({}), true);
        let this = self.clone();
        async move {
            let (value, receiver) = call.await?;
            let channel = stream_channel(&value)?;
            let receiver = receiver.context("events.subscribe opened no stream")?;
            Ok((
                value,
                Stream {
                    channel,
                    receiver,
                    connection: this,
                    automatic_input: false,
                },
            ))
        }
    }

    pub fn attach(
        self: &Arc<Self>,
        params: AttachParams,
    ) -> impl Future<Output = Result<Attachment>> + use<> {
        let call =
            serde_json::to_value(&params).map(|params| self.call("terminal.attach", params, true));
        let this = self.clone();
        async move {
            let (value, receiver) = call?.await?;
            let channel = stream_channel(&value)?;
            let receiver = receiver.context("terminal.attach opened no stream")?;
            let automatic_input = value
                .get("automatic_input")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let terminal: TerminalRecord = serde_json::from_value(
                value
                    .get("terminal")
                    .cloned()
                    .context("terminal.attach returned no terminal")?,
            )?;
            Ok(Attachment {
                terminal,
                stream: Stream {
                    channel,
                    receiver,
                    connection: this,
                    automatic_input,
                },
            })
        }
    }

    fn send_frame(&self, channel: u32, kind: u8, payload: Vec<u8>) {
        // A closed connection ends every stream, which is how callers learn of it.
        self.outgoing
            .unbounded_send(Frame {
                channel,
                kind,
                payload,
            })
            .ok();
    }
}

fn stream_channel(result: &Value) -> Result<u32> {
    let channel = result
        .get("channel")
        .and_then(Value::as_u64)
        .context("the core named no channel")?;
    u32::try_from(channel).context("channel out of range")
}

fn rpc_error(error: &Value) -> RpcError {
    RpcError {
        kind: error
            .pointer("/data/kind")
            .and_then(Value::as_str)
            .map(str::to_string),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string(),
    }
}

async fn read_frames(
    stdout: &mut BufReader<impl futures::AsyncRead + Unpin>,
    shared: &Mutex<Shared>,
    outgoing: &mpsc::UnboundedSender<Frame>,
) -> Result<()> {
    loop {
        let mut header = [0u8; 9];
        match stdout.read_exact(&mut header).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let (channel, kind, length) = decode_header(&header);
        if length > MAX_PAYLOAD {
            bail!("hc sent a frame of {length} bytes");
        }
        let mut payload = vec![0u8; length];
        stdout.read_exact(&mut payload).await?;

        if channel == CONTROL_CHANNEL {
            if kind == KIND_DATA {
                handle_control_message(&payload, shared);
            }
            continue;
        }

        let mut shared = shared.lock();
        match kind {
            KIND_DATA => {
                let delivered = shared.streams.get(&channel).is_some_and(|stream| {
                    stream.unbounded_send(StreamEvent::Data(payload)).is_ok()
                });
                if !delivered {
                    // Nobody reads this stream any more: tell the core to stop sending.
                    shared.streams.remove(&channel);
                    outgoing
                        .unbounded_send(Frame {
                            channel,
                            kind: KIND_CLOSE,
                            payload: Vec::new(),
                        })
                        .ok();
                }
            }
            KIND_RESIZE if payload.len() == 4 => {
                if let Some(stream) = shared.streams.get(&channel) {
                    stream
                        .unbounded_send(StreamEvent::Resize {
                            columns: u16::from_le_bytes([payload[0], payload[1]]),
                            rows: u16::from_le_bytes([payload[2], payload[3]]),
                        })
                        .ok();
                }
            }
            KIND_CLOSE => {
                if let Some(stream) = shared.streams.remove(&channel) {
                    let reason = serde_json::from_slice::<CloseReason>(&payload).ok();
                    stream.unbounded_send(StreamEvent::Closed(reason)).ok();
                }
            }
            _ => {}
        }
    }
}

fn handle_control_message(payload: &[u8], shared: &Mutex<Shared>) {
    let Some(message) = serde_json::from_slice::<Value>(payload)
        .context("hc sent a control message that is not JSON")
        .log_err()
    else {
        return;
    };
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let mut shared = shared.lock();
    let Some(call) = shared.pending.remove(&id) else {
        return;
    };
    let reply = if let Some(error) = message.get("error") {
        Err(anyhow::Error::new(rpc_error(error)))
    } else {
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        let receiver = if call.opens_stream {
            stream_channel(&result).ok().map(|channel| {
                let (sender, receiver) = mpsc::unbounded();
                shared.streams.insert(channel, sender);
                receiver
            })
        } else {
            None
        };
        Ok((result, receiver))
    };
    call.reply.send(reply).ok();
}

/// A stream the core opened: the events of the core, or the bytes of a terminal.
pub struct Stream {
    channel: u32,
    receiver: mpsc::UnboundedReceiver<StreamEvent>,
    connection: Arc<Connection>,
    automatic_input: bool,
}

impl Stream {
    pub async fn next(&mut self) -> Option<StreamEvent> {
        self.receiver.next().await
    }

    /// Tells the core that `bytes` bytes were consumed. The core stops sending
    /// to a stream that falls 256 KiB behind.
    pub fn consumed(&self, bytes: usize) {
        if let Ok(bytes) = u32::try_from(bytes) {
            self.connection
                .send_frame(self.channel, KIND_CREDIT, bytes.to_le_bytes().to_vec());
        }
    }

    pub fn handle(&self) -> StreamHandle {
        StreamHandle {
            channel: self.channel,
            connection: self.connection.clone(),
            automatic_input: self.automatic_input,
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let was_open = self
            .connection
            .shared
            .lock()
            .streams
            .remove(&self.channel)
            .is_some();
        if was_open {
            self.connection
                .send_frame(self.channel, KIND_CLOSE, Vec::new());
        }
    }
}

/// The viewer's side of a terminal stream: what it sends to the core.
#[derive(Clone)]
pub struct StreamHandle {
    channel: u32,
    connection: Arc<Connection>,
    automatic_input: bool,
}

impl StreamHandle {
    /// Keys and pasted text from a person.
    pub fn input(&self, bytes: Vec<u8>) {
        self.connection.send_frame(self.channel, KIND_DATA, bytes);
    }

    /// An answer the terminal emulator gives to a query of the program. The
    /// core does not count it as activity of a person.
    pub fn automatic_input(&self, bytes: Vec<u8>) {
        let kind = if self.automatic_input {
            KIND_AUTOMATIC_INPUT
        } else {
            KIND_DATA
        };
        self.connection.send_frame(self.channel, kind, bytes);
    }

    pub fn resize(&self, columns: u16, rows: u16) {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&columns.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        self.connection
            .send_frame(self.channel, KIND_RESIZE, payload);
    }

    pub fn focus(&self, focused: bool) {
        self.connection
            .send_frame(self.channel, KIND_FOCUS, vec![u8::from(focused)]);
    }

    pub fn display(&self, display: &crate::Display) {
        if let Some(payload) = serde_json::to_vec(display).log_err() {
            self.connection
                .send_frame(self.channel, KIND_FACTS, payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let bytes = encode_frame(&Frame {
            channel: 7,
            kind: KIND_RESIZE,
            payload: vec![100, 0, 30, 0],
        })
        .expect("a small frame encodes");
        assert_eq!(bytes.len(), 13);
        let header: [u8; 9] = bytes[..9].try_into().expect("nine header bytes");
        assert_eq!(decode_header(&header), (7, KIND_RESIZE, 4));
    }

    #[test]
    fn a_reply_registers_its_stream_before_the_next_frame() {
        let shared = Mutex::new(Shared::default());
        let (reply, mut receiver) = oneshot::channel();
        shared.lock().pending.insert(
            3,
            PendingCall {
                reply,
                opens_stream: true,
            },
        );
        handle_control_message(
            br#"{"jsonrpc":"2.0","id":3,"result":{"channel":5}}"#,
            &shared,
        );
        assert!(shared.lock().streams.contains_key(&5));
        let (value, stream) = receiver
            .try_recv()
            .expect("the reply is not cancelled")
            .expect("the reply is sent")
            .expect("the call succeeded");
        assert_eq!(value["channel"], 5);
        assert!(stream.is_some());
    }

    #[test]
    fn an_error_keeps_its_kind() {
        let shared = Mutex::new(Shared::default());
        let (reply, mut receiver) = oneshot::channel();
        shared.lock().pending.insert(
            1,
            PendingCall {
                reply,
                opens_stream: false,
            },
        );
        handle_control_message(
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"no terminal t9","data":{"kind":"not_found"}}}"#,
            &shared,
        );
        let error = receiver
            .try_recv()
            .expect("the reply is not cancelled")
            .expect("the reply is sent")
            .expect_err("the call failed");
        let error = error.downcast::<RpcError>().expect("an RPC error");
        assert_eq!(error.kind.as_deref(), Some("not_found"));
    }
}
