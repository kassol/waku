use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

use waku_protocol::MAX_WIRE_MESSAGE_BYTES;
use waku_protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage,
};

const READ_POLL_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

enum Outgoing {
    Message(ClientMessage),
    Shutdown,
}

struct ClientInner {
    outgoing: Sender<Outgoing>,
    pending: Mutex<HashMap<Uuid, Sender<Result<ResponsePayload, RpcError>>>>,
    sessions: Mutex<HashMap<(Uuid, Uuid), Sender<SequencedEvent>>>,
    pending_events: Mutex<HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>>,
    task_state_subscribers: Mutex<Vec<Sender<u64>>>,
    last_sequences: Mutex<HashMap<(Uuid, Uuid), LastSequence>>,
    disconnected: AtomicBool,
    resume_sequences: HashMap<(Uuid, Uuid), LastSequence>,
}

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

impl DaemonClient {
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        Self::connect_with_resume(address, token, Vec::new())
    }

    pub fn connect_with_resume(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
    ) -> anyhow::Result<Self> {
        let last_sequences: HashMap<_, _> = resume_from
            .iter()
            .map(|cursor| {
                (
                    (cursor.session_id, cursor.runtime_id),
                    LastSequence {
                        epoch: cursor.epoch,
                        sequence: cursor.sequence,
                    },
                )
            })
            .collect();
        let url = daemon_url(address)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let (mut socket, _) =
            tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
                .context("could not connect to Waku daemon")?;
        set_client_read_timeout(&mut socket, Some(Duration::from_secs(5)))?;
        write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client_id: Uuid::new_v4(),
                resume_from,
            },
        )?;
        let hello = read_server_message(&mut socket)?;
        match hello {
            ServerMessage::Hello {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => {}
            ServerMessage::Hello {
                protocol_version, ..
            } => bail!(
                "daemon protocol {protocol_version} does not match desktop protocol {PROTOCOL_VERSION}"
            ),
            ServerMessage::Rejected { message } => bail!("daemon rejected connection: {message}"),
            other => bail!("daemon sent an invalid handshake response: {other:?}"),
        }
        set_client_read_timeout(&mut socket, Some(READ_POLL_INTERVAL))?;

        let (outgoing, outgoing_rx) = unbounded();
        let inner = Arc::new(ClientInner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(HashMap::new()),
            task_state_subscribers: Mutex::new(Vec::new()),
            resume_sequences: last_sequences.clone(),
            last_sequences: Mutex::new(last_sequences),
            disconnected: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("waku-daemon-client".into())
            .spawn(move || run_client(socket, outgoing_rx, thread_inner))
            .context("could not start Waku daemon client thread")?;
        Ok(Self { inner })
    }

    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<SequencedEvent> {
        let cursor = self
            .inner
            .resume_sequences
            .get(&(session_id, runtime_id))
            .map(|cursor| waku_protocol::model::RuntimeEventCursor {
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            });
        self.subscribe_after(session_id, runtime_id, cursor)
    }

    /// Register live delivery before filling a missing persisted prefix. The
    /// caller receives one ordered stream, while replay stays off its thread.
    pub fn subscribe_after(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        cursor: Option<waku_protocol::model::RuntimeEventCursor>,
    ) -> Receiver<SequencedEvent> {
        let (live, incoming) = unbounded();
        let (output, receiver) = unbounded();
        let key = (session_id, runtime_id);
        {
            let mut sessions = self.inner.sessions.lock();
            sessions.insert(key, live.clone());
            if let Some(buffered) = self.inner.pending_events.lock().remove(&key) {
                for event in buffered {
                    let _ = live.send(event);
                }
            }
        }
        drop(live);
        let client = self.clone();
        std::thread::spawn(move || {
            let mut applied = cursor.map(|cursor| LastSequence {
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            });
            while let Ok(event) = incoming.recv() {
                let previous = applied
                    .filter(|previous| previous.epoch == event.epoch)
                    .map_or(0, |previous| previous.sequence);
                let persistence = event.event.kind == "historyPersistence";
                let missing_until = if persistence {
                    event.sequence
                } else {
                    event.sequence.saturating_sub(1)
                };
                if missing_until > previous && !event.event.kind.starts_with("terminal") {
                    let mut after = previous;
                    while after < missing_until {
                        let replay = client.request(
                            session_id,
                            runtime_id,
                            Command::ReplayEvents {
                                cursor: ReplayCursor {
                                    session_id,
                                    runtime_id,
                                    epoch: event.epoch,
                                    sequence: after,
                                },
                            },
                        );
                        let page = match replay {
                            Ok(ResponsePayload::EventReplay { events }) => events,
                            Ok(ResponsePayload::HistorySnapshot { session }) => {
                                let Some(saved) = session.history_saved_cursor.filter(|saved| {
                                    session.id == session_id
                                        && saved.runtime_id == runtime_id
                                        && saved.epoch == event.epoch
                                        && saved.sequence > after
                                }) else {
                                    let _ = output.send(SequencedEvent {
                                        event: waku_protocol::WireDriverEvent::new(
                                            "error",
                                            serde_json::json!("invalid history snapshot cursor"),
                                        ),
                                        ..event
                                    });
                                    return;
                                };
                                after = saved.sequence;
                                applied = Some(LastSequence {
                                    epoch: saved.epoch,
                                    sequence: after,
                                });
                                if output
                                    .send(SequencedEvent {
                                        session_id,
                                        runtime_id,
                                        epoch: saved.epoch,
                                        sequence: after,
                                        event: waku_protocol::WireDriverEvent::new(
                                            "historySnapshot",
                                            serde_json::to_value(session)
                                                .expect("session serializes"),
                                        ),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            other => {
                                if client.is_disconnected() {
                                    return;
                                }
                                let error = match other {
                                    Err(error) => error.to_string(),
                                    _ => "daemon returned invalid history replay".into(),
                                };
                                let _ = output.send(SequencedEvent {
                                    event: waku_protocol::WireDriverEvent::new(
                                        "error",
                                        serde_json::json!(error),
                                    ),
                                    ..event
                                });
                                return;
                            }
                        };
                        if page.first().is_none_or(|entry| entry.sequence != after + 1) {
                            let _ = output.send(SequencedEvent {
                                event: waku_protocol::WireDriverEvent::new(
                                    "error",
                                    serde_json::json!("saved history replay is incomplete"),
                                ),
                                ..event
                            });
                            return;
                        }
                        for replayed in page
                            .into_iter()
                            .take_while(|entry| entry.sequence <= missing_until)
                        {
                            after = replayed.sequence;
                            applied = Some(LastSequence {
                                epoch: replayed.epoch,
                                sequence: after,
                            });
                            if output.send(replayed).is_err() {
                                return;
                            }
                        }
                    }
                }
                let previous = applied
                    .filter(|previous| previous.epoch == event.epoch)
                    .map_or(0, |previous| previous.sequence);
                if persistence || event.sequence > previous || event.event.kind == "processExited" {
                    if !persistence && event.sequence > previous {
                        applied = Some(LastSequence {
                            epoch: event.epoch,
                            sequence: event.sequence,
                        });
                    }
                    if output.send(event).is_err() {
                        return;
                    }
                }
            }
        });
        receiver
    }

    /// Whether two handles send through the same WebSocket connection.
    ///
    /// The daemon supervisor publishes replacement clients after a managed
    /// restart. Runtime adapters use this identity check to ignore the
    /// subscription's initial snapshot and wait for an actual replacement.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.disconnected.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self, session_id: Uuid, runtime_id: Uuid) {
        self.inner.sessions.lock().remove(&(session_id, runtime_id));
    }

    pub fn subscribe_task_state(&self) -> Receiver<u64> {
        let (events, receiver) = unbounded();
        self.inner.task_state_subscribers.lock().push(events);
        receiver
    }

    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<ResponsePayload> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        let request_id = Uuid::new_v4();
        let (response, response_rx) = bounded(1);
        self.inner.pending.lock().insert(request_id, response);
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        if self
            .inner
            .outgoing
            .send(Outgoing::Message(message))
            .is_err()
        {
            self.inner.pending.lock().remove(&request_id);
            bail!("Waku daemon connection is closed");
        }
        match response_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(anyhow!(error.message)),
            Err(error) => {
                self.inner.pending.lock().remove(&request_id);
                Err(anyhow!("timed out waiting for Waku daemon: {error}"))
            }
        }
    }

    pub fn notify(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::Request(Request {
                // The nil request id is reserved for fire-and-forget controls;
                // the daemon executes them in the runtime mailbox but does
                // not allocate or send a response.
                request_id: Uuid::nil(),
                session_id,
                runtime_id,
                command,
            })))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn last_sequences(&self) -> Vec<ReplayCursor> {
        self.inner
            .last_sequences
            .lock()
            .iter()
            .map(|(&(session_id, runtime_id), cursor)| ReplayCursor {
                session_id,
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            })
            .collect()
    }

    pub fn shutdown(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Shutdown);
    }
}

fn daemon_url(address: &str) -> anyhow::Result<String> {
    let normalized = if address.starts_with("ws://") || address.starts_with("wss://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let mut url = url::Url::parse(&normalized).context("Waku daemon address is invalid")?;
    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

struct SnapshotTransfer {
    session_id: Uuid,
    cursor: Option<waku_protocol::model::RuntimeEventCursor>,
    replay: bool,
    total_bytes: u64,
    data: String,
}

fn run_client(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    outgoing: Receiver<Outgoing>,
    inner: Arc<ClientInner>,
) {
    let mut snapshots: HashMap<Uuid, SnapshotTransfer> = HashMap::new();
    'connection: loop {
        snapshots.retain(|id, _| inner.pending.lock().contains_key(id));
        while let Ok(message) = outgoing.try_recv() {
            match message {
                Outgoing::Message(message) => {
                    if write_json(&mut socket, &message).is_err() {
                        break 'connection;
                    }
                }
                Outgoing::Shutdown => {
                    let _ = write_json(&mut socket, &ClientMessage::Shutdown);
                    let _ = socket.flush();
                    break 'connection;
                }
            }
        }

        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(message) = serde_json::from_str::<ServerMessage>(text.as_ref()) else {
                    continue;
                };
                match message {
                    ServerMessage::Response {
                        request_id,
                        outcome,
                    } => {
                        if let Some(pending) = inner.pending.lock().remove(&request_id) {
                            let result = match outcome {
                                ResponseOutcome::Ok { payload } => Ok(payload),
                                ResponseOutcome::Error { error } => Err(error),
                            };
                            let _ = pending.send(result);
                        }
                    }
                    ServerMessage::HistorySnapshotChunk {
                        request_id,
                        session_id,
                        cursor,
                        replay,
                        offset,
                        total_bytes,
                        data,
                    } => {
                        if !inner.pending.lock().contains_key(&request_id) {
                            continue;
                        }
                        let transfer =
                            snapshots
                                .entry(request_id)
                                .or_insert_with(|| SnapshotTransfer {
                                    session_id,
                                    cursor,
                                    replay,
                                    total_bytes,
                                    data: String::new(),
                                });
                        let invalid = total_bytes == 0
                            || data.is_empty()
                            || data.len() > MAX_WIRE_MESSAGE_BYTES / 8
                            || transfer.session_id != session_id
                            || transfer.cursor != cursor
                            || transfer.replay != replay
                            || transfer.total_bytes != total_bytes
                            || offset != transfer.data.len() as u64
                            || offset
                                .checked_add(data.len() as u64)
                                .is_none_or(|end| end > total_bytes);
                        if invalid {
                            snapshots.remove(&request_id);
                            if let Some(pending) = inner.pending.lock().remove(&request_id) {
                                let _ = pending.send(Err(RpcError {
                                    message: "invalid history snapshot chunk".into(),
                                }));
                            }
                            continue;
                        }
                        transfer.data.push_str(&data);
                        if transfer.data.len() as u64 == total_bytes {
                            let transfer = snapshots.remove(&request_id).unwrap();
                            let result =
                                serde_json::from_str::<waku_protocol::model::AgentSession>(
                                    &transfer.data,
                                )
                                .map_err(|error| RpcError {
                                    message: format!("invalid history snapshot: {error}"),
                                })
                                .and_then(|session| {
                                    if session.id == session_id
                                        && session.runtime_event_cursor == cursor
                                        && (!replay
                                            || (cursor.is_some()
                                                && session.history_saved_cursor == cursor))
                                    {
                                        Ok(if replay {
                                            ResponsePayload::HistorySnapshot { session }
                                        } else {
                                            ResponsePayload::Session {
                                                session: Some(session),
                                            }
                                        })
                                    } else {
                                        Err(RpcError {
                                            message: "invalid history snapshot identity".into(),
                                        })
                                    }
                                });
                            if let Some(pending) = inner.pending.lock().remove(&request_id) {
                                let _ = pending.send(result);
                            }
                        }
                    }
                    ServerMessage::Event(event) => {
                        let should_deliver = {
                            let mut sequences = inner.last_sequences.lock();
                            let previous = sequences
                                .entry((event.session_id, event.runtime_id))
                                .or_insert(LastSequence {
                                    epoch: event.epoch,
                                    sequence: 0,
                                });
                            if previous.epoch == event.epoch && event.sequence <= previous.sequence
                            {
                                false
                            } else {
                                previous.epoch = event.epoch;
                                previous.sequence = event.sequence;
                                true
                            }
                        };
                        if should_deliver {
                            let key = (event.session_id, event.runtime_id);
                            let sessions = inner.sessions.lock();
                            if let Some(events) = sessions.get(&key) {
                                let _ = events.send(event);
                            } else {
                                let mut pending = inner.pending_events.lock();
                                let buffered = pending.entry(key).or_default();
                                let ephemeral = event.event.kind.starts_with("terminal");
                                buffered.push_back(event);
                                if ephemeral {
                                    while buffered.len() > 4096 {
                                        buffered.pop_front();
                                    }
                                }
                            }
                        }
                    }
                    ServerMessage::HistoryPersistence {
                        session_id,
                        runtime_id,
                        epoch,
                        sequence,
                        error,
                    } => {
                        let saved = error.is_none();
                        let event = SequencedEvent {
                            session_id,
                            runtime_id,
                            epoch,
                            sequence,
                            event: waku_protocol::WireDriverEvent::new(
                                "historyPersistence",
                                serde_json::json!({ "error": error }),
                            ),
                        };
                        let key = (session_id, runtime_id);
                        let sessions = inner.sessions.lock();
                        if let Some(events) = sessions.get(&key) {
                            let _ = events.send(event);
                        } else {
                            let mut pending = inner.pending_events.lock();
                            let buffered = pending.entry(key).or_default();
                            if saved {
                                while buffered.len() >= 4096
                                    && buffered.front().is_some_and(|entry| {
                                        entry.epoch == epoch && entry.sequence <= sequence
                                    })
                                {
                                    buffered.pop_front();
                                }
                            }
                            buffered.push_back(event);
                        }
                    }
                    ServerMessage::TaskStateChanged { revision } => {
                        inner
                            .task_state_subscribers
                            .lock()
                            .retain(|subscriber| subscriber.send(revision).is_ok());
                    }
                    ServerMessage::ShuttingDown => break,
                    ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => {}
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {
                let _ = socket.flush();
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if retryable_io(&error) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(_) => break,
        }
    }

    inner.disconnected.store(true, Ordering::Release);
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, response) in pending {
        let _ = response.send(Err(RpcError {
            message: "Waku daemon disconnected".into(),
        }));
    }
    // Closing the desktop transport is not evidence that a daemon-owned
    // provider exited. Drop the subscription senders so runtime adapters can
    // hand off to a replacement client and ask the daemon whether the same
    // runtime still exists. Real provider exits arrive through the replayable
    // `processExited` event emitted by the daemon.
    drop(std::mem::take(&mut *inner.sessions.lock()));
    inner.task_state_subscribers.lock().clear();
}

fn set_client_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Option<Duration>,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

fn retryable_io(error: &io::Error) -> bool {
    retryable_error(error)
}

fn retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            return true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return true;
        }
    }
    error.source().is_some_and(retryable_error)
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(value)?;
    socket.send(Message::Text(payload.into()))?;
    Ok(())
}

fn read_server_message(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<ServerMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("Waku daemon closed during handshake"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_endpoint_accepts_addresses_and_secure_urls() {
        assert_eq!(
            daemon_url("127.0.0.1:4312").unwrap(),
            "ws://127.0.0.1:4312/v1"
        );
        assert_eq!(
            daemon_url("wss://waku.example.test/old?ignored=1").unwrap(),
            "wss://waku.example.test/v1"
        );
    }
}
