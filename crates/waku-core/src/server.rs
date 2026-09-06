use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Condvar, Mutex};
use subtle::ConstantTimeEq as _;
use tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tungstenite::http::{StatusCode, header::ORIGIN};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_hdr_with_config};
use uuid::Uuid;

use crate::model::{AgentSession, Project, ProviderKind, SessionStatus};
use crate::protocol::MAX_WIRE_MESSAGE_BYTES;
use crate::protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage, WireDriverEvent,
};

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_HANDSHAKE_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_REPLAY_EVENTS_PER_SESSION: usize = 4096;
const MAX_CACHED_RESPONSES: usize = 2048;
const NATIVE_CLIENT_HEADER: &str = "x-waku-client";
const NATIVE_CLIENT_HEADER_VALUE: &str = "native";

#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// Browser WebSocket handshakes carry an Origin header. Most native clients
    /// do not; React Native does and identifies itself with `x-waku-client`.
    /// An empty set therefore still permits native clients only.
    pub allowed_origins: HashSet<String>,
    /// Only a daemon owned by the desktop process should accept the global
    /// shutdown control message. Service-managed daemons keep running when an
    /// authenticated client disconnects.
    pub allow_shutdown: bool,
}

struct ConnectionPermit(Arc<AtomicUsize>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub trait Backend: Send + Sync + 'static {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload>;

    /// Commit ordered provider events and their readable history together.
    fn persist_events(&self, _events: &[SequencedEvent]) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Drain a previous provider before the hub replaces its runtime identity.
    fn prepare_start(
        &self,
        _session_id: Uuid,
        _options: &crate::WireDriverStartOptions,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called on a separate worker after event ingestion releases its locks.
    fn stop_failed_work(&self) {}

    /// Process durable child notifications outside the event ingestion lock.
    fn resume_stewards(&self, _events: EventSink) {}

    fn authorize_steward(&self, _session_id: Uuid, _project_id: Uuid) -> anyhow::Result<()> {
        bail!("steward sessions are unavailable")
    }

    fn authorize_cached_creation(&self, _child: &AgentSession) -> anyhow::Result<()> {
        bail!("cached child creation is unavailable")
    }

    fn shutdown(&self) {}
}

#[derive(Clone)]
pub struct EventSink {
    session_id: Uuid,
    runtime_id: Uuid,
    hub: Arc<Hub>,
    creation_started: Option<Sender<Result<(), String>>>,
    pub(crate) scoped_project: Option<Uuid>,
    scoped_principal: Option<Uuid>,
}

pub(crate) struct ChildCreationGuard {
    session_id: Uuid,
    hub: Arc<Hub>,
    wake_on_release: bool,
}

impl Drop for ChildCreationGuard {
    fn drop(&mut self) {
        {
            let mut state = self.hub.state.lock();
            state.creating_sessions.remove(&self.session_id);
            state.operation_sessions.remove(&self.session_id);
        }
        self.hub.operation_released.notify_all();
        if self.wake_on_release {
            self.hub.wake_stewards();
        }
    }
}

impl EventSink {
    pub(crate) fn ensure_steward_active(&self) -> anyhow::Result<()> {
        if self
            .scoped_principal
            .is_some_and(|principal| !self.hub.state.lock().capabilities.contains_key(&principal))
        {
            bail!("steward runtime is no longer active");
        }
        Ok(())
    }

    pub(crate) fn mcp_config(&self, project_id: Uuid) -> anyhow::Result<Option<String>> {
        let Some(address) = self.hub.address else {
            return Ok(None);
        };
        let principal = Uuid::new_v4();
        let scope = StewardScope {
            principal,
            session_id: self.session_id,
            runtime_id: self.runtime_id,
            project_id,
        };
        self.hub.state.lock().capabilities.insert(principal, scope);
        Ok(Some(
            serde_json::json!({"mcpServers": {"waku": {
                "type": "stdio",
                "command": std::env::current_exe()?,
                "args": ["mcp"],
                "env": {
                    "WAKU_MCP_ADDRESS": address.to_string(),
                    "WAKU_MCP_TOKEN": principal.to_string(),
                    "WAKU_MCP_SESSION": self.session_id.to_string(),
                    "WAKU_MCP_RUNTIME": self.runtime_id.to_string()
                }
            }}})
            .to_string(),
        ))
    }

    pub(crate) fn reserve_child(&self, session_id: Uuid) -> ChildCreationGuard {
        self.hub.state.lock().creating_sessions.insert(session_id);
        ChildCreationGuard {
            session_id,
            hub: self.hub.clone(),
            wake_on_release: false,
        }
    }

    pub(crate) fn reserve_steward_target(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<ChildCreationGuard> {
        let mut state = self.hub.state.lock();
        if !state.creating_sessions.insert(session_id) {
            bail!("child session is busy accepting another operation");
        }
        state.operation_sessions.insert(session_id);
        Ok(ChildCreationGuard {
            session_id,
            hub: self.hub.clone(),
            wake_on_release: false,
        })
    }

    fn reserve_client_target(&self, session_id: Uuid) -> anyhow::Result<ChildCreationGuard> {
        let mut state = self.hub.state.lock();
        self.hub.operation_released.wait_while(&mut state, |state| {
            state.operation_sessions.contains(&session_id)
        });
        if !state.creating_sessions.insert(session_id) {
            bail!("child session creation is still in progress");
        }
        state.operation_sessions.insert(session_id);
        Ok(ChildCreationGuard {
            session_id,
            hub: self.hub.clone(),
            wake_on_release: true,
        })
    }

    pub(crate) fn child_sink(&self, session_id: Uuid, runtime_id: Uuid) -> Self {
        self.hub.event_sink(session_id, runtime_id)
    }

    pub(crate) fn begin_child(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
    ) -> (Self, Receiver<Result<(), String>>) {
        self.hub.begin_runtime(session_id, runtime_id);
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let mut sink = self.hub.event_sink(session_id, runtime_id);
        sink.creation_started = Some(sender);
        (sink, receiver)
    }

    pub(crate) fn end_runtime(&self) {
        self.hub.end_runtime(self.session_id, Some(self.runtime_id));
    }

    #[cfg(test)]
    pub(crate) fn for_test(backend: &Arc<dyn Backend>, session_id: Uuid, runtime_id: Uuid) -> Self {
        let hub = Arc::new(Hub {
            backend: Some(Arc::downgrade(backend)),
            ..Hub::default()
        });
        hub.begin_runtime(session_id, runtime_id);
        hub.event_sink(session_id, runtime_id)
    }

    pub(crate) fn stop_failed_work(&self) {
        if let Some(backend) = self.hub.backend.as_ref().and_then(Weak::upgrade) {
            std::thread::spawn(move || backend.stop_failed_work());
        }
    }

    pub fn send(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.send_batch(vec![event])
    }

    pub fn send_batch(&self, events: Vec<WireDriverEvent>) -> anyhow::Result<()> {
        if events.iter().any(|event| event.kind == "processExited") {
            self.hub.state.lock().capabilities.retain(|_, scope| {
                scope.session_id != self.session_id || scope.runtime_id != self.runtime_id
            });
        }
        let started = self.creation_started.as_ref().and_then(|_| {
            events.iter().find_map(|event| match event.kind.as_str() {
                "turnStarted" => Some(Ok(())),
                "error" => Some(Err(event
                    .payload
                    .as_str()
                    .unwrap_or("provider startup failed")
                    .to_owned())),
                "processExited" => Some(Err(
                    "provider exited before accepting the first prompt".into()
                )),
                _ => None,
            })
        });
        let saved = self
            .hub
            .emit_batch(self.session_id, self.runtime_id, events, true);
        // Creation completes only after provider acceptance is durable. The
        // single-slot channel never blocks the ordinary event forwarding path.
        if let Some(sender) = &self.creation_started {
            if let Some(result) = saved
                .as_ref()
                .err()
                .map(|error| Err(format!("{error:#}")))
                .or(started)
            {
                let state = self.hub.state.lock();
                if state.creating_sessions.contains(&self.session_id)
                    && state.active_runtimes.get(&self.session_id) == Some(&self.runtime_id)
                {
                    let _ = sender.try_send(result);
                }
            }
        }
        saved
    }

    /// Broadcast a live-only event without retaining it in the replay journal.
    /// High-volume PTY output is meaningful only to a terminal emulator that
    /// is currently attached; replaying raw chunks into a fresh emulator would
    /// also retain an unbounded terminal transcript in daemon memory.
    pub fn send_ephemeral(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.hub
            .emit(self.session_id, self.runtime_id, event, false);
        Ok(())
    }
}

#[derive(Default)]
struct HubState {
    next_subscriber_id: u64,
    task_state_revision: u64,
    subscribers: HashMap<u64, Sender<ServerMessage>>,
    active_runtimes: HashMap<Uuid, Uuid>,
    creating_sessions: HashSet<Uuid>,
    operation_sessions: HashSet<Uuid>,
    next_sequences: HashMap<(Uuid, Uuid), u64>,
    journal: HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>,
    history_persistence: HashMap<(Uuid, Uuid), ServerMessage>,
    capabilities: HashMap<Uuid, StewardScope>,
    responses: VecDeque<((Uuid, Uuid), ResponseOutcome)>,
    response_waiters: HashMap<(Uuid, Uuid), Vec<Sender<ServerMessage>>>,
    catalog_projects: HashMap<Uuid, ProjectCatalogEntry>,
    catalog_sessions: HashMap<Uuid, SessionCatalogEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectCatalogEntry {
    name: String,
    path: std::path::PathBuf,
    created_at: u64,
}

impl From<&Project> for ProjectCatalogEntry {
    fn from(project: &Project) -> Self {
        Self {
            name: project.name.clone(),
            path: project.path.clone(),
            created_at: project.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionCatalogEntry {
    title: String,
    auto_title: Option<String>,
    project_id: Uuid,
    provider: ProviderKind,
    model: Option<String>,
    status: SessionStatus,
    created_at: u64,
    last_reply_at: Option<u64>,
}

impl From<&AgentSession> for SessionCatalogEntry {
    fn from(session: &AgentSession) -> Self {
        Self {
            title: session.title.clone(),
            auto_title: session.auto_title.clone(),
            project_id: session.project_id,
            provider: session.provider,
            model: session.model.clone(),
            status: session.status,
            created_at: session.created_at,
            last_reply_at: session.last_reply_at,
        }
    }
}

#[derive(Clone, Debug)]
struct StewardScope {
    principal: Uuid,
    session_id: Uuid,
    runtime_id: Uuid,
    project_id: Uuid,
}

struct Hub {
    address: Option<std::net::SocketAddr>,
    epoch: Uuid,
    state: Mutex<HubState>,
    operation_released: Condvar,
    backend: Option<Weak<dyn Backend>>,
    steward_wake: Mutex<Option<Sender<()>>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            address: None,
            epoch: Uuid::new_v4(),
            state: Mutex::new(HubState::default()),
            operation_released: Condvar::new(),
            backend: None,
            steward_wake: Mutex::new(None),
        }
    }
}

struct DispatchedRequest {
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
}

struct RuntimeMailbox {
    id: Uuid,
    sender: Sender<DispatchedRequest>,
}

struct RequestDispatcher {
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
    /// A live provider runtime is an actor owned by the daemon, not by any
    /// particular WebSocket connection. One mailbox per session preserves
    /// lifecycle order across desktop and web clients without serializing
    /// unrelated sessions or read-only requests.
    runtime_mailboxes: Arc<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
}

impl Hub {
    fn wake_stewards(&self) {
        if let Some(sender) = self.steward_wake.lock().as_ref() {
            let _ = sender.try_send(());
        }
    }

    fn event_sink(self: &Arc<Self>, session_id: Uuid, runtime_id: Uuid) -> EventSink {
        EventSink {
            session_id,
            runtime_id,
            hub: self.clone(),
            creation_started: None,
            scoped_project: None,
            scoped_principal: None,
        }
    }

    fn begin_runtime(&self, session_id: Uuid, runtime_id: Uuid) {
        let mut state = self.state.lock();
        state
            .capabilities
            .retain(|_, scope| scope.session_id != session_id);
        state.active_runtimes.insert(session_id, runtime_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .history_persistence
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    /// A successful drain may retry the last failed commit without a new event.
    fn confirm_drained_history(&self, session_id: Uuid, runtime_id: Option<Uuid>) {
        let mut state = self.state.lock();
        let confirmations = state
            .history_persistence
            .iter_mut()
            .filter(|((session, runtime), _)| {
                *session == session_id && runtime_id.is_none_or(|id| id == *runtime)
            })
            .filter_map(|(_, status)| {
                let ServerMessage::HistoryPersistence { error, .. } = status else {
                    return None;
                };
                error.take().map(|_| status.clone())
            })
            .collect::<Vec<_>>();
        for status in confirmations {
            state
                .subscribers
                .retain(|_, subscriber| subscriber.send(status.clone()).is_ok());
        }
    }

    fn confirm_all_drained_history(&self) {
        let runtimes = self.state.lock().active_runtimes.clone();
        for (session_id, runtime_id) in runtimes {
            self.confirm_drained_history(session_id, Some(runtime_id));
        }
    }

    fn end_runtime(&self, session_id: Uuid, runtime_id: Option<Uuid>) {
        let mut state = self.state.lock();
        let matches_active = runtime_id
            .is_none_or(|runtime_id| state.active_runtimes.get(&session_id) == Some(&runtime_id));
        if !matches_active {
            return;
        }
        state
            .capabilities
            .retain(|_, scope| scope.session_id != session_id);
        state.active_runtimes.remove(&session_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .history_persistence
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    fn emit(&self, session_id: Uuid, runtime_id: Uuid, event: WireDriverEvent, replayable: bool) {
        let _ = self.emit_batch(session_id, runtime_id, vec![event], replayable);
    }

    fn emit_batch(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        events: Vec<WireDriverEvent>,
        replayable: bool,
    ) -> anyhow::Result<()> {
        // This lock serializes prompt acceptance, provider output and replay.
        // All callers run outside the UI; no event can pass a pending commit.
        let mut state = self.state.lock();
        if state.active_runtimes.get(&session_id) != Some(&runtime_id) {
            return Ok(());
        }
        let mut sequenced = Vec::with_capacity(events.len());
        for event in events {
            let sequence = state
                .next_sequences
                .entry((session_id, runtime_id))
                .or_default();
            *sequence = sequence.saturating_add(1);
            let event = SequencedEvent {
                session_id,
                runtime_id,
                epoch: self.epoch,
                sequence: *sequence,
                event,
            };
            if replayable {
                state
                    .journal
                    .entry((session_id, runtime_id))
                    .or_default()
                    .push_back(event.clone());
            }
            state.subscribers.retain(|_, subscriber| {
                subscriber.send(ServerMessage::Event(event.clone())).is_ok()
            });
            sequenced.push(event);
        }
        let was_failed = matches!(
            state.history_persistence.get(&(session_id, runtime_id)),
            Some(ServerMessage::HistoryPersistence { error: Some(_), .. })
        );
        let mut persistence_error = None;
        let mut committed = false;
        if replayable && let Some(backend) = self.backend.as_ref().and_then(Weak::upgrade) {
            let result = backend.persist_events(&sequenced);
            committed = matches!(&result, Ok(true));
            if let Some(last) = sequenced.last() {
                match &result {
                    Ok(true) | Err(_) => {
                        let message = ServerMessage::HistoryPersistence {
                            session_id,
                            runtime_id,
                            epoch: self.epoch,
                            sequence: last.sequence,
                            error: result.as_ref().err().map(|error| format!("{error:#}")),
                        };
                        state
                            .history_persistence
                            .insert((session_id, runtime_id), message.clone());
                        state
                            .subscribers
                            .retain(|_, subscriber| subscriber.send(message.clone()).is_ok());
                    }
                    Ok(false) => {}
                }
            }
            persistence_error = result.err();
        }
        if let Some(error) = persistence_error {
            drop(state);
            if !was_failed && let Some(backend) = self.backend.as_ref().and_then(Weak::upgrade) {
                std::thread::spawn(move || backend.stop_failed_work());
            }
            return Err(error);
        }
        if let Some(journal) = state.journal.get_mut(&(session_id, runtime_id)) {
            // Durable history is replayed from SQLite; this is only a hot tail.
            while journal.len() > MAX_REPLAY_EVENTS_PER_SESSION {
                journal.pop_front();
            }
        }
        let wake = committed && sequenced.iter().any(|event| matches!(event.event.kind.as_str(),
            "turnFinished" | "turnInterrupted" | "permission" | "userInputRequested"
                | "error" | "processExited" | "stewardWaitChanged"));
        drop(state);
        if wake {
            self.wake_stewards();
        }
        Ok(())
    }

    fn subscribe(&self, resume_from: &[ReplayCursor], sender: Sender<ServerMessage>) -> u64 {
        let mut state = self.state.lock();
        for (&(session_id, runtime_id), events) in &state.journal {
            let sequence = resume_from
                .iter()
                .find(|cursor| {
                    cursor.session_id == session_id
                        && cursor.runtime_id == runtime_id
                        && cursor.epoch == self.epoch
                })
                .map(|cursor| cursor.sequence)
                .unwrap_or_default();
            for event in events.iter().filter(|event| event.sequence > sequence) {
                let _ = sender.send(ServerMessage::Event(event.clone()));
            }
            if let Some(status) = state.history_persistence.get(&(session_id, runtime_id)) {
                let _ = sender.send(status.clone());
            }
        }
        let id = state.next_subscriber_id;
        state.next_subscriber_id = state.next_subscriber_id.saturating_add(1);
        state.subscribers.insert(id, sender);
        id
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        self.state.lock().subscribers.remove(&subscriber_id);
    }

    fn task_state_changed(&self, source_subscriber_id: u64) {
        let mut state = self.state.lock();
        Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
    }

    fn replace_task_catalog(&self, projects: &[Project], sessions: &[AgentSession]) {
        let mut state = self.state.lock();
        state.catalog_projects = projects
            .iter()
            .map(|project| (project.id, ProjectCatalogEntry::from(project)))
            .collect();
        state.catalog_sessions = sessions
            .iter()
            .map(|session| (session.id, SessionCatalogEntry::from(session)))
            .collect();
    }

    fn task_state_saved(
        &self,
        source_subscriber_id: u64,
        projects: &[Project],
        sessions: &[AgentSession],
    ) {
        let mut state = self.state.lock();
        let mut changed = false;
        for project in projects {
            let next = ProjectCatalogEntry::from(project);
            changed |= state
                .catalog_projects
                .insert(project.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        for session in sessions {
            let next = SessionCatalogEntry::from(session);
            changed |= state
                .catalog_sessions
                .insert(session.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        if changed {
            Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
        }
    }

    fn broadcast_task_state_changed(state: &mut HubState, source_subscriber_id: u64) {
        state.task_state_revision = state.task_state_revision.saturating_add(1);
        let message = ServerMessage::TaskStateChanged {
            revision: state.task_state_revision,
        };
        state.subscribers.retain(|subscriber_id, subscriber| {
            *subscriber_id == source_subscriber_id || subscriber.send(message.clone()).is_ok()
        });
    }

    /// Reserve a UUID before dispatch. Retransmissions share the first
    /// execution's response without occupying another worker or mailbox.
    fn reserve_request(&self, request_id: Uuid, outgoing: &Sender<ServerMessage>) -> bool {
        self.reserve_request_as(Uuid::nil(), request_id, outgoing)
    }

    fn reserve_request_as(
        &self,
        principal: Uuid,
        request_id: Uuid,
        outgoing: &Sender<ServerMessage>,
    ) -> bool {
        if request_id.is_nil() {
            return true;
        }
        let mut state = self.state.lock();
        if let Some(outcome) = state
            .responses
            .iter()
            .rev()
            .find_map(|(id, outcome)| (*id == (principal, request_id)).then(|| outcome.clone()))
        {
            drop(state);
            let _ = outgoing.send(ServerMessage::Response {
                request_id,
                outcome: self.validate_cached_creation(outcome),
            });
            return false;
        }
        match state.response_waiters.entry((principal, request_id)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push(outgoing.clone());
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Vec::new());
                true
            }
        }
    }

    fn cached_response(&self, request_id: Uuid) -> Option<ResponseOutcome> {
        self.cached_response_as(Uuid::nil(), request_id)
    }

    fn cached_response_as(&self, principal: Uuid, request_id: Uuid) -> Option<ResponseOutcome> {
        let outcome = self
            .state
            .lock()
            .responses
            .iter()
            .rev()
            .find_map(|(cached_id, outcome)| {
                (*cached_id == (principal, request_id)).then(|| outcome.clone())
            });
        outcome.map(|outcome| self.validate_cached_creation(outcome))
    }

    fn validate_cached_creation(&self, outcome: ResponseOutcome) -> ResponseOutcome {
        if let ResponseOutcome::Ok {
            payload: ResponsePayload::SessionCreated { session, .. },
        } = &outcome
            && let Some(backend) = self.backend.as_ref().and_then(Weak::upgrade)
            && let Err(error) = backend.authorize_cached_creation(session)
        {
            return ResponseOutcome::Error {
                error: RpcError::from(error),
            };
        }
        outcome
    }

    fn cache_response(&self, request_id: Uuid, outcome: ResponseOutcome) {
        self.cache_response_as(Uuid::nil(), request_id, outcome);
    }

    fn cache_response_as(&self, principal: Uuid, request_id: Uuid, outcome: ResponseOutcome) {
        let waiters = {
            let mut state = self.state.lock();
            state
                .responses
                .push_back(((principal, request_id), outcome.clone()));
            while state.responses.len() > MAX_CACHED_RESPONSES {
                state.responses.pop_front();
            }
            state
                .response_waiters
                .remove(&(principal, request_id))
                .unwrap_or_default()
        };
        for outgoing in waiters {
            let _ = outgoing.send(ServerMessage::Response {
                request_id,
                outcome: outcome.clone(),
            });
        }
    }
}

impl RequestDispatcher {
    fn new(backend: Arc<dyn Backend>, hub: Arc<Hub>) -> Self {
        Self {
            backend,
            hub,
            runtime_mailboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn dispatch(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        if !matches!(request.command, Command::StewardQuery { .. })
            && !self.hub.reserve_request(request.request_id, &outgoing)
        {
            return;
        }
        if command_targets_runtime(&request.command) {
            self.dispatch_runtime(request, outgoing, source_subscriber_id);
        } else {
            self.dispatch_independent(request, outgoing, source_subscriber_id);
        }
    }

    fn dispatch_independent(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let backend = self.backend.clone();
        let hub = self.hub.clone();
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("waku-daemon-request".into())
            .spawn(move || {
                handle_request(request, outgoing, source_subscriber_id, backend, hub);
            })
        {
            send_dispatch_error(
                failed_request_id,
                failed_outgoing,
                &self.hub,
                format!("could not start daemon request worker: {error}"),
            );
        }
    }

    fn dispatch_runtime(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let session_id = request.session_id;
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        let mut dispatched = DispatchedRequest {
            request,
            outgoing,
            source_subscriber_id,
        };
        loop {
            let mut mailboxes = self.runtime_mailboxes.lock();
            if let Some(mailbox) = mailboxes.get(&session_id) {
                match mailbox.sender.send(dispatched) {
                    Ok(()) => return,
                    Err(error) => {
                        dispatched = error.0;
                        mailboxes.remove(&session_id);
                        continue;
                    }
                }
            }

            let mailbox_id = Uuid::new_v4();
            let (sender, requests) = unbounded();
            sender
                .send(dispatched)
                .expect("a new runtime mailbox still has its receiver");
            mailboxes.insert(
                session_id,
                RuntimeMailbox {
                    id: mailbox_id,
                    sender,
                },
            );

            let backend = self.backend.clone();
            let hub = self.hub.clone();
            let mailbox_registry = Arc::downgrade(&self.runtime_mailboxes);
            let worker = std::thread::Builder::new()
                .name(format!("waku-daemon-runtime-{session_id}"))
                .spawn(move || {
                    run_runtime_mailbox(
                        session_id,
                        mailbox_id,
                        requests,
                        mailbox_registry,
                        backend,
                        hub,
                    );
                });
            if let Err(error) = worker {
                if mailboxes
                    .get(&session_id)
                    .is_some_and(|mailbox| mailbox.id == mailbox_id)
                {
                    mailboxes.remove(&session_id);
                }
                drop(mailboxes);
                send_dispatch_error(
                    failed_request_id,
                    failed_outgoing,
                    &self.hub,
                    format!("could not start runtime worker: {error}"),
                );
            }
            return;
        }
    }
}

fn mcp_address(mut address: std::net::SocketAddr) -> std::net::SocketAddr {
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    address
}

struct StewardWorker {
    hub: Arc<Hub>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StewardWorker {
    fn start(hub: Arc<Hub>, backend: Arc<dyn Backend>) -> anyhow::Result<Self> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = stopped.clone();
        let weak = Arc::downgrade(&hub);
        let thread = std::thread::Builder::new()
            .name("waku-steward-notifications".into())
            .spawn(move || {
                while receiver.recv().is_ok() && !worker_stop.load(Ordering::Acquire) {
                    let Some(hub) = weak.upgrade() else { break };
                    backend.resume_stewards(hub.event_sink(Uuid::nil(), Uuid::nil()));
                }
            })?;
        *hub.steward_wake.lock() = Some(sender);
        hub.wake_stewards();
        Ok(Self { hub, stopped, thread: Some(thread) })
    }
}

impl Drop for StewardWorker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.hub.steward_wake.lock().take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn serve(
    listener: TcpListener,
    token: String,
    backend: Arc<dyn Backend>,
    shutdown: Arc<AtomicBool>,
    options: ServerOptions,
) -> anyhow::Result<()> {
    let address = listener.local_addr()?;
    listener
        .set_nonblocking(true)
        .context("could not configure Waku daemon listener")?;
    let hub = Arc::new(Hub {
        address: Some(mcp_address(address)),
        backend: Some(Arc::downgrade(&backend)),
        ..Hub::default()
    });
    let _steward_worker = StewardWorker::start(hub.clone(), backend.clone())?;
    let dispatcher = Arc::new(RequestDispatcher::new(backend.clone(), hub.clone()));
    let options = Arc::new(options);
    let active_connections = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if active_connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                        (active < MAX_CONNECTIONS).then_some(active + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let connection_permit = ConnectionPermit(active_connections.clone());
                let token = token.clone();
                let dispatcher = dispatcher.clone();
                let hub = hub.clone();
                let shutdown = shutdown.clone();
                let options = options.clone();
                std::thread::Builder::new()
                    .name("waku-daemon-connection".into())
                    .spawn(move || {
                        let _connection_permit = connection_permit;
                        if let Err(error) =
                            handle_connection(stream, &token, dispatcher, hub, shutdown, &options)
                        {
                            eprintln!("waku-daemon connection ended: {error:#}");
                        }
                    })
                    .context("could not start Waku daemon connection thread")?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Waku daemon listener failed"),
        }
    }
    backend.shutdown();
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    expected_token: &str,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    shutdown: Arc<AtomicBool>,
    options: &ServerOptions,
) -> anyhow::Result<()> {
    // Accepted sockets can inherit the listener's nonblocking flag on some
    // platforms. The handshake is deliberately blocking; steady-state reads
    // get their bounded polling behavior from SO_RCVTIMEO below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES));
    let allowed_origins = options.allowed_origins.clone();
    let mut socket = accept_hdr_with_config(
        stream,
        move |request: &HandshakeRequest, response: HandshakeResponse| {
            validate_handshake(request, response, &allowed_origins)
        },
        Some(config),
    )
    .context("WebSocket handshake failed")?;
    let hello = read_client_message(&mut socket)?;
    let mut scope = None;
    let resume_from = match hello {
        ClientMessage::Hello {
            protocol_version,
            token,
            resume_from,
            ..
        } if protocol_version == PROTOCOL_VERSION && token_matches(expected_token, &token) => {
            resume_from
        }
        ClientMessage::Hello {
            protocol_version,
            token,
            ..
        } if protocol_version == PROTOCOL_VERSION
            && Uuid::parse_str(&token).ok().is_some_and(|id| {
                scope = hub.state.lock().capabilities.get(&id).cloned();
                scope.is_some()
            }) =>
        {
            Vec::new()
        }
        ClientMessage::Hello {
            protocol_version, ..
        } if protocol_version != PROTOCOL_VERSION => {
            write_json(
                &mut socket,
                &ServerMessage::Rejected {
                    message: format!(
                        "protocol {protocol_version} is unsupported; expected {PROTOCOL_VERSION}"
                    ),
                },
            )?;
            return Ok(());
        }
        ClientMessage::Hello { .. } => {
            write_json(
                &mut socket,
                &ServerMessage::Rejected {
                    message: "authentication failed".into(),
                },
            )?;
            return Ok(());
        }
        _ => bail!("first daemon message was not a hello"),
    };
    write_json(
        &mut socket,
        &ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    socket.set_config(|config| {
        config.max_message_size = Some(MAX_WIRE_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_WIRE_MESSAGE_BYTES);
    });
    socket
        .get_mut()
        .set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;

    let (outgoing, outgoing_rx) = unbounded();
    let subscriber_id = if scope.is_none() {
        hub.subscribe(&resume_from, outgoing.clone())
    } else {
        u64::MAX
    };

    'connection: while !shutdown.load(Ordering::Acquire) {
        while let Ok(message) = outgoing_rx.try_recv() {
            if write_server_message(&mut socket, &message).is_err() {
                break 'connection;
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str(text.as_ref()) {
                Ok(ClientMessage::Request(request)) => {
                    if let Some(scope) = &scope {
                        dispatch_steward(
                            request,
                            scope.clone(),
                            outgoing.clone(),
                            dispatcher.backend.clone(),
                            hub.clone(),
                        );
                        continue;
                    }
                    let exit_command = matches!(
                        request.command,
                        Command::PrepareShutdown | Command::ShutdownDaemon
                    );
                    if exit_command && !options.allow_shutdown {
                        write_json(
                            &mut socket,
                            &ServerMessage::Response {
                                request_id: request.request_id,
                                outcome: ResponseOutcome::Error {
                                    error: RpcError::from(anyhow::anyhow!(
                                        "daemon shutdown is managed by its service owner"
                                    )),
                                },
                            },
                        )?;
                    } else if matches!(request.command, Command::ShutdownDaemon) {
                        let result = dispatcher
                            .backend
                            .handle(request.clone(), hub.event_sink(Uuid::nil(), Uuid::nil()));
                        let successful = result.is_ok();
                        let outcome = match result {
                            Ok(payload) => ResponseOutcome::Ok { payload },
                            Err(error) => ResponseOutcome::Error {
                                error: RpcError::from(error),
                            },
                        };
                        write_json(
                            &mut socket,
                            &ServerMessage::Response {
                                request_id: request.request_id,
                                outcome,
                            },
                        )?;
                        if successful {
                            hub.confirm_all_drained_history();
                            shutdown.store(true, Ordering::Release);
                            break;
                        }
                    } else {
                        dispatcher.dispatch(request, outgoing.clone(), subscriber_id);
                    }
                }
                Ok(ClientMessage::Shutdown) => {
                    if options.allow_shutdown && scope.is_none() {
                        let result = dispatcher.backend.handle(
                            Request {
                                request_id: Uuid::new_v4(),
                                session_id: Uuid::nil(),
                                runtime_id: Uuid::nil(),
                                command: Command::PrepareShutdown,
                            },
                            hub.event_sink(Uuid::nil(), Uuid::nil()),
                        );
                        if let Err(error) = result {
                            write_json(
                                &mut socket,
                                &ServerMessage::Rejected {
                                    message: format!(
                                        "unsaved history prevents shutdown: {error:#}"
                                    ),
                                },
                            )?;
                            continue;
                        }
                        hub.confirm_all_drained_history();
                        write_json(&mut socket, &ServerMessage::ShuttingDown)?;
                        shutdown.store(true, Ordering::Release);
                        break;
                    }
                    write_json(
                        &mut socket,
                        &ServerMessage::Rejected {
                            message: "daemon shutdown is managed by its service owner".into(),
                        },
                    )?;
                }
                Ok(ClientMessage::Hello { .. }) => {}
                Err(error) => {
                    eprintln!("waku-daemon ignored invalid message: {error}");
                }
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {
                let _ = socket.flush();
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if retryable_io(&error) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(error) => return Err(error).context("Waku daemon WebSocket failed"),
        }
    }
    hub.unsubscribe(subscriber_id);
    Ok(())
}

fn dispatch_steward(
    request: Request,
    scope: StewardScope,
    outgoing: Sender<ServerMessage>,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) {
    let active = hub.state.lock().capabilities.contains_key(&scope.principal);
    let authorized = request.session_id == scope.session_id
        && request.runtime_id == scope.runtime_id
        && matches!(
            request.command,
            Command::CreateSession { .. }
                | Command::StewardQuery { .. }
                | Command::StewardPrompt { .. }
                | Command::StewardCancel { .. }
                | Command::StewardWait { .. }
        )
        && active
        && backend
            .authorize_steward(scope.session_id, scope.project_id)
            .is_ok();
    if !authorized {
        let _ = outgoing.send(ServerMessage::Response {
            request_id: request.request_id,
            outcome: ResponseOutcome::Error {
                error: RpcError {
                    message: "steward operation is not authorized".into(),
                },
            },
        });
        return;
    }
    let cacheable = !matches!(request.command, Command::StewardQuery { .. });
    if cacheable && !hub.reserve_request_as(scope.principal, request.request_id, &outgoing) {
        return;
    }
    let request_id = request.request_id;
    let failed_outgoing = outgoing.clone();
    let failed_hub = hub.clone();
    let principal = scope.principal;
    if let Err(error) = std::thread::Builder::new()
        .name("waku-steward-request".into())
        .spawn(move || {
            handle_request_as(request, outgoing, u64::MAX, backend, hub, Some(scope));
        })
    {
        let outcome = ResponseOutcome::Error {
            error: RpcError {
                message: format!("could not start steward request: {error}"),
            },
        };
        if cacheable {
            failed_hub.cache_response_as(principal, request_id, outcome.clone());
        }
        let _ = failed_outgoing.send(ServerMessage::Response {
            request_id,
            outcome,
        });
    }
}

fn validate_handshake(
    request: &HandshakeRequest,
    response: HandshakeResponse,
    allowed_origins: &HashSet<String>,
) -> Result<HandshakeResponse, ErrorResponse> {
    if request.uri().path() != "/v1" {
        return Err(handshake_error(
            StatusCode::NOT_FOUND,
            "unknown daemon endpoint",
        ));
    }
    let is_native_client = request
        .headers()
        .get(NATIVE_CLIENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == NATIVE_CLIENT_HEADER_VALUE);
    if let Some(origin) = request.headers().get(ORIGIN) {
        let allowed = origin
            .to_str()
            .ok()
            .is_some_and(|origin| allowed_origins.contains(origin));
        if !allowed && !is_native_client {
            return Err(handshake_error(
                StatusCode::FORBIDDEN,
                "WebSocket origin is not allowed",
            ));
        }
    }
    Ok(response)
}

fn handshake_error(status: StatusCode, message: &str) -> ErrorResponse {
    tungstenite::http::Response::builder()
        .status(status)
        .body(Some(message.to_owned()))
        .expect("static WebSocket rejection is valid")
}

fn token_matches(expected: &str, candidate: &str) -> bool {
    expected.as_bytes().ct_eq(candidate.as_bytes()).into()
}

fn command_targets_runtime(command: &Command) -> bool {
    matches!(
        command,
        Command::AttachSession
            | Command::Start { .. }
            | Command::Prompt { .. }
            | Command::Steer { .. }
            | Command::Cancel
            | Command::CancelComputerUse
            | Command::RefreshBackgroundWork
            | Command::StopBackgroundWork { .. }
            | Command::Respond { .. }
            | Command::RespondUserInput { .. }
            | Command::RunComputerTool { .. }
            | Command::RejectComputerTool { .. }
            | Command::ApplyOptions { .. }
            | Command::Rollback { .. }
            | Command::Fork { .. }
            | Command::ForkSessionFromResponse { .. }
            | Command::RewindSessionToMessage { .. }
            | Command::OpenTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal
            | Command::CloseSession
            | Command::RemoveSession
    )
}

fn run_runtime_mailbox(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: Receiver<DispatchedRequest>,
    mailbox_registry: Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) {
    let mut active_runtime_id = None;
    let mut pending = None;
    loop {
        let dispatched = match pending.take() {
            Some(request) => request,
            None => match requests.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        let runtime_id = dispatched.request.runtime_id;
        let starts_runtime = matches!(
            &dispatched.request.command,
            Command::Start { .. } | Command::OpenTerminal { .. }
        );
        let closes_runtime = matches!(
            &dispatched.request.command,
            Command::CloseSession
                | Command::CloseTerminal
                | Command::RemoveSession
                | Command::RewindSessionToMessage { .. }
        );
        let removes_session = matches!(
            &dispatched.request.command,
            Command::RemoveSession | Command::RewindSessionToMessage { .. }
        );
        let handled = handle_request(
            dispatched.request,
            dispatched.outgoing,
            dispatched.source_subscriber_id,
            backend.clone(),
            hub.clone(),
        );

        if handled.executed {
            if starts_runtime {
                active_runtime_id =
                    matches!(&handled.outcome, ResponseOutcome::Ok { .. }).then_some(runtime_id);
            } else if let ResponseOutcome::Ok {
                payload:
                    ResponsePayload::SessionRuntime {
                        runtime_id: Some(attached_runtime_id),
                        ..
                    },
            } = &handled.outcome
            {
                // A replacement mailbox can rediscover a provider runtime
                // that survived its previous actor worker.
                active_runtime_id = Some(*attached_runtime_id);
            } else if closes_runtime {
                if (removes_session || active_runtime_id == Some(runtime_id))
                    && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
                {
                    hub.confirm_drained_history(
                        session_id,
                        (!removes_session).then_some(runtime_id),
                    );
                    hub.end_runtime(session_id, (!removes_session).then_some(runtime_id));
                    active_runtime_id = None;
                }
            } else if active_runtime_id.is_none()
                && !matches!(
                    &handled.outcome,
                    ResponseOutcome::Ok {
                        payload: ResponsePayload::SessionRuntime { .. }
                    }
                )
                && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
            {
                // Recover the supervisor state if a previous mailbox worker
                // exited unexpectedly while the backend runtime stayed alive.
                active_runtime_id = Some(runtime_id);
            }
        }

        if active_runtime_id.is_none() {
            pending =
                take_queued_request_or_retire(session_id, mailbox_id, &requests, &mailbox_registry);
            if pending.is_none() {
                return;
            }
        }
    }
}

fn take_queued_request_or_retire(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: &Receiver<DispatchedRequest>,
    mailbox_registry: &Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
) -> Option<DispatchedRequest> {
    let Some(mailbox_registry) = mailbox_registry.upgrade() else {
        return requests.try_recv().ok();
    };
    // Dispatchers send while holding this same lock. Therefore an empty
    // receiver followed by removal is atomic with respect to a new command:
    // it either joins this actor before retirement or creates its successor.
    let mut mailboxes = mailbox_registry.lock();
    match requests.try_recv() {
        Ok(request) => Some(request),
        Err(crossbeam_channel::TryRecvError::Empty) => {
            if mailboxes
                .get(&session_id)
                .is_some_and(|mailbox| mailbox.id == mailbox_id)
            {
                mailboxes.remove(&session_id);
            }
            None
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => None,
    }
}

struct HandledRequest {
    outcome: ResponseOutcome,
    executed: bool,
}

enum TaskCatalogAction {
    None,
    Load,
    Save { projects: Vec<Project> },
    Changed,
    Created,
}

fn handle_request(
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) -> HandledRequest {
    handle_request_as(request, outgoing, source_subscriber_id, backend, hub, None)
}

fn handle_request_as(
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
    scope: Option<StewardScope>,
) -> HandledRequest {
    let principal = scope.as_ref().map_or(Uuid::nil(), |scope| scope.principal);
    let request_id = request.request_id;
    let notification = request_id.is_nil();
    let cacheable = !notification && !matches!(request.command, Command::StewardQuery { .. });
    let session_id = request.session_id;
    let runtime_id = request.runtime_id;
    let task_catalog_action = task_catalog_action(&request.command);
    let prepares_shutdown = matches!(request.command, Command::PrepareShutdown);
    let cancels = matches!(request.command, Command::Cancel);
    let starts_runtime = matches!(
        &request.command,
        Command::Start { .. } | Command::OpenTerminal { .. }
    );
    let mut began_runtime = false;
    let (outcome, executed) =
        if cacheable && let Some(cached) = hub.cached_response_as(principal, request_id) {
            (cached, false)
        } else {
            let mutation = (command_targets_runtime(&request.command)
                && !matches!(&request.command, Command::AttachSession))
                || matches!(&request.command, Command::CreateSession { .. });
            let operation = if mutation
                && scope.is_none()
                && !matches!(
                    &request.command,
                    Command::CreateSession { .. }
                        | Command::OpenTerminal { .. }
                        | Command::WriteTerminal { .. }
                        | Command::ResizeTerminal { .. }
                        | Command::CloseTerminal
                )
            {
                // Runs on the session mailbox worker, outside hub/backend locks.
                // A callback already submitting a turn must finish before user input.
                hub.event_sink(session_id, runtime_id)
                    .reserve_client_target(session_id)
                    .map(Some)
            } else if mutation && hub.state.lock().creating_sessions.contains(&session_id) {
                Err(anyhow::anyhow!(
                    "child session creation is still in progress"
                ))
            } else {
                Ok(None)
            };
            let prepared = operation.and_then(|guard| {
                if let Command::Start { options } = &request.command {
                    backend.prepare_start(session_id, options)?;
                }
                Ok(guard)
            });
            if starts_runtime && prepared.is_ok() {
                if matches!(&request.command, Command::Start { .. }) {
                    hub.confirm_drained_history(session_id, None);
                }
                hub.begin_runtime(session_id, runtime_id);
                began_runtime = true;
            }
            let outcome = match prepared.and_then(|_operation| {
                let mut events = hub.event_sink(session_id, runtime_id);
                events.scoped_project = scope.as_ref().map(|scope| scope.project_id);
                events.scoped_principal = scope.as_ref().map(|scope| scope.principal);
                if let Some(scope) = &scope {
                    if !hub.state.lock().capabilities.contains_key(&scope.principal) {
                        bail!("steward runtime is no longer active");
                    }
                }
                backend.handle(request, events)
            }) {
                Ok(payload) => ResponseOutcome::Ok { payload },
                Err(error) => ResponseOutcome::Error {
                    error: RpcError::from(error),
                },
            };
            if cacheable {
                hub.cache_response_as(principal, request_id, outcome.clone());
            }
            (outcome, true)
        };
    if executed && prepares_shutdown && matches!(&outcome, ResponseOutcome::Ok { .. }) {
        hub.confirm_all_drained_history();
    }
    if began_runtime && matches!(&outcome, ResponseOutcome::Error { .. }) {
        hub.end_runtime(session_id, Some(runtime_id));
    }
    if executed {
        match (&task_catalog_action, &outcome) {
            (
                TaskCatalogAction::Load,
                ResponseOutcome::Ok {
                    payload:
                        ResponsePayload::TaskState {
                            projects, sessions, ..
                        },
                },
            ) => hub.replace_task_catalog(projects, sessions),
            (
                TaskCatalogAction::Save { projects },
                ResponseOutcome::Ok {
                    payload: ResponsePayload::TaskStateSaved { sessions },
                },
            ) => hub.task_state_saved(source_subscriber_id, projects, sessions),
            (TaskCatalogAction::Changed, ResponseOutcome::Ok { .. })
            | (TaskCatalogAction::Created, _) => {
                hub.task_state_changed(source_subscriber_id);
                if cancels {
                    let _ = outgoing.send(ServerMessage::TaskStateChanged {
                        revision: hub.state.lock().task_state_revision,
                    });
                }
            }
            _ => {}
        }
    }
    if !notification {
        let _ = outgoing.send(ServerMessage::Response {
            request_id,
            outcome: outcome.clone(),
        });
    }
    HandledRequest { outcome, executed }
}

fn task_catalog_action(command: &Command) -> TaskCatalogAction {
    match command {
        Command::CreateSession { .. } => TaskCatalogAction::Created,
        Command::LoadTaskState => TaskCatalogAction::Load,
        Command::SaveTaskState { projects, .. } => TaskCatalogAction::Save {
            projects: projects.clone(),
        },
        Command::RemoveSession
        | Command::Cancel
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. } => TaskCatalogAction::Changed,
        _ => TaskCatalogAction::None,
    }
}

fn send_dispatch_error(
    request_id: Uuid,
    outgoing: Sender<ServerMessage>,
    hub: &Arc<Hub>,
    message: String,
) {
    if request_id.is_nil() {
        return;
    }
    let outcome = hub
        .cached_response(request_id)
        .unwrap_or_else(|| ResponseOutcome::Error {
            error: RpcError { message },
        });
    hub.cache_response(request_id, outcome.clone());
    let _ = outgoing.send(ServerMessage::Response {
        request_id,
        outcome,
    });
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

fn read_client_message(socket: &mut WebSocket<TcpStream>) -> anyhow::Result<ClientMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("client closed during daemon handshake"),
            _ => {}
        }
    }
}

fn write_server_message<S: io::Read + io::Write>(
    socket: &mut WebSocket<S>,
    message: &ServerMessage,
) -> anyhow::Result<()> {
    let (request_id, session, replay) = match message {
        ServerMessage::Response {
            request_id,
            outcome:
                ResponseOutcome::Ok {
                    payload: ResponsePayload::HistorySnapshot { session },
                },
        } => (*request_id, session, true),
        ServerMessage::Response {
            request_id,
            outcome:
                ResponseOutcome::Ok {
                    payload:
                        ResponsePayload::Session {
                            session: Some(session),
                        },
                },
        } => (*request_id, session, false),
        _ => return write_json(socket, message),
    };
    let data = serde_json::to_string(session)?;
    if !replay && data.len() < MAX_WIRE_MESSAGE_BYTES / 2 {
        return write_json(socket, message);
    }
    let cursor = session.runtime_event_cursor;
    if replay && (cursor.is_none() || cursor != session.history_saved_cursor) {
        bail!("snapshot has no reliable saved cursor");
    }
    let mut offset = 0;
    while offset < data.len() {
        // JSON escaping can expand one source byte to six bytes.
        let mut end = (offset + MAX_WIRE_MESSAGE_BYTES / 8).min(data.len());
        while !data.is_char_boundary(end) {
            end -= 1;
        }
        write_json(
            socket,
            &ServerMessage::HistorySnapshotChunk {
                request_id,
                session_id: session.id,
                cursor,
                replay,
                offset: offset as u64,
                total_bytes: data.len() as u64,
                data: data[offset..end].to_owned(),
            },
        )?;
        offset = end;
    }
    Ok(())
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    socket.send(Message::Text(serde_json::to_string(value)?.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::daemon::WakuBackend;
    #[cfg(unix)]
    use crate::model::Project;
    use crate::model::{AgentSession, ProviderKind};
    #[cfg(unix)]
    use crate::persistence::StateStore;
    #[cfg(unix)]
    use crate::settings::DaemonSettingsStore;
    use crate::{DaemonSettings, WireDriverStartOptions};
    #[cfg(unix)]
    use base64::Engine as _;
    use crossbeam_channel::{RecvTimeoutError, bounded};
    use serde_json::json;
    use std::path::PathBuf;
    use waku_client::{DaemonClient, DaemonSupervisor};

    #[derive(Default)]
    struct TestBackend {
        runtimes: Mutex<HashMap<Uuid, Uuid>>,
    }

    impl Backend for TestBackend {
        fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
            let session_id = request.session_id;
            let runtime_id = request.runtime_id;
            match request.command {
                Command::Start { .. } => {
                    self.runtimes.lock().insert(session_id, runtime_id);
                    events.send(WireDriverEvent::new("connected", json!({})))?;
                    Ok(ResponsePayload::Started {
                        supports_steer: true,
                    })
                }
                Command::AttachSession => Ok(ResponsePayload::SessionRuntime {
                    runtime_id: self.runtimes.lock().get(&session_id).copied(),
                    supports_steer: true,
                }),
                Command::GetSettings => Ok(ResponsePayload::Settings {
                    settings: DaemonSettings::default(),
                }),
                Command::Prompt { prompt, .. } => {
                    events.send(WireDriverEvent::new("textDelta", json!(prompt)))?;
                    Ok(ResponsePayload::Ack)
                }
                Command::CloseSession => {
                    self.runtimes.lock().remove(&session_id);
                    Ok(ResponsePayload::Ack)
                }
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[derive(Default)]
    struct TaskStateBackend {
        sessions: Mutex<Vec<AgentSession>>,
    }

    impl Backend for TaskStateBackend {
        fn handle(&self, request: Request, _events: EventSink) -> anyhow::Result<ResponsePayload> {
            match request.command {
                Command::SaveTaskState { sessions, .. } => {
                    let mut stored = self.sessions.lock();
                    for session in sessions {
                        if let Some(existing) =
                            stored.iter_mut().find(|existing| existing.id == session.id)
                        {
                            *existing = session;
                        } else {
                            stored.push(session);
                        }
                    }
                    Ok(ResponsePayload::TaskStateSaved {
                        sessions: stored.clone(),
                    })
                }
                Command::LoadTaskState => Ok(ResponsePayload::TaskState {
                    projects: Vec::new(),
                    sessions: self.sessions.lock().clone(),
                    default_cwd: PathBuf::from("/tmp"),
                    projectless_root: Some(PathBuf::from("/tmp/.waku/projects")),
                }),
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[test]
    fn client_prompt_and_cancel_wait_for_callback_operation() {
        for command in [
            Command::Prompt {
                prompt: "new user input".into(),
                turn_id: None,
                message_id: None,
            },
            Command::Cancel,
        ] {
            let cancels = matches!(command, Command::Cancel);
            let hub = Arc::new(Hub::default());
            let session_id = Uuid::new_v4();
            let runtime_id = Uuid::new_v4();
            hub.begin_runtime(session_id, runtime_id);
            let sink = hub.event_sink(session_id, runtime_id);
            let callback = sink.reserve_steward_target(session_id).unwrap();
            let (wake, wakes) = bounded(1);
            *hub.steward_wake.lock() = Some(wake);
            let (outgoing, received) = unbounded();
            let source = hub.subscribe(&[], outgoing.clone());
            let (started, start) = bounded(1);
            let (completed, completion) = bounded(1);
            let worker_hub = hub.clone();
            let worker = std::thread::spawn(move || {
                started.send(()).unwrap();
                let handled = handle_request(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id,
                        runtime_id,
                        command,
                    },
                    outgoing,
                    source,
                    Arc::new(TestBackend::default()),
                    worker_hub,
                );
                completed.send(()).unwrap();
                handled.outcome
            });
            start.recv_timeout(Duration::from_secs(2)).unwrap();
            let early = completion.recv_timeout(Duration::from_millis(50));
            drop(callback);
            let outcome = worker.join().unwrap();
            assert!(matches!(early, Err(RecvTimeoutError::Timeout)));
            assert!(matches!(outcome, ResponseOutcome::Ok { .. }));
            assert!(wakes.try_recv().is_ok());
            assert!(sink.reserve_steward_target(session_id).is_ok());
            if cancels {
                assert!(received.try_iter().any(|message| {
                    matches!(message, ServerMessage::TaskStateChanged { .. })
                }));
            }
        }
    }

    #[test]
    fn mcp_wildcard_listeners_connect_through_same_family_loopback() {
        for (bound, expected) in [
            ("0.0.0.0:1234", "127.0.0.1:1234"),
            ("[::]:1234", "[::1]:1234"),
            ("127.0.0.1:1234", "127.0.0.1:1234"),
            ("192.0.2.1:1234", "192.0.2.1:1234"),
        ] {
            assert_eq!(mcp_address(bound.parse().unwrap()).to_string(), expected);
        }
    }

    #[test]
    fn drained_history_retry_confirms_the_same_cursor_without_another_event() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        hub.begin_runtime(session_id, runtime_id);
        let (sender, receiver) = unbounded();
        hub.subscribe(&[], sender);
        hub.state.lock().history_persistence.insert(
            (session_id, runtime_id),
            ServerMessage::HistoryPersistence {
                session_id,
                runtime_id,
                epoch: hub.epoch,
                sequence: 7,
                error: Some("disk full".into()),
            },
        );
        hub.confirm_drained_history(session_id, Some(Uuid::new_v4()));
        assert!(receiver.try_recv().is_err());
        hub.confirm_drained_history(session_id, Some(runtime_id));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            ServerMessage::HistoryPersistence {
                sequence: 7,
                error: None,
                ..
            }
        ));
        assert!(hub.state.lock().next_sequences.is_empty());
        hub.confirm_drained_history(session_id, Some(runtime_id));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn task_state_revisions_notify_other_clients_only() {
        let hub = Hub::default();
        let (source_tx, source_rx) = unbounded();
        let source_id = hub.subscribe(&[], source_tx);
        let (observer_tx, observer_rx) = unbounded();
        hub.subscribe(&[], observer_tx);

        hub.task_state_changed(source_id);

        assert!(source_rx.try_recv().is_err());
        assert!(matches!(
            observer_rx.recv_timeout(Duration::from_secs(1)),
            Ok(ServerMessage::TaskStateChanged { revision: 1 })
        ));
    }

    #[test]
    fn websocket_task_state_changes_reach_another_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TaskStateBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let observer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let source_revisions = source.subscribe_task_state();
        let observer_revisions = observer.subscribe_task_state();
        let session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let session_id = session.id;

        assert!(matches!(
            source
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: Vec::new(),
                        live_session_ids: vec![session_id],
                        sessions: vec![session],
                    },
                )
                .unwrap(),
            ResponsePayload::TaskStateSaved { .. }
        ));
        assert_eq!(
            observer_revisions.recv_timeout(Duration::from_secs(1)),
            Ok(1)
        );
        assert!(source_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = observer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);

        // Streaming checkpoints update transcript detail and `updated_at`,
        // but do not change anything rendered in another client's task
        // catalog. They must not trigger a list reload for every stream save.
        let mut checkpoint = sessions[0].clone();
        checkpoint.updated_at = checkpoint.updated_at.saturating_add(1);
        source
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    live_session_ids: vec![session_id],
                    sessions: vec![checkpoint],
                },
            )
            .unwrap();
        assert!(
            observer_revisions
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        // Desktop persistence uses fire-and-forget notifications, while Web
        // uses requests. Both directions must wake the other application's
        // catalog without echoing back to the source connection.
        let second = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let second_id = second.id;
        observer
            .notify(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    live_session_ids: vec![session_id, second_id],
                    sessions: vec![second],
                },
            )
            .unwrap();
        assert_eq!(source_revisions.recv_timeout(Duration::from_secs(1)), Ok(2));
        assert!(observer_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = source
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.id == second_id));

        source.shutdown();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn isolated_daemon_saves_reopens_and_stops_without_touching_original_resources() {
        let root = std::env::temp_dir().join(format!("waku-daemon-isolation-{}", Uuid::new_v4()));
        let test_root = root.join("test");
        let original_root = root.join("original");
        std::fs::create_dir_all(&test_root).unwrap();
        std::fs::create_dir_all(&original_root).unwrap();
        let original_settings = original_root.join("settings.json");
        let original_database = original_root.join("app.db");
        std::fs::write(&original_settings, "original settings").unwrap();
        std::fs::write(&original_database, "original database").unwrap();
        let mut original_process = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let project = Project::from_path(test_root.join("workspace"));
            std::fs::create_dir_all(&project.path).unwrap();
            let mut session = AgentSession::new(project.id, ProviderKind::Codex);
            session.title = "isolated saved session".into();
            session.begin_turn("isolated input");
            for first_start in [true, false] {
                let backend = WakuBackend::new(
                    DaemonSettingsStore::open(test_root.join("settings.json")).unwrap(),
                    StateStore::daemon(test_root.join("app.db")),
                )
                .unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let address = listener.local_addr().unwrap();
                let shutdown = Arc::new(AtomicBool::new(false));
                let server_shutdown = shutdown.clone();
                let server = std::thread::spawn(move || {
                    serve(
                        listener,
                        "isolated-secret".into(),
                        Arc::new(backend),
                        server_shutdown,
                        ServerOptions {
                            allow_shutdown: true,
                            ..ServerOptions::default()
                        },
                    )
                    .unwrap()
                });
                let round_trip = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let client =
                        DaemonClient::connect(&address.to_string(), "isolated-secret".into())
                            .unwrap();
                    if first_start {
                        assert!(matches!(
                            client
                                .request(
                                    Uuid::nil(),
                                    Uuid::nil(),
                                    Command::SaveTaskState {
                                        projects: vec![project.clone()],
                                        live_session_ids: vec![session.id],
                                        sessions: vec![session.clone()],
                                    }
                                )
                                .unwrap(),
                            ResponsePayload::TaskStateSaved { .. }
                        ));
                        client
                            .request(
                                Uuid::nil(),
                                Uuid::nil(),
                                Command::UpdateSettings {
                                    settings: DaemonSettings {
                                        disabled_providers: vec![ProviderKind::Pi],
                                        ..DaemonSettings::default()
                                    },
                                },
                            )
                            .unwrap();
                    }
                    let ResponsePayload::Session {
                        session: Some(saved),
                    } = client
                        .request(
                            session.id,
                            Uuid::nil(),
                            Command::HydrateSession {
                                session_id: session.id,
                            },
                        )
                        .unwrap()
                    else {
                        panic!("saved session must remain readable");
                    };
                    assert_eq!(saved.title, "isolated saved session");
                    assert_eq!(
                        serde_json::to_value(&saved.messages).unwrap(),
                        serde_json::to_value(&session.messages).unwrap()
                    );
                    let ResponsePayload::Settings { settings } = client
                        .request(Uuid::nil(), Uuid::nil(), Command::GetSettings)
                        .unwrap()
                    else {
                        panic!("expected daemon settings");
                    };
                    assert_eq!(settings.disabled_providers, [ProviderKind::Pi]);
                    client.shutdown();
                }));
                shutdown.store(true, Ordering::Release);
                server.join().unwrap();
                if let Err(error) = round_trip {
                    std::panic::resume_unwind(error);
                }
                assert!(original_process.try_wait().unwrap().is_none());
                assert_eq!(
                    std::fs::read_to_string(&original_settings).unwrap(),
                    "original settings"
                );
                assert_eq!(
                    std::fs::read_to_string(&original_database).unwrap(),
                    "original database"
                );
            }
        }));
        let _ = original_process.kill();
        original_process.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[cfg(unix)]
    #[test]
    fn stale_projection_cannot_resurrect_a_removed_session() {
        let root = std::env::temp_dir().join(format!("waku-remove-race-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let stale_client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let remover = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let project = Project::from_path(root.join("repo"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("persist me");
        stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    live_session_ids: vec![session.id],
                    sessions: vec![session.clone()],
                },
            )
            .unwrap();
        remover
            .request(session.id, Uuid::nil(), Command::RemoveSession)
            .unwrap();
        let ResponsePayload::TaskStateSaved { sessions } = stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![session.id],
                    sessions: vec![session],
                },
            )
            .unwrap()
        else {
            panic!("expected task-state save response");
        };
        assert!(sessions.is_empty());
        let ResponsePayload::TaskState { sessions, .. } = stale_client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected task state");
        };
        assert!(sessions.is_empty());

        stale_client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_daemon_acknowledges_safe_exit_before_closing_the_connection() {
        let root = std::env::temp_dir().join(format!("waku-exit-ack-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("state.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopping = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                stopping,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown)
                .unwrap(),
            ResponsePayload::Ack
        ));
        assert!(!shutdown.load(Ordering::Acquire));
        let error = client
            .request(
                Uuid::new_v4(),
                Uuid::new_v4(),
                Command::Prompt {
                    prompt: "must not start".into(),
                    turn_id: None,
                    message_id: None,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("shutting down"));
        assert!(matches!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::ShutdownDaemon)
                .unwrap(),
            ResponsePayload::Ack
        ));
        server.join().unwrap();
        assert!(shutdown.load(Ordering::Acquire));
        drop(client);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn service_owned_daemon_rejects_global_exit_commands_and_stays_available() {
        let root = std::env::temp_dir().join(format!("waku-exit-ownership-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("state.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopping = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                stopping,
                ServerOptions::default(),
            )
            .unwrap()
        });
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let external =
            waku_client::DaemonSupervisor::connect(&address.to_string(), "secret".into()).unwrap();
        external.prepare_shutdown().unwrap();
        external.finish_shutdown().unwrap();
        for command in [Command::PrepareShutdown, Command::ShutdownDaemon] {
            let error = client
                .request(Uuid::nil(), Uuid::nil(), command)
                .unwrap_err();
            assert!(error.to_string().contains("service owner"));
        }
        assert!(matches!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::GetSettings)
                .unwrap(),
            ResponsePayload::Settings { .. }
        ));
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
        drop(client);
        drop(external);
        std::fs::remove_dir_all(root).unwrap();
    }

    struct HistoryBackend {
        inner: WakuBackend,
        sink: Mutex<Option<EventSink>>,
    }

    impl Backend for HistoryBackend {
        fn persist_events(&self, events: &[SequencedEvent]) -> anyhow::Result<bool> {
            self.inner.persist_events(events)
        }
        fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
            if matches!(request.command, Command::Start { .. }) {
                *self.sink.lock() = Some(events);
                return Ok(ResponsePayload::Started {
                    supports_steer: false,
                });
            }
            self.inner.handle(request, events)
        }
    }

    #[test]
    fn pruned_snapshot_larger_than_wire_limit_restores_over_the_real_socket() {
        let root = std::env::temp_dir().join(format!("waku-large-history-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = StateStore::daemon(root.join("app.db"));
        let mut state = crate::persistence::PersistedState::fresh(root.join("workspace"));
        state.sessions[0].begin_turn("large preserved history");
        state.sessions[0].push_message(
            crate::model::MessageRole::Assistant,
            "界".repeat(MAX_WIRE_MESSAGE_BYTES / 3 + 1024),
        );
        let session_id = state.sessions[0].id;
        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let cursor = crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 20_001,
        };
        state.sessions[0].runtime_event_cursor = Some(cursor);
        state.sessions[0].history_saved_cursor = Some(cursor);
        let events = (1..=20_001)
            .map(|sequence| SequencedEvent {
                session_id,
                runtime_id,
                epoch,
                sequence,
                event: WireDriverEvent::new("textDelta", json!("x")),
            })
            .collect::<Vec<_>>();
        store.save_events(&mut state, &events).unwrap();
        drop(state);
        drop(store);
        let backend = Arc::new(
            WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("app.db")),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "fixture".into(),
                backend,
                stop,
                ServerOptions::default(),
            )
            .unwrap()
        });
        let result = std::panic::catch_unwind(|| {
            let client = DaemonClient::connect(&address.to_string(), "fixture".into()).unwrap();
            let ResponsePayload::HistorySnapshot { session } = client
                .request(
                    session_id,
                    runtime_id,
                    Command::ReplayEvents {
                        cursor: ReplayCursor {
                            session_id,
                            runtime_id,
                            epoch,
                            sequence: 0,
                        },
                    },
                )
                .unwrap()
            else {
                panic!("missing large snapshot");
            };
            assert_eq!(session.history_saved_cursor, Some(cursor));
            assert_eq!(
                session.messages.last().unwrap().content.len(),
                MAX_WIRE_MESSAGE_BYTES + 3072
            );
            assert!(session.messages.last().unwrap().content.ends_with("界"));
            drop(session);
            let ResponsePayload::Session {
                session: Some(session),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession { session_id },
                )
                .unwrap()
            else {
                panic!("missing large details");
            };
            assert_eq!(
                session.messages.last().unwrap().content.len(),
                MAX_WIRE_MESSAGE_BYTES + 3072
            );
            drop(session);
            assert!(
                matches!(client.request(session_id, runtime_id, Command::ReplayEvents { cursor: ReplayCursor { session_id, runtime_id, epoch, sequence: 20_001 } }).unwrap(), ResponsePayload::EventReplay { events } if events.is_empty())
            );
        });
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
    }

    #[test]
    fn daemon_preserves_unviewed_provider_history_across_database_reopen() {
        let root = std::env::temp_dir().join(format!("waku-history-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = Arc::new(HistoryBackend {
            inner: WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("app.db")),
            )
            .unwrap(),
            sink: Mutex::new(None),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server_backend = backend.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                server_backend,
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
            let project = Project::from_path(root.join("workspace"));
            let session = AgentSession::new(project.id, ProviderKind::Codex);
            let session_id = session.id;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![project],
                        live_session_ids: vec![session_id],
                        sessions: vec![session],
                    },
                )
                .unwrap();
            let runtime_id = Uuid::new_v4();
            client
                .request(
                    session_id,
                    runtime_id,
                    Command::Start {
                        options: WireDriverStartOptions {
                            provider: "codex".into(),
                            binary: "unused".into(),
                            cwd: root.clone(),
                            mode: "fullAccess".into(),
                            model: None,
                            reasoning_effort: None,
                            service_tier: None,
                            context_window: None,
                            agent_preset: None,
                            computer_use_enabled: false,
                            provider_cursor: None,
                        },
                    },
                )
                .unwrap();
            let sink = backend.sink.lock().clone().unwrap();
            // No session subscription or desktop reducer participates in this turn.
            for event in [
                WireDriverEvent::new(
                    "promptSubmitted",
                    json!({"message":"inspect", "turnId":Uuid::new_v4(), "messageId":Uuid::new_v4()}),
                ),
                WireDriverEvent::new("turnStarted", json!(null)),
                WireDriverEvent::new("textDelta", json!("before")),
                WireDriverEvent::new(
                    "activity",
                    json!({"id":"tool", "kind":"command", "title":"read", "detail":null, "complete":true}),
                ),
                WireDriverEvent::new("textDelta", json!("after")),
                WireDriverEvent::new("turnFinished", json!({"success":true,"summary":null})),
            ] {
                sink.send(event).unwrap();
            }
            let ResponsePayload::Session {
                session: Some(mut stale),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession { session_id },
                )
                .unwrap()
            else {
                panic!("missing saved session");
            };
            let saved_cursor = stale.runtime_event_cursor.unwrap();
            stale.updated_at += 100;
            stale.title = "renamed".into();
            stale.messages.clear();
            stale.transcript_blocks.clear();
            stale.turns.clear();
            // Even an equal cursor and a newer metadata timestamp cannot
            // authorize a client to replace daemon-owned history.
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: Vec::new(),
                        live_session_ids: vec![session_id],
                        sessions: vec![stale],
                    },
                )
                .unwrap();
            let late = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
            let replay = late.subscribe(session_id, runtime_id);
            let mut replayed = Vec::new();
            loop {
                let event = replay.recv_timeout(Duration::from_secs(2)).unwrap();
                if event.event.kind == "historyPersistence" {
                    assert_eq!(event.sequence, 6);
                    break;
                }
                replayed.push(event.sequence);
            }
            assert_eq!(replayed, [1, 2, 3, 4, 5, 6]);
            let resumed = DaemonClient::connect_with_resume(
                &address.to_string(),
                "secret".into(),
                vec![ReplayCursor {
                    session_id,
                    runtime_id,
                    epoch: saved_cursor.epoch,
                    sequence: saved_cursor.sequence,
                }],
            )
            .unwrap();
            let resumed_events = resumed.subscribe(session_id, runtime_id);
            let acknowledgment = resumed_events.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(acknowledgment.event.kind, "historyPersistence");
            assert_eq!(acknowledgment.sequence, 6);
            assert!(
                resumed_events
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            let store = StateStore::daemon(root.join("app.db"));
            let mut loaded = store.load().unwrap();
            let restored = loaded
                .sessions
                .iter_mut()
                .find(|item| item.id == session_id)
                .expect("daemon must save a session even when no viewer reduces its events");
            store.hydrate(restored).unwrap();
            assert_eq!(
                restored
                    .messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>(),
                ["inspect", "before", "after"]
            );
            assert_eq!(restored.transcript_blocks[0].after_message, 2);
            assert_eq!(restored.title, "renamed");
            assert_eq!(
                restored.turns[0].status,
                crate::model::TurnStatus::Completed
            );
            assert_eq!(restored.runtime_event_cursor.unwrap().sequence, 6);
            assert_eq!(restored.history_saved_cursor, restored.runtime_event_cursor);
            // Cross the old in-memory replay limit with real persisted events.
            sink.send(WireDriverEvent::new("turnStarted", json!(null)))
                .unwrap();
            sink.send_batch(
                (0..4_200)
                    .map(|_| WireDriverEvent::new("textDelta", json!("x")))
                    .collect(),
            )
            .unwrap();
            let tail_client = DaemonClient::connect_with_resume(
                &address.to_string(),
                "secret".into(),
                vec![ReplayCursor {
                    session_id,
                    runtime_id,
                    epoch: saved_cursor.epoch,
                    sequence: saved_cursor.sequence,
                }],
            )
            .unwrap();
            let tail = tail_client.subscribe(session_id, runtime_id);
            let mut sequences = Vec::new();
            loop {
                let event = tail.recv_timeout(Duration::from_secs(5)).unwrap();
                if event.event.kind == "historyPersistence" {
                    break;
                }
                sequences.push(event.sequence);
            }
            assert_eq!(sequences.len(), 4_201);
            assert_eq!(sequences.first(), Some(&7));
            assert_eq!(sequences.last(), Some(&4_207));
            sink.send(WireDriverEvent::new("textDelta", json!("tail")))
                .unwrap();
            assert_eq!(
                tail.recv_timeout(Duration::from_secs(2)).unwrap().sequence,
                4_208
            );
            assert_eq!(
                tail.recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .event
                    .kind,
                "historyPersistence"
            );
            let ResponsePayload::Session {
                session: Some(current),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession { session_id },
                )
                .unwrap()
            else {
                panic!("missing current history");
            };
            assert_eq!(
                current.messages.last().unwrap().content,
                format!("{}tail", "x".repeat(4_200))
            );
            let fault = rusqlite::Connection::open(root.join("app.db")).unwrap();
            fault.execute_batch("CREATE TRIGGER fail_history BEFORE INSERT ON session_events BEGIN SELECT RAISE(ABORT, 'controlled history failure'); END;").unwrap();
            assert!(
                sink.send(WireDriverEvent::new("textDelta", json!(" unsaved")))
                    .is_err()
            );
            assert_eq!(
                tail.recv_timeout(Duration::from_secs(2)).unwrap().sequence,
                4_209
            );
            let failed = tail.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(failed.event.kind, "historyPersistence");
            assert!(
                failed.event.payload["error"]
                    .as_str()
                    .unwrap()
                    .contains("controlled history failure")
            );
            let failure_store = StateStore::daemon(root.join("app.db"));
            let mut failure_state = failure_store.load().unwrap();
            let durable = failure_state
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
                .unwrap();
            failure_store.hydrate(durable).unwrap();
            assert_eq!(
                durable.messages.last().unwrap().content,
                format!("{}tail", "x".repeat(4_200))
            );
            assert_eq!(durable.history_saved_cursor.unwrap().sequence, 4_208);
            fault.execute_batch("DROP TRIGGER fail_history").unwrap();
            sink.send(WireDriverEvent::new("textDelta", json!(" recovered")))
                .unwrap();
            assert_eq!(
                tail.recv_timeout(Duration::from_secs(2)).unwrap().sequence,
                4_210
            );
            let recovered = tail.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(recovered.event.payload["error"].is_null());
            let ResponsePayload::Session {
                session: Some(current),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession { session_id },
                )
                .unwrap()
            else {
                panic!("missing recovered history");
            };
            assert_eq!(
                current.messages.last().unwrap().content,
                format!("{}tail unsaved recovered", "x".repeat(4_200))
            );
            assert_eq!(current.history_saved_cursor.unwrap().sequence, 4_210);
            // A late subscriber crosses persisted pruning as well as the hot window.
            sink.send_batch(
                (0..16_000)
                    .map(|_| WireDriverEvent::new("textDelta", json!("x")))
                    .collect(),
            )
            .unwrap();
            let pruned_client =
                DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
            let pruned = pruned_client.subscribe(session_id, runtime_id);
            let snapshot = pruned.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(snapshot.event.kind, "historySnapshot");
            assert_eq!(snapshot.sequence, 20_210);
            let mut restored: crate::model::AgentSession =
                serde_json::from_value(snapshot.event.payload).unwrap();
            assert_eq!(
                restored.messages.last().unwrap().content,
                format!(
                    "{}{}",
                    current.messages.last().unwrap().content,
                    "x".repeat(16_000)
                )
            );
            assert_eq!(
                pruned
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .event
                    .kind,
                "historyPersistence"
            );
            sink.send(WireDriverEvent::new("textDelta", json!(" after snapshot")))
                .unwrap();
            let tail = pruned.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(tail.sequence, 20_211);
            assert_eq!(tail.event.kind, "textDelta");
            let mut reducer = waku_protocol::history::HistoryReducer::default();
            let snapshot = restored.clone();
            reducer.apply(
                &mut restored,
                crate::model::DriverEvent::HistorySnapshot(Box::new(snapshot)),
            );
            reducer.apply(
                &mut restored,
                waku_protocol::event_from_wire(tail.event).unwrap(),
            );
            let ResponsePayload::Session {
                session: Some(current),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession { session_id },
                )
                .unwrap()
            else {
                panic!("missing tail after snapshot");
            };
            assert_eq!(restored.messages.len(), current.messages.len());
            assert_eq!(
                restored.messages.last().unwrap().content,
                current.messages.last().unwrap().content
            );
            let reopened_backend = WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("app.db")),
            )
            .unwrap();
            let mut stale = current.clone();
            stale.runtime_event_cursor = None;
            stale.history_saved_cursor = None;
            stale.updated_at += 100;
            stale.messages.clear();
            stale.turns.clear();
            stale.transcript_blocks.clear();
            reopened_backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: Uuid::nil(),
                        runtime_id: Uuid::nil(),
                        command: Command::SaveTaskState {
                            projects: Vec::new(),
                            live_session_ids: vec![session_id],
                            sessions: vec![stale],
                        },
                    },
                    sink.clone(),
                )
                .unwrap();
            let ResponsePayload::Session {
                session: Some(reopened),
            } = reopened_backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: Uuid::nil(),
                        runtime_id: Uuid::nil(),
                        command: Command::HydrateSession { session_id },
                    },
                    sink,
                )
                .unwrap()
            else {
                panic!("missing history after backend reopen");
            };
            assert_eq!(
                reopened.messages.last().unwrap().content,
                current.messages.last().unwrap().content
            );
            assert_eq!(reopened.history_saved_cursor, current.history_saved_cursor);
        }));
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
    }

    #[test]
    fn websocket_round_trip_sequences_provider_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        assert!(
            DaemonClient::connect(&address.to_string(), "wrong-secret".into()).is_err(),
            "the server must reject a client before it can issue requests"
        );
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let response = client
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();
        assert!(matches!(
            response,
            ResponsePayload::Started {
                supports_steer: true
            }
        ));
        // Start can emit before a refreshed app discovers and subscribes to
        // the daemon-owned runtime. The client must retain that replay.
        let events = client.subscribe(session_id, runtime_id);
        let event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(event.runtime_id, runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "connected");

        client.shutdown();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
        server.join().unwrap();
    }

    #[test]
    fn late_client_attaches_to_replay_and_live_runtime_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let source_events = source.subscribe(session_id, runtime_id);
        source
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            source_events
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .event
                .kind,
            "connected"
        );

        let late = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            late.request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));
        let late_events = late.subscribe(session_id, runtime_id);
        let replayed = late_events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(replayed.sequence, 1);
        assert_eq!(replayed.event.kind, "connected");

        source
            .request(
                session_id,
                runtime_id,
                Command::Prompt {
                    prompt: "streamed from the first client".into(),
                    turn_id: None,
                    message_id: None,
                },
            )
            .unwrap();
        for events in [&source_events, &late_events] {
            let live = events.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(live.sequence, 2);
            assert_eq!(live.event.kind, "textDelta");
            assert_eq!(live.event.payload, json!("streamed from the first client"));
        }

        source
            .request(session_id, runtime_id, Command::CloseSession)
            .unwrap();
        let after_close = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            after_close
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: None,
                supports_steer: true,
            }
        ));
        let stale_events = after_close.subscribe(session_id, runtime_id);
        assert!(
            stale_events
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "an explicitly closed runtime must not replay into future clients"
        );

        source.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn remote_supervisor_reconnects_without_losing_the_daemon_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Keep the address reserved between servers. If the first listener is
        // dropped before the replacement binds, a parallel test can claim the
        // newly freed ephemeral port and make this test fail with AddrInUse.
        let replacement_listener = listener.try_clone().unwrap();
        let address = listener.local_addr().unwrap();
        let backend = Arc::new(TestBackend::default());
        let first_shutdown = Arc::new(AtomicBool::new(false));
        let first_server = {
            let backend = backend.clone();
            let shutdown = first_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    listener,
                    "secret".into(),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let supervisor = DaemonSupervisor::connect(&address.to_string(), "secret".into()).unwrap();
        let clients = supervisor.subscribe_clients();
        let initial = clients.recv_timeout(Duration::from_secs(1)).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        initial
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();

        first_shutdown.store(true, Ordering::Release);
        first_server.join().unwrap();

        let second_shutdown = Arc::new(AtomicBool::new(false));
        let second_server = {
            let backend = backend.clone();
            let shutdown = second_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    replacement_listener,
                    "secret".into(),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let replacement = clients.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!initial.same_connection(&replacement));
        assert!(matches!(
            replacement
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));

        second_shutdown.store(true, Ordering::Release);
        second_server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_idle_terminal_does_not_wait_for_output() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let terminal = crate::terminal::DaemonTerminal::open(
            &root,
            80,
            24,
            hub.event_sink(Uuid::new_v4(), Uuid::new_v4()),
        )
        .unwrap();
        let (dropped, finished) = bounded(1);
        std::thread::spawn(move || {
            drop(terminal);
            let _ = dropped.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(3)).is_ok(),
            "dropping an idle daemon terminal blocked on its output reader"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_round_trip_streams_input_and_output() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        assert!(matches!(
            client
                .request(
                    terminal_id,
                    terminal_id,
                    Command::OpenTerminal {
                        cwd: root.clone(),
                        cols: 80,
                        rows: 24,
                    },
                )
                .unwrap(),
            ResponsePayload::Ack
        ));
        client
            .request(
                terminal_id,
                terminal_id,
                Command::WriteTerminal {
                    data: b"waku-terminal-round-trip\r".to_vec(),
                },
            )
            .unwrap();

        // The raw test client intentionally does not emulate wterm's replies
        // to terminal capability queries. The PTY's local echo is enough to
        // prove that daemon-side input and output both crossed the WebSocket.
        let marker = b"waku-terminal-round-trip";
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut seen_events = Vec::new();
        while std::time::Instant::now() < deadline
            && !output.windows(marker.len()).any(|window| window == marker)
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(event) = events.recv_timeout(remaining) else {
                break;
            };
            seen_events.push(event.event.kind.clone());
            if event.event.kind != "terminalOutput" {
                continue;
            }
            let data = event.event.payload["data"].as_str().unwrap();
            output.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            );
        }
        assert!(
            output.windows(marker.len()).any(|window| window == marker),
            "daemon terminal did not return the shell marker; events={seen_events:?}, output={}",
            String::from_utf8_lossy(&output)
        );
        assert!(matches!(
            client
                .request(terminal_id, terminal_id, Command::CloseTerminal)
                .unwrap(),
            ResponsePayload::Ack
        ));

        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nil_request_ids_execute_without_responses_or_cache_entries() {
        let (outgoing, responses) = unbounded();
        let hub = Arc::new(Hub::default());
        let handled = handle_request(
            Request {
                request_id: Uuid::nil(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::GetSettings,
            },
            outgoing,
            0,
            Arc::new(TestBackend::default()),
            hub.clone(),
        );

        assert!(handled.executed);
        assert!(matches!(handled.outcome, ResponseOutcome::Ok { .. }));
        assert!(responses.try_recv().is_err());
        assert!(hub.cached_response(Uuid::nil()).is_none());
    }

    #[test]
    fn browser_origins_are_denied_unless_explicitly_allowed() {
        let request = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://app.waku.test")
            .body(())
            .unwrap();
        let response = HandshakeResponse::new(());
        assert_eq!(
            validate_handshake(&request, response, &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );

        let allowed = HashSet::from(["https://app.waku.test".to_owned()]);
        assert!(validate_handshake(&request, HandshakeResponse::new(()), &allowed).is_ok());
        let native = HandshakeRequest::builder().uri("/v1").body(()).unwrap();
        assert!(validate_handshake(&native, HandshakeResponse::new(()), &HashSet::new()).is_ok());

        let react_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "http://192.168.0.114:34125")
            .header(NATIVE_CLIENT_HEADER, NATIVE_CLIENT_HEADER_VALUE)
            .body(())
            .unwrap();
        assert!(
            validate_handshake(&react_native, HandshakeResponse::new(()), &HashSet::new()).is_ok()
        );

        let forged_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://attacker.example")
            .header(NATIVE_CLIENT_HEADER, "browser")
            .body(())
            .unwrap();
        assert_eq!(
            validate_handshake(&forged_native, HandshakeResponse::new(()), &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn daemon_tokens_require_an_exact_match() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "Secret"));
        assert!(!token_matches("secret", "secret-extra"));
    }

    #[test]
    fn replaced_runtime_ignores_late_events_from_the_old_generation() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let old_runtime_id = Uuid::new_v4();
        let new_runtime_id = Uuid::new_v4();
        let (outgoing, events) = unbounded();
        hub.subscribe(&[], outgoing);

        hub.begin_runtime(session_id, old_runtime_id);
        let old_sink = hub.event_sink(session_id, old_runtime_id);
        old_sink
            .send(WireDriverEvent::new("old", serde_json::Value::Null))
            .unwrap();
        assert!(matches!(
            events.recv().unwrap(),
            ServerMessage::Event(event) if event.runtime_id == old_runtime_id
        ));

        hub.begin_runtime(session_id, new_runtime_id);
        old_sink
            .send(WireDriverEvent::new("stale", serde_json::Value::Null))
            .unwrap();
        hub.event_sink(session_id, new_runtime_id)
            .send(WireDriverEvent::new("new", serde_json::Value::Null))
            .unwrap();

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected a daemon event");
        };
        assert_eq!(event.runtime_id, new_runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "new");
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn replacing_runtime_drains_old_history_before_changing_its_identity() {
        struct DrainingBackend(EventSink);
        impl Backend for DrainingBackend {
            fn prepare_start(
                &self,
                _: Uuid,
                _: &crate::WireDriverStartOptions,
            ) -> anyhow::Result<()> {
                self.0.send(WireDriverEvent::new(
                    "processExited",
                    serde_json::Value::Null,
                ))
            }
            fn handle(&self, _: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
                Ok(ResponsePayload::Started {
                    supports_steer: false,
                })
            }
        }
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let old_runtime = Uuid::new_v4();
        let new_runtime = Uuid::new_v4();
        let (outgoing, received) = unbounded();
        hub.subscribe(&[], outgoing.clone());
        hub.begin_runtime(session_id, old_runtime);
        let backend = Arc::new(DrainingBackend(hub.event_sink(session_id, old_runtime)));
        let result = handle_request(
            Request {
                request_id: Uuid::new_v4(),
                session_id,
                runtime_id: new_runtime,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            outgoing,
            0,
            backend,
            hub.clone(),
        );
        assert!(matches!(result.outcome, ResponseOutcome::Ok { .. }));
        let ServerMessage::Event(exit) = received.recv().unwrap() else {
            panic!("missing final old-runtime event")
        };
        assert_eq!(exit.runtime_id, old_runtime);
        assert_eq!(exit.event.kind, "processExited");
        assert_eq!(
            hub.state.lock().active_runtimes.get(&session_id),
            Some(&new_runtime)
        );
    }

    #[test]
    fn replay_cursor_from_an_old_daemon_epoch_does_not_hide_new_events() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        hub.begin_runtime(session_id, runtime_id);
        hub.event_sink(session_id, runtime_id)
            .send(WireDriverEvent::new("new-daemon", serde_json::Value::Null))
            .unwrap();

        let (outgoing, events) = unbounded();
        hub.subscribe(
            &[ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::nil(),
                sequence: u64::MAX,
            }],
            outgoing,
        );

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected the new daemon event to replay");
        };
        assert_eq!(event.epoch, hub.epoch);
        assert_eq!(event.sequence, 1);
    }

    struct BlockingProbeBackend {
        probe_started: Sender<()>,
        release_probe: Receiver<()>,
    }

    impl Backend for BlockingProbeBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            if matches!(request.command, Command::ProbeProvider { .. }) {
                self.probe_started.send(()).unwrap();
                self.release_probe.recv().unwrap();
            }
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn slow_background_command_does_not_block_session_hydration() {
        let (outgoing, response_rx) = unbounded();
        let (probe_started, probe_started_rx) = bounded(1);
        let (release_probe, release_probe_rx) = bounded(1);
        let backend: Arc<dyn Backend> = Arc::new(BlockingProbeBackend {
            probe_started,
            release_probe: release_probe_rx,
        });
        let hub = Arc::new(Hub::default());
        let dispatcher = RequestDispatcher::new(backend, hub);

        let probe_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: probe_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::ProbeProvider {
                    provider: crate::model::ProviderKind::Codex,
                    binary_override: None,
                    discover_models: false,
                    probe_version: false,
                },
            },
            outgoing.clone(),
            0,
        );
        probe_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let hydration_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: hydration_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::HydrateSession {
                    session_id: Uuid::new_v4(),
                },
            },
            outgoing,
            0,
        );
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == hydration_id
        ));

        release_probe.send(()).unwrap();
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == probe_id
        ));
    }

    struct RuntimeOrderingBackend {
        blocked_session_id: Uuid,
        handled: Sender<(Uuid, &'static str)>,
        release_start: Receiver<()>,
    }

    impl Backend for RuntimeOrderingBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            let command = match request.command {
                Command::Start { .. } => {
                    self.handled.send((request.session_id, "start")).unwrap();
                    if request.session_id == self.blocked_session_id {
                        self.release_start.recv().unwrap();
                    }
                    return Ok(ResponsePayload::Started {
                        supports_steer: true,
                    });
                }
                Command::Prompt { .. } => "prompt",
                Command::CloseSession => "close",
                _ => "other",
            };
            self.handled.send((request.session_id, command)).unwrap();
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn runtime_commands_are_ordered_per_session_without_blocking_other_sessions() {
        let blocked_session_id = Uuid::new_v4();
        let blocked_runtime_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        let other_runtime_id = Uuid::new_v4();
        let (handled, handled_rx) = unbounded();
        let (release_start, release_start_rx) = bounded(1);
        let dispatcher = RequestDispatcher::new(
            Arc::new(RuntimeOrderingBackend {
                blocked_session_id,
                handled,
                release_start: release_start_rx,
            }),
            Arc::new(Hub::default()),
        );
        let (start_outgoing, start_responses) = unbounded();
        let (second_client_outgoing, second_client_responses) = unbounded();
        let (other_outgoing, other_responses) = unbounded();

        let blocked_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_start_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            start_outgoing,
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "start")
        );

        let prompt_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: prompt_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Prompt {
                    prompt: "after start".into(),
                    turn_id: None,
                    message_id: None,
                },
            },
            second_client_outgoing,
            0,
        );

        let other_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_start_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            other_outgoing.clone(),
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (other_session_id, "start")
        );
        assert!(matches!(
            other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == other_start_id
        ));
        assert!(handled_rx.recv_timeout(Duration::from_millis(50)).is_err());

        release_start.send(()).unwrap();
        assert!(matches!(
            start_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == blocked_start_id
        ));
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "prompt")
        );
        assert!(matches!(
            second_client_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == prompt_id
        ));

        let blocked_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_close_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing.clone(),
            0,
        );
        let other_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_close_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing,
            0,
        );
        let mut close_responses = [false; 2];
        for _ in 0..2 {
            let ServerMessage::Response { request_id, .. } = other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
            else {
                panic!("expected a close response");
            };
            if request_id == blocked_close_id {
                close_responses[0] = true;
            } else if request_id == other_close_id {
                close_responses[1] = true;
            }
        }
        assert_eq!(close_responses, [true, true]);
    }

    fn test_start_options() -> WireDriverStartOptions {
        WireDriverStartOptions {
            provider: "codex".into(),
            binary: PathBuf::from("codex"),
            cwd: PathBuf::from("."),
            mode: "fullAccess".into(),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
        }
    }
}

#[cfg(all(test, unix))]
#[path = "session_creation_tests.rs"]
mod session_creation_tests;
