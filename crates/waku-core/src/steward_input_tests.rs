use super::*;
use crate::driver::DriverControl;
use crate::model::{InputConfirmation, InputDeliveryOutcome, InputDeliveryState};
use crate::server::{ServerOptions, serve};
use std::net::TcpListener;
use std::time::Duration;
use waku_client::DaemonClient;

struct QueueDriver {
    events: EventSink,
    calls: crossbeam_channel::Sender<(Uuid, String)>,
    target: Uuid,
    steer: bool,
    callback_gate: Arc<Mutex<Option<crossbeam_channel::Receiver<()>>>>,
}

impl DriverControl for QueueDriver {
    fn prompt(&self, prompt: String) {
        if prompt.contains("automatic child-session notification") || prompt.contains("automatic decision notification") {
            let gate = self.callback_gate.lock().clone();
            if let Some(gate) = gate {
                self.calls.send((self.target, prompt)).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        self.events
            .send(event_to_wire(DriverEvent::TurnStarted).unwrap())
            .unwrap();
    }
    fn deliver_input(&self, prompt: String, id: Uuid, steer: bool) -> anyhow::Result<()> {
        assert!(
            !steer || self.steer,
            "unsupported provider must never receive native steer"
        );
        if !steer {
            self.events.send(event_to_wire(DriverEvent::TurnStarted)?)?;
        }
        if prompt != "lose confirmation"
            && !(prompt.starts_with("[Waku manager decision]") && prompt.contains("lose confirmation"))
            && !prompt.contains("\"instruction\":\"lose confirmation\"")
        {
            self.events
                .send(event_to_wire(DriverEvent::InputDeliveryOutcome(
                    InputDeliveryOutcome {
                        id,
                        state: InputDeliveryState::Received,
                        confirmation: Some(InputConfirmation::Transport),
                        reason: None,
                    },
                ))?)?;
        }
        self.calls.send((self.target, prompt)).unwrap();
        Ok(())
    }
    fn supports_steer(&self) -> bool {
        self.steer
    }
    fn cancel(&self) {}
    fn respond(&self, _: String, _: String) {}
    fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        unreachable!()
    }
}

// Replace only provider process creation; authorization, socket dispatch, storage,
// turn projection, and queue scheduling remain the production implementations.
struct QueueBackend {
    inner: Arc<WakuBackend>,
    sinks: Mutex<HashMap<Uuid, EventSink>>,
    calls: crossbeam_channel::Sender<(Uuid, String)>,
    paused: AtomicBool,
    steer: AtomicBool,
    callback_gate: Arc<Mutex<Option<crossbeam_channel::Receiver<()>>>>,
}
impl Backend for QueueBackend {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        if matches!(request.command, Command::Start { .. }) {
            self.inner.sessions.lock().insert(
                request.session_id,
                (
                    request.runtime_id,
                    DriverHandle::from_control(Arc::new(QueueDriver {
                        events: events.clone(),
                        calls: self.calls.clone(),
                        target: request.session_id,
                        steer: self.steer.load(Ordering::Acquire),
                        callback_gate: self.callback_gate.clone(),
                    })),
                ),
            );
            self.sinks.lock().insert(request.session_id, events.clone());
            events.send(event_to_wire(DriverEvent::Connected {
                provider_cursor: None,
            })?)?;
            return Ok(ResponsePayload::Ack);
        }
        self.inner.handle(request, events)
    }
    fn persist_events(&self, events: &[crate::SequencedEvent]) -> anyhow::Result<bool> {
        self.inner.persist_events(events)
    }
    fn resume_stewards(&self, events: EventSink) {
        // The controlled runtime is attached explicitly after reopening storage.
        if self.paused.load(Ordering::Acquire) || self.sinks.lock().is_empty() {
            return;
        }
        self.inner.resume_stewards(events);
    }
}

struct QueueServer {
    backend: Arc<QueueBackend>,
    address: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    calls: crossbeam_channel::Receiver<(Uuid, String)>,
}
impl QueueServer {
    fn open(root: &Path) -> Self {
        let settings = DaemonSettingsStore::open(root.join("settings.json")).unwrap();
        let mut config = settings.get();
        config.provider_binary_overrides.insert(
            ProviderKind::Codex,
            root.join("controlled-provider")
                .to_string_lossy()
                .into_owned(),
        );
        settings.replace(config).unwrap();
        let (sender, calls) = crossbeam_channel::unbounded();
        let backend = Arc::new(QueueBackend {
            inner: Arc::new(
                WakuBackend::new(settings, StateStore::daemon(root.join("state.db"))).unwrap(),
            ),
            sinks: Mutex::new(HashMap::new()),
            calls: sender,
            paused: AtomicBool::new(false),
            steer: AtomicBool::new(false),
            callback_gate: Arc::new(Mutex::new(None)),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let service = backend.clone();
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            serve(
                listener,
                "queue-test".into(),
                service,
                stopping,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap();
        });
        Self {
            backend,
            address,
            stop,
            thread: Some(thread),
            calls,
        }
    }
    fn connect(&self) -> DaemonClient {
        DaemonClient::connect(&self.address, "queue-test".into()).unwrap()
    }
    fn start(&self, client: &DaemonClient, root: &Path, target: Uuid) -> Uuid {
        let runtime = Uuid::new_v4();
        client
            .request(
                target,
                runtime,
                Command::Start {
                    options: crate::WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: root.join("controlled-provider"),
                        cwd: root.into(),
                        mode: "ask".into(),
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
        runtime
    }
    fn finish(&self, target: Uuid) {
        self.backend
            .sinks
            .lock()
            .get(&target)
            .unwrap()
            .send(
                event_to_wire(DriverEvent::TurnFinished {
                    success: true,
                    summary: None,
                })
                .unwrap(),
            )
            .unwrap();
    }
    fn submit(
        &self,
        client: &DaemonClient,
        parent: Uuid,
        child: Uuid,
        id: Uuid,
        prompt: &str,
    ) -> serde_json::Value {
        serde_json::to_value(
            client
                .request(
                    parent,
                    Uuid::nil(),
                    Command::StewardPrompt {
                        child_session_id: child,
                        prompt: prompt.into(),
                        delivery_id: Some(id),
                    },
                )
                .unwrap(),
        )
        .unwrap()
    }
}
impl Drop for QueueServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
        self.backend.inner.sessions.lock().clear();
    }
}

fn seed_queue() -> (PathBuf, AgentSession, AgentSession) {
    let root = std::env::temp_dir().join(format!("waku-input-queue-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let project = Project::from_path(root.clone());
    let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
    parent.runtime_mode = RuntimeMode::FullAccess;
    parent.begin_turn("Delegate");
    parent.finish_active_turn(crate::model::TurnStatus::Completed);
    let mut child = AgentSession::new(project.id, ProviderKind::Codex);
    child.runtime_mode = RuntimeMode::FullAccess;
    child.parent_session_id = Some(parent.id);
    child.begin_turn("Previous task");
    child.finish_active_turn(crate::model::TurnStatus::Completed);
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap();
    state.projects.push(project);
    state.sessions.extend([parent.clone(), child.clone()]);
    state.mark_session_dirty(parent.id);
    state.mark_session_dirty(child.id);
    store.save(&mut state).unwrap();
    (root, parent, child)
}

#[test]
fn input_queue_socket_orders_feedback_without_a_desktop() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let client = server.connect();
    let runtime = server.start(&client, &root, child.id);
    client
        .request(
            child.id,
            runtime,
            Command::Prompt {
                prompt: "initial".into(),
                turn_id: None,
                message_id: None,
            },
        )
        .unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let queued = server.submit(&client, parent.id, child.id, first, "first feedback");
    assert_eq!(queued["delivery"]["state"], "queued", "{queued}");
    server.submit(&client, parent.id, child.id, first, "first feedback");
    server.submit(&client, parent.id, child.id, second, "second feedback");
    assert!(server.calls.try_recv().is_err());
    drop(client);
    server.finish(child.id);
    assert_eq!(
        server.calls.recv_timeout(Duration::from_secs(5)).unwrap().1,
        "first feedback"
    );
    assert!(server.calls.try_recv().is_err());
    server.finish(child.id);
    assert_eq!(
        server.calls.recv_timeout(Duration::from_secs(5)).unwrap().1,
        "second feedback"
    );
    assert!(server.calls.try_recv().is_err());
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

fn queue_state(client: &DaemonClient, target: Uuid, id: Uuid) -> InputDeliveryState {
    let reply = client
        .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
        .unwrap();
    let json = serde_json::to_value(reply).unwrap();
    let session = json["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == target.to_string())
        .unwrap();
    serde_json::from_value(
        session["input_deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id.to_string())
            .unwrap()["state"]
            .clone(),
    )
    .unwrap()
}

fn begin_queue_work(server: &QueueServer, root: &Path, child: Uuid) -> (DaemonClient, Uuid) {
    let client = server.connect();
    let runtime = server.start(&client, root, child);
    client
        .request(
            child,
            runtime,
            Command::Prompt {
                prompt: "initial".into(),
                turn_id: None,
                message_id: None,
            },
        )
        .unwrap();
    (client, runtime)
}

#[test]
fn input_queue_socket_restart_resumes_pending_and_never_replays_uncertain() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let (client, _) = begin_queue_work(&server, &root, child.id);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    server.submit(&client, parent.id, child.id, first, "lose confirmation");
    server.submit(&client, parent.id, child.id, second, "later feedback");
    drop(client);
    drop(server);
    let reopened = QueueServer::open(&root);
    let client = reopened.connect();
    reopened.start(&client, &root, child.id);
    assert_eq!(
        reopened
            .calls
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .1,
        "lose confirmation"
    );
    assert_eq!(
        queue_state(&client, child.id, first),
        InputDeliveryState::Uncertain
    );
    reopened.finish(child.id);
    assert!(
        reopened
            .calls
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    drop(client);
    drop(reopened);
    let restarted = QueueServer::open(&root);
    let client = restarted.connect();
    restarted.start(&client, &root, child.id);
    assert_eq!(
        queue_state(&client, child.id, first),
        InputDeliveryState::Uncertain
    );
    assert_eq!(
        queue_state(&client, child.id, second),
        InputDeliveryState::Queued
    );
    let repeated = restarted.submit(&client, parent.id, child.id, first, "lose confirmation");
    assert_eq!(repeated["delivery"]["state"], "uncertain");
    assert!(
        restarted
            .calls
            .recv_timeout(Duration::from_millis(150))
            .is_err()
    );
    drop(client);
    drop(restarted);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_queue_socket_revalidates_permissions_and_turn_before_dispatch() {
    for replace_turn in [false, true] {
        let (root, mut parent, child) = seed_queue();
        let server = QueueServer::open(&root);
        let (client, runtime) = begin_queue_work(&server, &root, child.id);
        let id = Uuid::new_v4();
        server.submit(&client, parent.id, child.id, id, "old feedback");
        if replace_turn {
            // Delay only the worker to exercise a user turn that wins the race.
            server.backend.paused.store(true, Ordering::Release);
            server.finish(child.id);
            // A newer explicit prompt is authoritative; queued feedback cannot join it.
            client
                .request(
                    child.id,
                    runtime,
                    Command::Prompt {
                        prompt: "replacement".into(),
                        turn_id: None,
                        message_id: None,
                    },
                )
                .unwrap();
        } else {
            parent.runtime_mode = RuntimeMode::Ask;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![parent],
                        live_session_ids: vec![child.id],
                    },
                )
                .unwrap();
        }
        server.backend.paused.store(false, Ordering::Release);
        server.finish(child.id);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while queue_state(&client, child.id, id) != InputDeliveryState::Failed {
            assert!(
                std::time::Instant::now() < deadline,
                "replace_turn={replace_turn}, state={:?}",
                server
                    .backend
                    .inner
                    .task_state
                    .lock()
                    .sessions
                    .iter()
                    .find(|s| s.id == child.id)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(server.calls.try_recv().is_err());
        drop(client);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn input_queue_socket_waits_for_user_and_stops_during_shutdown() {
    for shutdown in [false, true] {
        let (root, parent, child) = seed_queue();
        let server = QueueServer::open(&root);
        let (client, _) = begin_queue_work(&server, &root, child.id);
        let id = Uuid::new_v4();
        server.submit(&client, parent.id, child.id, id, "waiting feedback");
        let sink = server.backend.sinks.lock().get(&child.id).unwrap().clone();
        sink.send(
            event_to_wire(DriverEvent::UserInputRequested {
                request_id: "question".into(),
                questions: vec![crate::model::UserInputQuestion {
                    id: "q".into(),
                    header: "Decision".into(),
                    question: "Continue?".into(),
                    options: vec![],
                    multi_select: false,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            queue_state(&client, child.id, id),
            InputDeliveryState::Queued
        );
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        if shutdown {
            client
                .request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown)
                .unwrap();
            sink.input_state_changed();
            assert!(
                server
                    .calls
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
        } else {
            sink.send(
                event_to_wire(DriverEvent::InteractionResponded {
                    request_id: "question".into(),
                })
                .unwrap(),
            )
            .unwrap();
            server.finish(child.id);
            assert_eq!(
                server.calls.recv_timeout(Duration::from_secs(5)).unwrap().1,
                "waiting feedback"
            );
        }
        drop(client);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn input_queue_socket_cancel_and_restart_with_unanswered_question_do_not_dispatch() {
    for cancel in [false, true] {
        let (root, parent, child) = seed_queue();
        let server = QueueServer::open(&root);
        let (client, runtime) = begin_queue_work(&server, &root, child.id);
        let id = Uuid::new_v4();
        server.submit(&client, parent.id, child.id, id, "held feedback");
        if cancel {
            client.request(child.id, runtime, Command::Cancel).unwrap();
            assert_eq!(
                queue_state(&client, child.id, id),
                InputDeliveryState::Failed
            );
        } else {
            server
                .backend
                .sinks
                .lock()
                .get(&child.id)
                .unwrap()
                .send(
                    event_to_wire(DriverEvent::UserInputRequested {
                        request_id: "unanswered".into(),
                        questions: vec![crate::model::UserInputQuestion {
                            id: "q".into(),
                            header: "Decision".into(),
                            question: "Continue?".into(),
                            options: vec![],
                            multi_select: false,
                        }],
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        drop(client);
        drop(server);
        let reopened = QueueServer::open(&root);
        let client = reopened.connect();
        reopened.start(&client, &root, child.id);
        assert_eq!(
            queue_state(&client, child.id, id),
            InputDeliveryState::Failed
        );
        assert!(
            reopened
                .calls
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(client);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn input_queue_socket_missing_runtime_is_unknown_capability_and_never_queues() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let (client, _) = begin_queue_work(&server, &root, child.id);
    server.backend.inner.sessions.lock().remove(&child.id);
    let id = Uuid::new_v4();
    let reply = server.submit(&client, parent.id, child.id, id, "early feedback");
    assert_eq!(reply["delivery"]["state"], "failed");
    assert!(
        reply["delivery"]["reason"]
            .as_str()
            .unwrap()
            .contains("unknown")
    );
    assert!(server.calls.try_recv().is_err());
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_queue_socket_guard_release_recovers_a_consumed_completion_wake() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let (client, _) = begin_queue_work(&server, &root, child.id);
    server.submit(
        &client,
        parent.id,
        child.id,
        Uuid::new_v4(),
        "after operation",
    );
    let sink = server.backend.sinks.lock().get(&child.id).unwrap().clone();
    let operation = sink.reserve_input_target(child.id, true).unwrap();
    server.finish(child.id);
    assert!(
        server
            .calls
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    drop(operation);
    assert_eq!(
        server.calls.recv_timeout(Duration::from_secs(5)).unwrap().1,
        "after operation"
    );
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_queue_user_source_waits_for_an_existing_callback_operation() {
    let (root, _, child) = seed_queue();
    let server = QueueServer::open(&root);
    let (client, _) = begin_queue_work(&server, &root, child.id);
    let sink = server.backend.sinks.lock().get(&child.id).unwrap().clone();
    let callback = sink.reserve_steward_target(child.id).unwrap();
    let inner = server.backend.inner.clone();
    let (done, result) = crossbeam_channel::bounded(1);
    let task = std::thread::spawn(move || {
        done.send(inner.deliver_authorized_input(
            child.id,
            child.id,
            "user feedback".into(),
            None,
            Some(Uuid::new_v4()),
            &sink,
        ))
        .unwrap();
    });
    assert!(result.recv_timeout(Duration::from_millis(100)).is_err());
    drop(callback);
    assert!(result.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
    task.join().unwrap();
    server.finish(child.id);
    assert_eq!(
        server.calls.recv_timeout(Duration::from_secs(5)).unwrap().1,
        "user feedback"
    );
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_queue_socket_restart_keeps_delivery_id_bound_to_the_original_target() {
    let (root, parent, child) = seed_queue();
    let mut other = child.clone();
    other.id = Uuid::new_v4();
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap();
    state.sessions.push(other.clone());
    state.mark_session_dirty(other.id);
    store.save(&mut state).unwrap();
    let server = QueueServer::open(&root);
    let (client, _) = begin_queue_work(&server, &root, child.id);
    let id = Uuid::new_v4();
    server.submit(&client, parent.id, child.id, id, "bound feedback");
    drop(client);
    drop(server);
    let reopened = QueueServer::open(&root);
    let client = reopened.connect();
    let error = client
        .request(
            parent.id,
            Uuid::nil(),
            Command::StewardPrompt {
                child_session_id: other.id,
                prompt: "bound feedback".into(),
                delivery_id: Some(id),
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("already bound"));
    assert!(reopened.calls.try_recv().is_err());
    drop(client);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn consultation_unconfirmed_direction_blocks_only_its_old_wait() {
    for reject in [true, false] {
        let (root, parent, child) = seed_queue();
        let server = QueueServer::open(&root);
        server.backend.steer.store(true, Ordering::Release);
        let (client, parent_runtime) = begin_queue_work(&server, &root, parent.id);
        let (_, child_runtime) = begin_queue_work(&server, &root, child.id);
        client
            .request(
                parent.id,
                parent_runtime,
                Command::StewardWait {
                    session_ids: vec![child.id],
                },
            )
            .unwrap();
        let id = Uuid::new_v4();
        let reply = client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::ExecuteConsultation {
                    source_session_id: parent.id,
                    delivery_id: id,
                    instruction: "lose confirmation".into(),
                },
            )
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(
            reply["consultation"]["instructions"][0]["delivery"]["state"],
            "uncertain"
        );
        assert!(
            server
                .calls
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .1
                .contains(&child.id.to_string())
        );
        server.finish(child.id);
        server.finish(parent.id);
        // Allow the event-driven worker to process the two controlled completions.
        std::thread::sleep(Duration::from_millis(150));
        let snapshot = |client: &DaemonClient| {
            let ResponsePayload::Session {
                session: Some(session),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession {
                        session_id: parent.id,
                    },
                )
                .unwrap()
            else {
                panic!()
            };
            session
        };
        let waiting = snapshot(&client);
        assert!(
            waiting.steward_wait.is_some(),
            "an unconfirmed direction must not resume the old plan"
        );
        assert_eq!(waiting.turns.len(), 2);
        if !reject {
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::ExecuteConsultation {
                        source_session_id: parent.id,
                        delivery_id: Uuid::new_v4(),
                        instruction: "Proceed with a fresh confirmed direction".into(),
                    },
                )
                .unwrap();
            assert!(
                server
                    .calls
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .1
                    .contains(&child.id.to_string())
            );
            client
                .request(
                    child.id,
                    child_runtime,
                    Command::Prompt {
                        prompt: "Follow the revised plan".into(),
                        turn_id: None,
                        message_id: None,
                    },
                )
                .unwrap();
            client
                .request(
                    parent.id,
                    parent_runtime,
                    Command::StewardWait {
                        session_ids: vec![child.id],
                    },
                )
                .unwrap();
            server.finish(parent.id);
            server.finish(child.id);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let current = snapshot(&client);
                if current.turns.len() == 4 {
                    assert_eq!(
                        current
                            .input_deliveries
                            .iter()
                            .find(|d| d.id == id)
                            .unwrap()
                            .state,
                        InputDeliveryState::Uncertain
                    );
                    assert!(current.steward_wait.is_none());
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "a historical uncertain delivery blocked the revised plan"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            client
                .request(
                    child.id,
                    child_runtime,
                    Command::Prompt {
                        prompt: "Another revised child task".into(),
                        turn_id: None,
                        message_id: None,
                    },
                )
                .unwrap();
            client
                .request(
                    parent.id,
                    parent_runtime,
                    Command::StewardWait {
                        session_ids: vec![child.id],
                    },
                )
                .unwrap();
            let latest_wait = snapshot(&client).steward_wait.unwrap();
            server
                .backend
                .sinks
                .lock()
                .get(&parent.id)
                .unwrap()
                .send(
                    event_to_wire(DriverEvent::InputDeliveryOutcome(InputDeliveryOutcome {
                        id,
                        state: InputDeliveryState::Received,
                        confirmation: Some(InputConfirmation::Provider),
                        reason: None,
                    }))
                    .unwrap(),
                )
                .unwrap();
            assert_eq!(
                snapshot(&client).steward_wait.unwrap().id,
                latest_wait.id,
                "old receipt erased a newer wait"
            );
            server.finish(parent.id);
            server.finish(child.id);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while snapshot(&client).turns.len() != 5 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(client);
            drop(server);
            std::fs::remove_dir_all(root).unwrap();
            continue;
        }
        server
            .backend
            .sinks
            .lock()
            .get(&parent.id)
            .unwrap()
            .send(
                event_to_wire(DriverEvent::InputDeliveryOutcome(InputDeliveryOutcome {
                    id,
                    state: InputDeliveryState::Failed,
                    confirmation: None,
                    reason: Some("native turn rejected the input".into()),
                }))
                .unwrap(),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let current = snapshot(&client);
            if current.turns.len() == 3 {
                assert!(current.steward_wait.is_none());
                assert!(
                    current
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .contains("automatic child-session notification")
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn consultation_direction_coordinates_both_callback_orders_and_preserves_other_tasks() {
    for callback_first in [false, true] {
        for steer in [false, true] {
            let (root, parent, child) = seed_queue();
            let mut other = child.clone();
            other.id = Uuid::new_v4();
            other.title = "unrelated task".into();
            let store = StateStore::daemon(root.join("state.db"));
            let mut seed = store.load().unwrap();
            seed.sessions.push(other.clone());
            seed.mark_session_dirty(other.id);
            store.save(&mut seed).unwrap();
            let server = QueueServer::open(&root);
            server.backend.steer.store(steer, Ordering::Release);
            let (client, parent_runtime) = begin_queue_work(&server, &root, parent.id);
            let (_, child_runtime) = begin_queue_work(&server, &root, child.id);
            let (_, _) = begin_queue_work(&server, &root, other.id);
            client
                .request(
                    parent.id,
                    parent_runtime,
                    Command::StewardWait {
                        session_ids: vec![child.id, other.id],
                    },
                )
                .unwrap();
            let snapshot = |id| {
                let ResponsePayload::Session {
                    session: Some(session),
                } = client
                    .request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::HydrateSession { session_id: id },
                    )
                    .unwrap()
                else {
                    panic!()
                };
                session
            };
            if callback_first {
                server.finish(child.id);
                server.finish(parent.id);
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while snapshot(parent.id).turns.len() != 3 {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let id = Uuid::new_v4();
            let reply = serde_json::to_value(
                client
                    .request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::ExecuteConsultation {
                            source_session_id: parent.id,
                            delivery_id: id,
                            instruction: "Change only the parser task; preserve unrelated work."
                                .into(),
                        },
                    )
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                reply["consultation"]["instructions"][0]["delivery"]["state"],
                if steer { "received" } else { "queued" }
            );
            if !callback_first {
                server.finish(child.id);
            }
            server.finish(parent.id);
            let (_, prompt) = server.calls.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(prompt.contains("Change only the parser task"));
            if !callback_first {
                assert!(
                    prompt.contains(&child.id.to_string())
                        && prompt.contains(&other.id.to_string())
                );
            }
            assert!(snapshot(other.id).active_turn_id().is_some());
            assert!(snapshot(other.id).cancellation_requested_turn_id.is_none());
            assert!(snapshot(parent.id).steward_wait.is_none());
            let expected_turns = 2 + usize::from(callback_first) + usize::from(!steer);
            assert_eq!(snapshot(parent.id).turns.len(), expected_turns);
            // A later independent result cannot revive the previous wait.
            server.finish(other.id);
            assert!(
                server
                    .calls
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
            assert_eq!(snapshot(parent.id).turns.len(), expected_turns);
            assert!(snapshot(child.id).active_turn_id().is_none());
            let _ = child_runtime;
            drop(client);
            drop(server);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[test]
fn consultation_execution_keeps_rejected_text_while_cancellation_is_unconfirmed() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let (client, runtime) = begin_queue_work(&server, &root, parent.id);
    let (_, _) = begin_queue_work(&server, &root, child.id);
    client.request(parent.id, runtime, Command::Cancel).unwrap();
    let id = Uuid::new_v4();
    let reply = serde_json::to_value(
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::ExecuteConsultation {
                    source_session_id: parent.id,
                    delivery_id: id,
                    instruction: "Switch direction after the current work has stopped".into(),
                },
            )
            .unwrap(),
    )
    .unwrap();
    let record = &reply["consultation"]["instructions"][0];
    assert!(
        record["error"]
            .as_str()
            .unwrap()
            .contains("cancellation has not settled")
    );
    assert!(record["delivery"].is_null());
    assert_eq!(
        record["instruction"],
        "Switch direction after the current work has stopped"
    );
    assert!(server.calls.try_recv().is_err());
    let ResponsePayload::Session {
        session: Some(parent),
    } = client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::HydrateSession {
                session_id: parent.id,
            },
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(parent.active_turn_id().is_some());
    assert!(parent.cancellation_requested_turn_id.is_some());
    let loaded = client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::LoadConsultation {
                source_session_id: parent.id,
            },
        )
        .unwrap();
    assert_eq!(serde_json::to_value(loaded).unwrap(), reply);
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn consultation_execution_waits_for_an_inflight_callback_before_steering_it() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.steer.store(true, Ordering::Release);
    let (release, gate) = crossbeam_channel::bounded(1);
    *server.backend.callback_gate.lock() = Some(gate);
    let (client, runtime) = begin_queue_work(&server, &root, parent.id);
    let (_, _) = begin_queue_work(&server, &root, child.id);
    client
        .request(
            parent.id,
            runtime,
            Command::StewardWait {
                session_ids: vec![child.id],
            },
        )
        .unwrap();
    server.finish(child.id);
    server.finish(parent.id);
    assert!(
        server
            .calls
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .1
            .contains("automatic child-session notification")
    );
    let concurrent = server.connect();
    let (done, reply) = crossbeam_channel::bounded(1);
    let id = Uuid::new_v4();
    let writer = std::thread::spawn(move || {
        done.send(concurrent.request(
            Uuid::nil(),
            Uuid::nil(),
            Command::ExecuteConsultation {
                source_session_id: parent.id,
                delivery_id: id,
                instruction: "Use the corrected plan".into(),
            },
        ))
        .unwrap();
    });
    assert!(reply.recv_timeout(Duration::from_millis(100)).is_err());
    release.send(()).unwrap();
    let response =
        serde_json::to_value(reply.recv_timeout(Duration::from_secs(5)).unwrap().unwrap()).unwrap();
    assert_eq!(
        response["consultation"]["instructions"][0]["delivery"]["mode"],
        "steer"
    );
    assert_eq!(
        response["consultation"]["instructions"][0]["delivery"]["state"],
        "received"
    );
    assert!(
        server
            .calls
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .1
            .contains("Use the corrected plan")
    );
    writer.join().unwrap();
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

// Advertise an explicitly unsupported capability at the provider boundary.
// Tracked input, cancellation and process lifetime still use the real fixture process.
struct UnsteerableParent(DriverHandle);
impl crate::driver::DriverControl for UnsteerableParent {
    fn prompt(&self, prompt: String) {
        self.0.prompt(prompt);
    }
    fn supports_steer(&self) -> bool {
        false
    }
    fn deliver_input(&self, prompt: String, id: Uuid, steer: bool) -> anyhow::Result<()> {
        assert!(!steer, "unsupported parent must receive a queued new turn");
        self.0.deliver_input(prompt, id, steer)
    }
    fn cancel(&self) {
        self.0.cancel();
    }
    fn respond(&self, id: String, option: String) {
        self.0.respond(id, option);
    }
    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        self.0.rollback(turns)
    }
}
struct CoordinationBackend(Arc<WakuBackend>);
impl Backend for CoordinationBackend {
    fn prepare_start(
        &self,
        id: Uuid,
        options: &crate::WireDriverStartOptions,
    ) -> anyhow::Result<()> {
        self.0.prepare_start(id, options)
    }
    fn stop_failed_work(&self) {
        self.0.stop_failed_work();
    }
    fn authorize_steward(&self, id: Uuid, project: Uuid) -> anyhow::Result<()> {
        self.0.authorize_steward(id, project)
    }
    fn authorize_cached_creation(&self, child: &AgentSession) -> anyhow::Result<()> {
        self.0.authorize_cached_creation(child)
    }
    fn shutdown(&self) {
        self.0.shutdown();
    }
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        let parent =
            matches!(&request.command, Command::Start {options} if options.provider == "claude")
                .then_some(request.session_id);
        let response = self.0.handle(request, events)?;
        if let Some(parent) = parent {
            let mut sessions = self.0.sessions.lock();
            let (_, driver) = sessions.get_mut(&parent).expect("started parent fixture");
            *driver = DriverHandle::from_control(Arc::new(UnsteerableParent(driver.clone())));
        }
        Ok(response)
    }
    fn persist_events(&self, events: &[crate::SequencedEvent]) -> anyhow::Result<bool> {
        self.0.persist_events(events)
    }
    fn resume_stewards(&self, events: EventSink) {
        self.0.resume_stewards(events);
    }
}

impl WakuBackend {
    pub(crate) fn fixture_with_unsteerable_parent(self: Arc<Self>) -> Arc<dyn Backend> {
        Arc::new(CoordinationBackend(self))
    }
}

#[test]
fn consultation_instruction_presentation_survives_delivery_and_restart() {
    for mode in ["idle", "steer", "queued"] {
        let (root, parent, _) = seed_queue();
        let server = QueueServer::open(&root);
        server.backend.steer.store(mode == "steer", Ordering::Release);
        let client = if mode == "idle" {
            let client = server.connect();
            server.start(&client, &root, parent.id);
            client
        } else {
            begin_queue_work(&server, &root, parent.id).0
        };
        let id = Uuid::new_v4();
        let instruction = "只修改 dependent.txt，保留当前成果。";
        client.request(Uuid::nil(), Uuid::nil(), Command::ExecuteConsultation {
            source_session_id: parent.id, delivery_id: id, instruction: instruction.into(),
        }).unwrap();
        if mode == "queued" {
            assert!(server.calls.try_recv().is_err());
            server.finish(parent.id);
        }
        let (target, prompt) = server.calls.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(target, parent.id);
        assert!(prompt.contains("recent_discussion"));
        assert!(prompt.contains(instruction));
        let verify = |client: &DaemonClient| {
            let ResponsePayload::Session { session: Some(session) } = client.request(
                Uuid::nil(), Uuid::nil(), Command::HydrateSession { session_id: parent.id },
            ).unwrap() else { panic!("source is missing"); };
            let delivery = session.input_deliveries.iter().find(|delivery| delivery.id == id).unwrap();
            assert_eq!(delivery.prompt, prompt, "audit keeps the complete provider input");
            assert_eq!(delivery.state, InputDeliveryState::Received);
            let message = session.messages.iter().find(|message| message.content == prompt).unwrap();
            assert_eq!(message.visible_content(), instruction, "{mode}: show the user's original instruction");
            assert_eq!(message.display_content.as_deref(), Some(instruction));
        };
        verify(&client);
        server.finish(parent.id);
        drop(client);
        drop(server);
        let reopened = QueueServer::open(&root);
        let client = reopened.connect();
        verify(&client);
        drop(client);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn decision_socket_persists_request_rejects_conflicts_and_resumes_once() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    server.start(&client, &root, parent.id);
    let runtime = server.start(&client, &root, child.id);
    client.request(child.id, runtime, Command::Prompt { prompt: "Work until a decision is needed".into(), turn_id: None, message_id: None }).unwrap();
    let id = Uuid::new_v4();
    let operation = json!({"type":"request","request_id":id,"question":"Which format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"});
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let submit = || serde_json::to_value(client.request(child.id, runtime, command(operation.clone())).unwrap()).unwrap();
    let requested = submit();
    assert_eq!(requested["requests"][0]["state"], "waitingManager");
    assert_eq!(submit()["requests"][0]["id"], id.to_string());
    let mut conflict = operation.clone();
    conflict["question"] = json!("Different question");
    assert!(client.request(child.id, runtime, command(conflict)).is_err());
    server.finish(child.id);
    let decision = json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON","authority_message_id":parent.messages[0].id});
    client.request(parent.id, Uuid::nil(), command(decision.clone())).unwrap();
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains("Use JSON"));
    client.request(parent.id, Uuid::nil(), command(decision)).unwrap();
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err());
    let query = json!({"type":"list","session_id":child.id});
    let result = serde_json::to_value(client.request(parent.id, Uuid::nil(), command(query.clone())).unwrap()).unwrap();
    assert_eq!(result["requests"][0]["state"], "resolved");
    drop(client);
    drop(server);
    let reopened = QueueServer::open(&root);
    let result = serde_json::to_value(reopened.connect().request(parent.id, Uuid::nil(), command(query)).unwrap()).unwrap();
    assert_eq!(result["requests"][0]["decision"], "Use JSON");
    assert_eq!(result["requests"][0]["state"], "resolved");
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_busy_manager_preserves_result_wait_and_rejects_missing_authority() { decision_wait_scenario(false); }

#[test]
fn decision_socket_resumed_child_native_wait_wakes_manager() { decision_wait_scenario(true); }

fn decision_wait_scenario(native_wait: bool) {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    let client = server.connect();
    let parent_runtime = server.start(&client, &root, parent.id);
    let child_runtime = server.start(&client, &root, child.id);
    let subscribed = client.subscribe(parent.id, parent_runtime);
    let (release, gate) = crossbeam_channel::unbounded();
    release.send(()).unwrap();
    release.send(()).unwrap();
    *server.backend.callback_gate.lock() = Some(gate);
    for (id, runtime, prompt) in [(parent.id, parent_runtime, "Delegate JSON output"), (child.id, child_runtime, "Prepare output")] {
        client.request(id, runtime, Command::Prompt { prompt: prompt.into(), turn_id: None, message_id: None }).unwrap();
    }
    let wait = client.request(parent.id, parent_runtime, Command::StewardWait { session_ids: vec![child.id] }).unwrap();
    assert!(matches!(wait, ResponsePayload::StewardWait { wait: Some(_), .. }));
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let id = Uuid::new_v4();
    let request = client.request(child.id, child_runtime, command(json!({"type":"request","request_id":id,"question":"Which format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}))).unwrap();
    let request = serde_json::to_value(request).unwrap();
    let authority = request["requests"][0]["instruction_message_id"].clone();
    server.finish(child.id);
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err(), "busy parent must not receive another turn");
    server.finish(parent.id);
    let notice = server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1;
    assert!(notice.contains("automatic decision notification"));
    // Follow only the public socket stream, with no hydrated callback message or input receipt.
    let mut fresh = parent.clone();
    let mut reducer = waku_protocol::history::HistoryReducer::default();
    let mut saw_callback = false;
    loop {
        let event = subscribed.recv_timeout(Duration::from_secs(3)).unwrap().event;
        // The client handles persistence acknowledgements separately from transcript events.
        if event.kind == "historyPersistence" { continue; }
        let callback = event.kind == "promptSubmitted"
            && event.payload.get("message").and_then(Value::as_str)
                .is_some_and(|message| message.contains("automatic decision notification"));
        if callback {
            saw_callback = true;
            assert_eq!(event.payload["displayContent"], "子会话请求管家决定。");
        }
        reducer.apply(&mut fresh, waku_protocol::event_from_wire(event).unwrap());
        if saw_callback && fresh.steward_wait.as_ref().is_some_and(|wait| Some(wait.parent_turn_id) == fresh.active_turn_id()) { break; }
    }
    let message = fresh.messages.last().unwrap();
    assert!(message.content.contains("automatic decision notification"));
    assert_eq!(message.visible_content(), "子会话请求管家决定。");
    assert!(fresh.input_deliveries.is_empty(), "presentation requires no invented input receipt");
    assert_eq!(fresh.steward_wait.as_ref().unwrap().parent_turn_id, fresh.active_turn_id().unwrap());

    let waiting = client.request(parent.id, parent_runtime, command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON"}))).unwrap();
    assert_eq!(serde_json::to_value(waiting).unwrap()["requests"][0]["state"], "waitingUser");
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err());
    client.request(parent.id, parent_runtime, command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON","authority_message_id":authority}))).unwrap();
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains("Use JSON"));
    server.finish(parent.id);
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err(), "request turn completion is not the result");
    if native_wait {
        server.backend.sinks.lock().get(&child.id).unwrap().send(event_to_wire(DriverEvent::Permission {
            request_id: "approval".into(), title: "Approve work".into(), detail: "Need native permission".into(), options: Vec::new(),
        }).unwrap()).unwrap();
    } else { server.finish(child.id); }
    let notification = server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1;
    assert!(notification.contains("automatic child-session notification"));
    if native_wait { assert!(notification.contains("\"waiting_for_permission\":true")); }
    server.finish(parent.id);
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err());
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_cancellation_direction_and_shutdown_do_not_resume_old_work() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let pr = server.start(&client, &root, parent.id);
    let cr = server.start(&client, &root, child.id);
    client.request(child.id, cr, Command::Prompt { prompt: "Need a decision".into(), turn_id: None, message_id: None }).unwrap();
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let request = |id| command(json!({"type":"request","request_id":id,"question":"Format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}));
    let id = Uuid::new_v4();
    client.request(child.id, cr, request(id)).unwrap();
    assert!(client.request(parent.id, pr, command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON","authority_message_id":child.messages[0].id}))).is_err());
    server.finish(child.id);
    client.request(parent.id, pr, Command::Prompt { prompt: "Change direction: do not produce output".into(), turn_id: None, message_id: None }).unwrap();
    let list = || serde_json::to_value(client.request(parent.id, pr, command(json!({"type":"list","session_id":child.id}))).unwrap()).unwrap();
    assert_eq!(list()["requests"][0]["state"], "invalidated");
    assert!(client.request(parent.id, pr, command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON","authority_message_id":parent.messages[0].id}))).is_err());
    client.request(child.id, cr, Command::Prompt { prompt: "Another decision".into(), turn_id: None, message_id: None }).unwrap();
    let cancelled = Uuid::new_v4();
    client.request(child.id, cr, request(cancelled)).unwrap();
    server.finish(child.id);
    client.request(parent.id, pr, Command::StewardCancel { child_session_id: child.id }).unwrap();
    assert_eq!(list()["requests"][1]["state"], "invalidated");
    client.request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown).unwrap();
    assert!(client.request(child.id, cr, request(Uuid::new_v4())).is_err());
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err());
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_uncertain_receipt_survives_restart_without_resubmission() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let runtime = server.start(&client, &root, child.id);
    client.request(child.id, runtime, Command::Prompt { prompt: "Need a decision".into(), turn_id: None, message_id: None }).unwrap();
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let id = Uuid::new_v4();
    client.request(child.id, runtime, command(json!({"type":"request","request_id":id,"question":"Format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}))).unwrap();
    server.finish(child.id);
    let decision = json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"lose confirmation","authority_message_id":parent.messages[0].id});
    let pending = client.request(parent.id, Uuid::nil(), command(decision.clone())).unwrap();
    assert_eq!(serde_json::to_value(pending).unwrap()["requests"][0]["state"], "pendingReceipt");
    server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(client);
    drop(server);
    let reopened = QueueServer::open(&root);
    let client = reopened.connect();
    reopened.start(&client, &root, child.id);
    let retry = client.request(parent.id, Uuid::nil(), command(decision)).unwrap();
    assert_eq!(serde_json::to_value(retry).unwrap()["requests"][0]["state"], "pendingReceipt");
    assert!(reopened.calls.recv_timeout(Duration::from_millis(200)).is_err());
    drop(client);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_multiple_requests_resume_in_order_without_losing_context() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let runtime = server.start(&client, &root, child.id);
    client.request(child.id, runtime, Command::Prompt { prompt: "Need two decisions".into(), turn_id: None, message_id: None }).unwrap();
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    for id in ids {
        client.request(child.id, runtime, command(json!({"type":"request","request_id":id,"question":"Format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}))).unwrap();
    }
    server.finish(child.id);
    for (id, decision) in ids.into_iter().zip(["First decision", "Second decision"]) {
        client.request(parent.id, Uuid::nil(), command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":decision,"authority_message_id":parent.messages[0].id}))).unwrap();
    }
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains("First decision"));
    assert!(server.calls.recv_timeout(Duration::from_millis(100)).is_err());
    server.backend.paused.store(false, Ordering::Release);
    server.finish(child.id);
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains("Second decision"));
    let listed = client.request(parent.id, Uuid::nil(), command(json!({"type":"list","session_id":child.id}))).unwrap();
    let listed = serde_json::to_value(listed).unwrap();
    assert_eq!(listed["requests"][0]["state"], "resolved");
    assert_eq!(listed["requests"][1]["state"], "resolved");
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_rejects_ordinary_input_receipt_collision_and_forged_delivery() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let runtime = server.start(&client, &root, child.id);
    let existing = Uuid::new_v4();
    server.submit(&client, parent.id, child.id, existing, "Ordinary input");
    server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let request = |id| command(json!({"type":"request","request_id":id,"question":"Format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}));
    assert!(client.request(child.id, runtime, request(existing)).is_err());
    let id = Uuid::new_v4();
    client.request(child.id, runtime, request(id)).unwrap();
    server.finish(child.id);
    assert!(client.request(parent.id, Uuid::nil(), Command::StewardPrompt { child_session_id: child.id, prompt: "Forged ordinary decision".into(), delivery_id: Some(id) }).is_err());
    let listed = client.request(parent.id, Uuid::nil(), command(json!({"type":"list","session_id":child.id}))).unwrap();
    assert_eq!(serde_json::to_value(listed).unwrap()["requests"][0]["state"], "waitingManager");
    assert!(server.calls.recv_timeout(Duration::from_millis(100)).is_err());
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_socket_consultation_direction_is_user_authority_and_invalidates_old_requests() {
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    server.start(&client, &root, parent.id);
    let runtime = server.start(&client, &root, child.id);
    client.request(child.id, runtime, Command::Prompt { prompt: "Need a decision".into(), turn_id: None, message_id: None }).unwrap();
    let command = |operation| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":operation})).unwrap();
    let request = |id| command(json!({"type":"request","request_id":id,"question":"Format?","context":"Output file","recommendation":"JSON","blocked_work":"Write output"}));
    let old = Uuid::new_v4();
    client.request(child.id, runtime, request(old)).unwrap();
    client.request(Uuid::nil(), Uuid::nil(), Command::ExecuteConsultation { source_session_id: parent.id, delivery_id: Uuid::new_v4(), instruction: "New direction: write a JSON file".into() }).unwrap();
    server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    let listed = client.request(parent.id, Uuid::nil(), command(json!({"type":"list","session_id":child.id}))).unwrap();
    assert_eq!(serde_json::to_value(listed).unwrap()["requests"][0]["state"], "invalidated");
    let id = Uuid::new_v4();
    let created = client.request(child.id, runtime, request(id)).unwrap();
    let created = serde_json::to_value(created).unwrap();
    assert_eq!(created["requests"][0]["instruction"], "New direction: write a JSON file");
    let authority = created["requests"][0]["instruction_message_id"].clone();
    assert_ne!(authority, json!(parent.messages[0].id));
    server.finish(child.id);
    let decided = client.request(parent.id, Uuid::nil(), command(json!({"type":"decide","session_id":child.id,"request_id":id,"decision":"Use JSON","authority_message_id":authority}))).unwrap();
    let decided = serde_json::to_value(decided).unwrap();
    assert_eq!(decided["requests"][0]["state"], "invalidated");
    assert_eq!(decided["requests"][1]["state"], "resolved");
    // List retains both records, while only the new request can resume work.
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains("Use JSON"));
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_user_answer_socket_is_saved_and_resumes_only_its_request() { decision_user_answer_scenario(false); }

#[test]
fn decision_user_answer_socket_uncertain_receipt_is_not_replayed_on_restart() { decision_user_answer_scenario(true); }

fn decision_user_answer_scenario(uncertain: bool) {
    let answer_text = if uncertain { "lose confirmation" } else { "Add the report" };
    let expected_state = if uncertain { "pendingReceipt" } else { "resolved" };
    let (root, parent, child) = seed_queue();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let runtime = server.start(&client, &root, child.id);
    client.request(child.id, runtime, Command::Prompt { prompt: "Need user decisions".into(), turn_id: None, message_id: None }).unwrap();
    let command = |value| serde_json::from_value::<Command>(value).unwrap();
    let operation = |value| command(json!({"type":"stewardDecision","operation":value}));
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    for id in ids {
        client.request(child.id, runtime, operation(json!({"type":"request","request_id":id,"question":"Expand scope?","context":"Original task excludes a report","recommendation":"Add report","blocked_work":"Report generation"}))).unwrap();
        client.request(parent.id, Uuid::nil(), operation(json!({"type":"escalate","session_id":child.id,"request_id":id,"reason":"Additional scope requires approval","options":[{"label":"Add report","impact":"Additional file"},{"label":"Keep scope","impact":"No report"}],"impact":"Changes output scope"}))).unwrap();
    }
    server.finish(child.id);
    let answer = json!({"type":"answerDecision","childSessionId":child.id,"requestId":ids[0],"answer":answer_text});
    let answered = client.request(parent.id, Uuid::nil(), command(answer.clone())).unwrap();
    let answered = serde_json::to_value(answered).unwrap();
    assert_eq!(answered["requests"][0]["user_answer"], answer_text);
    assert_eq!(answered["requests"][0]["state"], expected_state);
    assert_eq!(answered["requests"][1]["state"], "waitingUser");
    assert!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().1.contains(answer_text));
    client.request(parent.id, Uuid::nil(), command(answer)).unwrap();
    assert!(server.calls.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(client.request(parent.id, Uuid::nil(), command(json!({"type":"answerDecision","childSessionId":child.id,"requestId":ids[0],"answer":"Different answer"}))).is_err());
    client.request(Uuid::nil(), Uuid::nil(), Command::SaveTaskState { projects: Vec::new(), live_session_ids: Vec::new(), sessions: vec![parent.clone()] }).unwrap();
    let history = client.request(Uuid::nil(), Uuid::nil(), Command::HydrateSession { session_id: parent.id }).unwrap();
    let history = serde_json::to_value(history).unwrap();
    assert!(history["session"]["messages"].as_array().unwrap().iter().any(|m| m["role"] == "user" && m["content"] == answer_text));
    drop(client);
    drop(server);
    let reopened = QueueServer::open(&root);
    let listed = reopened.connect().request(parent.id, Uuid::nil(), operation(json!({"type":"list","session_id":child.id}))).unwrap();
    let listed = serde_json::to_value(listed).unwrap();
    assert_eq!(listed["requests"][0]["user_answer"], answer_text);
    assert_eq!(listed["requests"][1]["state"], "waitingUser");
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_user_answer_socket_follows_two_managers_and_preserves_final_results() { decision_escalation_scenario(false); }

#[test]
fn decision_user_answer_socket_root_cancellation_invalidates_the_entire_chain() { decision_escalation_scenario(true); }

fn decision_escalation_scenario(cancel: bool) {
    let (root, parent, manager) = seed_queue();
    let mut leaf = AgentSession::new(parent.project_id, ProviderKind::Codex);
    leaf.parent_session_id = Some(manager.id);
    leaf.runtime_mode = RuntimeMode::FullAccess;
    leaf.begin_turn("Leaf task");
    leaf.finish_active_turn(crate::model::TurnStatus::Completed);
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap();
    state.sessions.push(leaf.clone()); state.mark_session_dirty(leaf.id); store.save(&mut state).unwrap();
    let server = QueueServer::open(&root);
    let client = server.connect();
    let (release, gate) = crossbeam_channel::unbounded();
    for _ in 0..3 { release.send(()).unwrap(); }
    *server.backend.callback_gate.lock() = Some(gate);
    let pr = server.start(&client, &root, parent.id);
    let mr = server.start(&client, &root, manager.id);
    let lr = server.start(&client, &root, leaf.id);
    for (id, runtime, prompt) in [(parent.id,pr,"Delegate task"),(manager.id,mr,"Delegate leaf work"),(leaf.id,lr,"Prepare work")] {
        client.request(id, runtime, Command::Prompt { prompt: prompt.into(), turn_id: None, message_id: None }).unwrap();
    }
    let operation = |value| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":value})).unwrap();
    let id = Uuid::new_v4();
    client.request(leaf.id, lr, operation(json!({"type":"request","request_id":id,"question":"Expand scope?","context":"Report excluded","recommendation":"Add report","blocked_work":"Write report"}))).unwrap();
    let escalation = |child,id| operation(json!({"type":"escalate","session_id":child,"request_id":id,"reason":"New scope","options":[{"label":"Add","impact":"Extra file"},{"label":"Skip","impact":"Keep scope"}],"impact":"Additional output"}));
    let forwarded = client.request(manager.id, mr, escalation(leaf.id,id)).unwrap();
    let upstream: Uuid = serde_json::from_value(serde_json::to_value(forwarded).unwrap()["requests"][0]["upstream_request_id"].clone()).unwrap();
    assert!(client.request(parent.id,pr,escalation(leaf.id,id)).is_err(), "manager cannot skip a layer");
    client.request(parent.id,pr,escalation(manager.id,upstream)).unwrap();
    server.finish(leaf.id); server.finish(manager.id);
    if cancel {
        client.request(parent.id,pr,Command::Cancel).unwrap();
        assert!(client.request(parent.id, pr, Command::AnswerDecision { child_session_id: manager.id, request_id: upstream, answer: "Add the report".into() }).is_err());
        let listed = client.request(manager.id, mr, operation(json!({"type":"list","session_id":leaf.id}))).unwrap();
        assert_eq!(serde_json::to_value(listed).unwrap()["requests"][0]["state"], "invalidated");
        assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err());
        // Historical invalidation must not erase a fresh wait on the same child.
        client.request(manager.id, mr, Command::Prompt { prompt: "New assignment".into(), turn_id: None, message_id: None }).unwrap();
        client.request(leaf.id, lr, Command::Prompt { prompt: "New work".into(), turn_id: None, message_id: None }).unwrap();
        client.request(manager.id, mr, Command::StewardWait { session_ids: vec![leaf.id] }).unwrap();
        client.request(manager.id, mr, operation(json!({"type":"list","session_id":leaf.id}))).unwrap();
        let ResponsePayload::Session { session: Some(session) } = client.request(manager.id, mr, Command::HydrateSession { session_id: manager.id }).unwrap() else { panic!("history") };
        assert!(session.steward_wait.is_some(), "old invalidation must preserve the new result wait");
        drop(client); drop(server); std::fs::remove_dir_all(root).unwrap();
        return;
    }
    let answered = client.request(parent.id, pr, Command::AnswerDecision { child_session_id: manager.id, request_id: upstream, answer: "Add the report".into() }).unwrap();
    assert_eq!(serde_json::to_value(answered).unwrap()["requests"][0]["state"], "resolved");
    let delivered = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(delivered.0, leaf.id); assert!(delivered.1.contains("Add the report"));
    server.finish(parent.id);
    assert!(server.calls.recv_timeout(Duration::from_millis(150)).is_err(), "intermediate question completion must not consume the root's result wait");
    server.finish(leaf.id);
    let manager_notice = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(manager_notice.0, manager.id);
    server.finish(manager.id);
    let root_notice = server.calls.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(root_notice.0, parent.id);
    assert!(root_notice.1.contains("automatic child-session notification"));
    drop(client); drop(server); std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decision_nested_manager_reuses_only_confirmed_user_authority() {
    let (root, parent, manager) = seed_queue();
    let mut leaf = AgentSession::new(parent.project_id, ProviderKind::Codex);
    leaf.parent_session_id = Some(manager.id); leaf.runtime_mode = RuntimeMode::FullAccess;
    leaf.begin_turn("Leaf task"); leaf.finish_active_turn(crate::model::TurnStatus::Completed);
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap(); state.sessions.push(leaf.clone()); state.mark_session_dirty(leaf.id); store.save(&mut state).unwrap();
    let server = QueueServer::open(&root); server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let mr = server.start(&client,&root,manager.id); let lr = server.start(&client,&root,leaf.id);
    for (id,runtime) in [(manager.id,mr),(leaf.id,lr)] { client.request(id,runtime,Command::Prompt {prompt:"Need clarification".into(),turn_id:None,message_id:None}).unwrap(); }
    let operation = |value| serde_json::from_value::<Command>(json!({"type":"stewardDecision","operation":value})).unwrap();
    let source = Uuid::new_v4(); let target = Uuid::new_v4();
    for (session,runtime,id) in [(manager.id,mr,source),(leaf.id,lr,target)] { client.request(session,runtime,operation(json!({"type":"request","request_id":id,"question":"Format?","context":"Original deliverable","recommendation":"JSON","blocked_work":"Write output"}))).unwrap(); }
    server.finish(leaf.id);
    let delegated = client.request(manager.id,mr,operation(json!({"type":"list","session_id":leaf.id}))).unwrap();
    let fake_authority = serde_json::to_value(delegated).unwrap()["requests"][0]["instruction_message_id"].clone();
    assert!(client.request(manager.id,mr,operation(json!({"type":"decide","session_id":leaf.id,"request_id":target,"decision":"Use JSON","authority_message_id":fake_authority}))).is_err());
    server.finish(manager.id);
    client.request(parent.id,Uuid::nil(),operation(json!({"type":"decide","session_id":manager.id,"request_id":source,"decision":"Use JSON within the delegated task","authority_message_id":parent.messages[0].id}))).unwrap();
    assert_eq!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().0,manager.id);
    let decided = client.request(manager.id,mr,operation(json!({"type":"decide","session_id":leaf.id,"request_id":target,"decision":"Use JSON","authority_message_id":parent.messages[0].id}))).unwrap();
    assert_eq!(serde_json::to_value(decided).unwrap()["requests"][0]["state"],"resolved");
    assert_eq!(server.calls.recv_timeout(Duration::from_secs(3)).unwrap().0,leaf.id);
    drop(client); drop(server); std::fs::remove_dir_all(root).unwrap();
}
