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
}

impl DriverControl for QueueDriver {
    fn prompt(&self, _: String) {
        self.events
            .send(event_to_wire(DriverEvent::TurnStarted).unwrap())
            .unwrap();
    }
    fn deliver_input(&self, prompt: String, id: Uuid, steer: bool) -> anyhow::Result<()> {
        assert!(
            !steer,
            "unsupported provider must never receive native steer"
        );
        self.events.send(event_to_wire(DriverEvent::TurnStarted)?)?;
        if prompt != "lose confirmation" {
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
