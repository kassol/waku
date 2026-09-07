use super::*;

struct WaitDriver(crossbeam_channel::Sender<String>);

impl crate::driver::DriverControl for WaitDriver {
    fn prompt(&self, prompt: String) {
        self.0.send(prompt).unwrap();
    }
    fn cancel(&self) {
        panic!("waiting parent has no active provider turn to cancel");
    }
    fn respond(&self, _: String, _: String) {}
    fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        unreachable!()
    }
}

struct WaitFixture {
    root: PathBuf,
    backend: Arc<WakuBackend>,
    sink: EventSink,
    child_sink: EventSink,
    parent: Uuid,
    child: Uuid,
    prompts: crossbeam_channel::Receiver<String>,
}

impl WaitFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("waku-steward-wait-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let backend = Self::open(&root);
        let project = Project::from_path(root.join("repo"));
        let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
        parent.begin_turn("Delegate and return when the child finishes");
        parent.status = SessionStatus::Working;
        let mut child = AgentSession::new(project.id, ProviderKind::Codex);
        child.parent_session_id = Some(parent.id);
        child.begin_turn("Child work");
        child.status = SessionStatus::Working;
        let parent_id = parent.id;
        let child_id = child.id;
        {
            let mut state = backend.task_state.lock();
            state.projects.push(project);
            state.sessions.extend([parent, child]);
            state.mark_session_dirty(parent_id);
            state.mark_session_dirty(child_id);
            backend.task_store.save(&mut state).unwrap();
        }
        let (sink, prompts) = Self::attach(&backend, parent_id);
        let erased: Arc<dyn Backend> = backend.clone();
        let child_sink = EventSink::for_test(&erased, child_id, Uuid::new_v4());
        Self {
            root,
            backend,
            sink,
            child_sink,
            parent: parent_id,
            child: child_id,
            prompts,
        }
    }

    fn open(root: &Path) -> Arc<WakuBackend> {
        Arc::new(
            WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("state.db")),
            )
            .unwrap(),
        )
    }

    fn attach(
        backend: &Arc<WakuBackend>,
        parent: Uuid,
    ) -> (EventSink, crossbeam_channel::Receiver<String>) {
        let runtime = Uuid::new_v4();
        let (sender, prompts) = crossbeam_channel::unbounded();
        backend.sessions.lock().insert(
            parent,
            (
                runtime,
                DriverHandle::from_control(Arc::new(WaitDriver(sender))),
            ),
        );
        let erased: Arc<dyn Backend> = backend.clone();
        (EventSink::for_test(&erased, parent, runtime), prompts)
    }

    fn register(&self) -> ResponsePayload {
        self.backend
            .register_steward_wait(self.parent, vec![self.child], &self.sink)
            .unwrap()
    }

    fn parent_state(&self) -> AgentSession {
        self.backend
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == self.parent)
            .unwrap()
            .clone()
    }

    fn finish_child(&self) {
        self.child_sink
            .send(
                event_to_wire(DriverEvent::TurnFinished {
                    success: true,
                    summary: None,
                })
                .unwrap(),
            )
            .unwrap();
    }

    fn finish_parent(&self, success: bool) {
        self.sink
            .send(
                event_to_wire(DriverEvent::TurnFinished {
                    success,
                    summary: None,
                })
                .unwrap(),
            )
            .unwrap();
    }

    fn resume(&self) {
        self.backend.resume_stewards(self.sink.clone());
    }
}

impl Drop for WaitFixture {
    fn drop(&mut self) {
        self.backend.sessions.lock().clear();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn steward_wait_child_completion_waits_for_parent_and_prompts_once() {
    let fixture = WaitFixture::new();
    let previous = fixture.parent_state().active_turn_id();
    assert!(matches!(
        fixture.register(),
        ResponsePayload::StewardWait { wait: Some(_), .. }
    ));
    assert_eq!(fixture.parent_state().active_turn_id(), previous);
    assert!(fixture.prompts.try_recv().is_err());
    fixture.finish_child();
    fixture.resume();
    assert!(fixture.prompts.try_recv().is_err());
    assert_eq!(fixture.parent_state().active_turn_id(), previous);
    fixture.finish_parent(true);
    fixture.resume();
    let prompt = fixture
        .prompts
        .try_recv()
        .expect("ready child must wake the completed parent");
    assert!(prompt.contains(&fixture.child.to_string()));
    assert!(fixture.parent_state().steward_wait.is_none());
    assert_eq!(fixture.parent_state().turns.len(), 2);
    fixture.resume();
    assert!(fixture.prompts.try_recv().is_err());
}

#[test]
fn steward_wait_ready_child_returns_without_registering() {
    let fixture = WaitFixture::new();
    fixture.finish_child();
    assert!(matches!(
        fixture.register(),
        ResponsePayload::StewardWait { wait: None, .. }
    ));
    assert!(fixture.parent_state().steward_wait.is_none());
    assert!(fixture.prompts.try_recv().is_err());
}

#[test]
fn steward_wait_rejects_foreign_session() {
    let fixture = WaitFixture::new();
    let foreign = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let foreign_id = foreign.id;
    fixture.backend.task_state.lock().sessions.push(foreign);
    assert!(
        fixture
            .backend
            .register_steward_wait(fixture.parent, vec![foreign_id], &fixture.sink,)
            .is_err()
    );
    assert!(fixture.parent_state().steward_wait.is_none());
}

#[test]
fn steward_wait_cancel_failure_and_new_input_revoke_callback() {
    for event in [
        DriverEvent::SteerAccepted {
            message: "Change the delegated task".into(),
        },
        DriverEvent::CancelRequested,
        DriverEvent::TurnFinished {
            success: false,
            summary: Some("failed".into()),
        },
        DriverEvent::PromptSubmitted {
            display_content: None,
            message: "A new user instruction".into(),
            turn_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
        },
    ] {
        let fixture = WaitFixture::new();
        fixture.register();
        fixture.sink.send(event_to_wire(event).unwrap()).unwrap();
        assert!(fixture.parent_state().steward_wait.is_none());
        fixture.finish_child();
        fixture.finish_parent(true);
        fixture.resume();
        assert!(fixture.prompts.try_recv().is_err());
    }
}

#[test]
fn steward_wait_restores_from_database_without_duplicate_submission() {
    let fixture = WaitFixture::new();
    fixture.register();
    fixture.finish_parent(true);
    fixture.finish_child();
    let reopened = WaitFixture::open(&fixture.root);
    let (sink, prompts) = WaitFixture::attach(&reopened, fixture.parent);
    reopened.resume_stewards(sink.clone());
    assert!(prompts.try_recv().is_ok());
    reopened.resume_stewards(sink);
    assert!(prompts.try_recv().is_err());
    reopened.sessions.lock().clear();
}

#[test]
fn steward_wait_shutdown_never_wakes_parent() {
    let fixture = WaitFixture::new();
    fixture.register();
    fixture.finish_parent(true);
    fixture.finish_child();
    fixture.backend.quitting.store(true, Ordering::Release);
    fixture.resume();
    assert!(fixture.prompts.try_recv().is_err());
    assert_eq!(fixture.parent_state().turns.len(), 1);
}

#[test]
fn steward_wait_cancel_command_clears_wait_without_cancelling_provider() {
    let fixture = WaitFixture::new();
    fixture.register();
    fixture.finish_parent(true);
    assert!(fixture.parent_state().is_waiting_for_children());
    let runtime_id = fixture.backend.sessions.lock()[&fixture.parent].0;
    let response = fixture
        .backend
        .handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id: fixture.parent,
                runtime_id,
                command: Command::Cancel,
            },
            fixture.sink.clone(),
        )
        .unwrap();
    assert!(matches!(response, ResponsePayload::Ack));
    assert!(fixture.parent_state().steward_wait.is_none());
    assert!(fixture.parent_state().active_turn_id().is_none());
    fixture.finish_child();
    fixture.resume();
    assert!(fixture.prompts.try_recv().is_err());
    assert_eq!(fixture.parent_state().turns.len(), 1);
}

#[test]
fn steward_wait_cancel_is_durable_without_a_provider_runtime() {
    let fixture = WaitFixture::new();
    fixture.register();
    fixture.finish_parent(true);
    fixture.backend.sessions.lock().remove(&fixture.parent);
    fixture.sink.end_runtime();
    assert!(matches!(
        fixture
            .backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: fixture.parent,
                    runtime_id: Uuid::nil(),
                    command: Command::Cancel,
                },
                fixture.sink.clone(),
            )
            .unwrap(),
        ResponsePayload::Ack
    ));
    assert!(fixture.parent_state().steward_wait.is_none());
    let reopened = StateStore::daemon(fixture.root.join("state.db"));
    let restored = reopened.load().unwrap();
    assert!(
        restored
            .sessions
            .iter()
            .find(|s| s.id == fixture.parent)
            .unwrap()
            .steward_wait
            .is_none()
    );
    fixture.finish_child();
    fixture.resume();
    assert!(fixture.prompts.try_recv().is_err());
    assert_eq!(fixture.parent_state().turns.len(), 1);
}

#[test]
fn steward_wait_saved_callback_is_failed_when_shutdown_starts_before_submission() {
    let fixture = WaitFixture::new();
    fixture.register();
    fixture.finish_parent(true);
    fixture.finish_child();
    let prompt = "Waku automatic child-session notification".to_owned();
    let (parent, turn_id, message_id) = {
        let mut state = fixture.backend.task_state.lock();
        let parent = state
            .sessions
            .iter_mut()
            .find(|session| session.id == fixture.parent)
            .unwrap();
        parent.steward_wait = None;
        let turn_id = parent.begin_turn(prompt.clone());
        let message_id = parent.messages.last().unwrap().id;
        parent.status = SessionStatus::Connecting;
        let parent = parent.clone();
        state.mark_session_dirty(fixture.parent);
        fixture.backend.task_store.save(&mut state).unwrap();
        (parent, turn_id, message_id)
    };
    fixture.backend.quitting.store(true, Ordering::Release);
    fixture
        .backend
        .send_saved_steward_turn(
            parent,
            fixture.root.join("repo"),
            prompt,
            turn_id,
            message_id,
            None,
            &fixture.sink,
        )
        .unwrap();
    assert!(fixture.prompts.try_recv().is_err());
    let parent = fixture.parent_state();
    assert!(parent.active_turn_id().is_none());
    assert!(parent.steward_wait.is_none());
    assert_eq!(
        parent.turns.last().unwrap().status,
        crate::model::TurnStatus::Failed
    );
    assert!(
        parent
            .last_driver_error
            .as_deref()
            .is_some_and(|error| error.contains("could not run saved turn"))
    );
    let reopened = WaitFixture::open(&fixture.root);
    {
        let mut state = reopened.task_state.lock();
        let saved = state
            .sessions
            .iter_mut()
            .find(|session| session.id == fixture.parent)
            .unwrap();
        reopened.task_store.hydrate(saved).unwrap();
        assert_eq!(saved.turns.last().unwrap().id, turn_id);
        assert_eq!(
            saved.turns.last().unwrap().status,
            crate::model::TurnStatus::Failed
        );
        assert!(saved.last_driver_error.is_some());
    }
    let (sink, prompts) = WaitFixture::attach(&reopened, fixture.parent);
    reopened.resume_stewards(sink);
    assert!(
        prompts.try_recv().is_err(),
        "failed saved submission must not replay on restart"
    );
    reopened.sessions.lock().clear();
}
