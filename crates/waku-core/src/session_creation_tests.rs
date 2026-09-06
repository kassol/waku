//! Creation is exercised through the real socket, Git, SQLite and Codex transport.
use super::*;
use crate::daemon::WakuBackend;
use crate::model::{AgentSession, Project, ProviderKind, RuntimeMode};
use crate::persistence::StateStore;
use crate::settings::DaemonSettingsStore;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use waku_client::DaemonClient;

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn creates_codex_child_in_isolated_worktree_and_restores_its_history() {
    let command: Command = serde_json::from_value(json!({
        "type": "createSession", "provider": "codex", "prompt": "write fixture result"
    }))
    .unwrap();
    with_creation_daemon(|client, observer, root, project_path, _address| {
        let revisions = observer.subscribe_task_state();
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate a task");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        while revisions.try_recv().is_ok() {}
        let created =
            serde_json::to_value(client.request(parent_id, Uuid::nil(), command).unwrap()).unwrap();
        assert_eq!(created["type"], "sessionCreated");
        let child_id = Uuid::parse_str(created["session"]["id"].as_str().unwrap()).unwrap();
        let runtime_id = Uuid::parse_str(created["runtimeId"].as_str().unwrap()).unwrap();
        let path = PathBuf::from(created["workspacePath"].as_str().unwrap());
        assert!(path.starts_with(root.join("worktrees")));
        assert_ne!(path, project_path);
        assert!(created["branch"].as_str().unwrap().starts_with("waku/"));
        revisions.recv_timeout(Duration::from_secs(3)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let completed = loop {
            let ResponsePayload::Session {
                session: Some(session),
            } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::HydrateSession {
                        session_id: child_id,
                    },
                )
                .unwrap()
            else {
                panic!("missing child")
            };
            if session
                .turns
                .last()
                .is_some_and(|turn| turn.status == crate::model::TurnStatus::Completed)
            {
                break session;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not finish: {:?}",
                session.status
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            serde_json::to_value(&completed).unwrap()["parent_session_id"],
            parent_id.to_string()
        );
        assert_eq!(completed.runtime_mode, RuntimeMode::Ask);
        assert_eq!(
            completed
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["write fixture result", "Child finished."]
        );
        assert_eq!(
            std::fs::read_to_string(path.join("child-result.txt")).unwrap(),
            "created in isolated worktree\n"
        );
        assert!(!project_path.join("child-result.txt").exists());
        let calls = std::fs::read_to_string(path.join("child-calls.jsonl")).unwrap();
        assert_eq!(calls.lines().count(), 1);
        let call: serde_json::Value = serde_json::from_str(calls.lines().next().unwrap()).unwrap();
        assert_eq!(call["approvalPolicy"], "untrusted");
        assert_eq!(call["sandboxPolicy"]["type"], "readOnly");
        client
            .request(child_id, runtime_id, Command::CloseSession)
            .unwrap();
        let store = StateStore::daemon(root.join("app.db"));
        let mut restored = store.load().unwrap();
        let child = restored
            .sessions
            .iter_mut()
            .find(|session| session.id == child_id)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&*child).unwrap()["parent_session_id"],
            parent_id.to_string()
        );
        store.hydrate(child).unwrap();
        assert_eq!(child.messages.last().unwrap().content, "Child finished.");
    });
}

fn with_creation_daemon(
    test: impl FnOnce(DaemonClient, DaemonClient, &Path, &Path, std::net::SocketAddr),
) {
    let root = std::env::temp_dir().join(format!("waku-create-{}", Uuid::new_v4()));
    let project_path = root.join("project");
    std::fs::create_dir_all(&project_path).unwrap();
    git(&project_path, &["init", "-b", "main"]);
    std::fs::write(project_path.join("README.md"), "fixture\n").unwrap();
    git(&project_path, &["add", "README.md"]);
    git(
        &project_path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "Initial",
        ],
    );
    let binary = root.join("codex-fixture");
    std::fs::write(&binary, include_str!("../tests/fixtures/codex_create.py")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = DaemonSettingsStore::open(root.join("settings.json")).unwrap();
    let mut config = settings.get();
    config
        .provider_binary_overrides
        .insert(ProviderKind::Codex, binary.to_string_lossy().into());
    settings.replace(config).unwrap();
    let backend =
        Arc::new(WakuBackend::new(settings, StateStore::daemon(root.join("app.db"))).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let service = backend.clone();
    let server = std::thread::spawn(move || {
        serve(
            listener,
            "fixture".into(),
            service,
            stop,
            ServerOptions {
                allow_shutdown: true,
                ..ServerOptions::default()
            },
        )
        .unwrap()
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let client = DaemonClient::connect(&address.to_string(), "fixture".into()).unwrap();
        let observer = DaemonClient::connect(&address.to_string(), "fixture".into()).unwrap();
        test(client, observer, &root, &project_path, address);
    }));
    stopping.store(true, Ordering::Release);
    server.join().unwrap();
    drop(backend);
    std::fs::remove_dir_all(root).unwrap();
    result.unwrap();
}

#[test]
fn child_creation_reports_provider_rejection_and_retains_failed_history_and_worktree() {
    with_creation_daemon(|client, _observer, root, project_path, _address| {
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let result = client.request(
            parent_id,
            Uuid::nil(),
            Command::CreateSession {
                provider: ProviderKind::Codex,
                prompt: "reject fixture turn".into(),
                model: None,
                title: None,
                runtime_mode: None,
            },
        );
        assert!(
            result.is_err(),
            "provider rejection must fail the creation request: {result:?}"
        );
        let store = StateStore::daemon(root.join("app.db"));
        let mut restored = store.load().unwrap();
        let child = restored
            .sessions
            .iter_mut()
            .find(|session| session.parent_session_id == Some(parent_id))
            .unwrap();
        store.hydrate(child).unwrap();
        assert_eq!(
            child.turns.last().unwrap().status,
            crate::model::TurnStatus::Failed
        );
        assert!(
            child
                .messages
                .iter()
                .any(|message| message.content.contains("fixture rejected"))
        );
        let crate::model::SessionWorkspace::Worktree { path, .. } = &child.workspace else {
            panic!("missing worktree")
        };
        assert!(path.exists());
    });
}

#[test]
fn child_permissions_and_parent_relationship_are_daemon_authoritative() {
    with_creation_daemon(|client, _observer, root, project_path, _address| {
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
        parent.runtime_mode = RuntimeMode::Auto;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let create = |mode| Command::CreateSession {
            provider: ProviderKind::Codex,
            prompt: "write fixture result".into(),
            model: None,
            title: None,
            runtime_mode: mode,
        };
        assert!(
            client
                .request(parent_id, Uuid::nil(), create(None))
                .is_err(),
            "Claude auto approval cannot be inherited by Codex auto review"
        );
        let ResponsePayload::SessionCreated {
            mut session,
            runtime_id,
            workspace_path,
            ..
        } = client
            .request(parent_id, Uuid::nil(), create(Some(RuntimeMode::Ask)))
            .unwrap()
        else {
            panic!("missing child")
        };
        let child_id = session.id;
        for parent_spoof in [None, Some(Uuid::new_v4())] {
            session.parent_session_id = parent_spoof;
            let ResponsePayload::TaskStateSaved { sessions } = client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        live_session_ids: vec![child_id],
                        sessions: vec![session.clone()],
                    },
                )
                .unwrap()
            else {
                panic!("missing save acknowledgement")
            };
            assert_eq!(sessions[0].parent_session_id, Some(parent_id));
            session = sessions[0].clone();
        }
        let mut catalog_only = session.list_projection();
        catalog_only.parent_session_id = None;
        assert!(
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        live_session_ids: vec![child_id],
                        sessions: vec![catalog_only],
                    }
                )
                .is_err(),
            "an unloaded catalog's default permissions cannot overwrite the child"
        );
        let mut escalated = session.clone();
        escalated.runtime_mode = RuntimeMode::FullAccess;
        assert!(
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        live_session_ids: vec![child_id],
                        sessions: vec![escalated],
                    }
                )
                .is_err()
        );
        let mut forged = session.clone();
        forged.id = Uuid::new_v4();
        assert!(
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        live_session_ids: vec![forged.id],
                        sessions: vec![forged],
                    }
                )
                .is_err()
        );
        assert!(
            client
                .request(
                    child_id,
                    runtime_id,
                    Command::ApplyOptions {
                        options: crate::WireSessionOptions {
                            mode: "fullAccess".into(),
                            model: None,
                            reasoning_effort: None,
                            service_tier: None,
                            context_window: None,
                        }
                    }
                )
                .is_err()
        );
        assert!(
            client
                .request(
                    child_id,
                    Uuid::new_v4(),
                    Command::Start {
                        options: crate::WireDriverStartOptions {
                            provider: "codex".into(),
                            binary: root.join("codex-fixture"),
                            cwd: workspace_path,
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
                )
                .is_err()
        );
        let ResponsePayload::SessionRuntime {
            runtime_id: active, ..
        } = client
            .request(child_id, Uuid::nil(), Command::AttachSession)
            .unwrap()
        else {
            panic!("missing runtime")
        };
        assert_eq!(
            active,
            Some(runtime_id),
            "rejected options must preserve the live child"
        );
        client
            .request(child_id, runtime_id, Command::CloseSession)
            .unwrap();
        client
            .request(parent_id, Uuid::nil(), Command::RemoveSession)
            .unwrap();
        let store = StateStore::daemon(root.join("app.db"));
        let restored = store.load().unwrap();
        assert!(
            !restored
                .sessions
                .iter()
                .any(|session| session.id == parent_id)
        );
        assert_eq!(
            restored
                .sessions
                .iter()
                .find(|session| session.id == child_id)
                .unwrap()
                .parent_session_id,
            Some(parent_id)
        );
    });
}

#[test]
fn child_creation_excludes_concurrent_runtime_replacement_and_removal() {
    with_creation_daemon(|client, observer, root, project_path, _address| {
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let creation = std::thread::spawn(move || {
            observer.request(
                parent_id,
                Uuid::nil(),
                Command::CreateSession {
                    provider: ProviderKind::Codex,
                    prompt: "hold fixture turn".into(),
                    model: None,
                    title: None,
                    runtime_mode: None,
                },
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (child_id, path) = loop {
            let store = StateStore::daemon(root.join("app.db"));
            let mut state = store.load().unwrap();
            if let Some(child) = state
                .sessions
                .iter_mut()
                .find(|session| session.parent_session_id == Some(parent_id))
            {
                store.hydrate(child).unwrap();
                if let crate::model::SessionWorkspace::Worktree { path, .. } = &child.workspace {
                    if path.join("creation-waiting").exists() {
                        break (child.id, path.clone());
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "provider never reached the controlled wait"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let ResponsePayload::SessionRuntime {
            runtime_id: Some(runtime_id),
            ..
        } = client
            .request(child_id, Uuid::nil(), Command::AttachSession)
            .unwrap()
        else {
            panic!("missing runtime")
        };
        let replacement = client.request(
            child_id,
            runtime_id,
            Command::Start {
                options: crate::WireDriverStartOptions {
                    provider: "codex".into(),
                    binary: root.join("codex-fixture"),
                    cwd: path.clone(),
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
        );
        let removal = client.request(child_id, runtime_id, Command::RemoveSession);
        std::fs::write(path.join("creation-release"), "continue").unwrap();
        let created = creation.join().unwrap();
        assert!(
            replacement.is_err(),
            "a runtime cannot replace a child during creation"
        );
        assert!(
            removal.is_err(),
            "a child cannot be removed during creation"
        );
        assert!(
            matches!(created, Ok(ResponsePayload::SessionCreated { .. })),
            "{created:?}"
        );
        client
            .request(child_id, runtime_id, Command::CloseSession)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("child-calls.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        client
            .request(child_id, runtime_id, Command::RemoveSession)
            .unwrap();
    });
}

fn creation_socket(address: std::net::SocketAddr) -> WebSocket<TcpStream> {
    let stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let (mut socket, _) = tungstenite::client(format!("ws://{address}/v1"), stream).unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token: "fixture".into(),
                client_id: Uuid::new_v4(),
                resume_from: vec![],
            })
            .unwrap()
            .into(),
        ))
        .unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(socket.read().unwrap().to_text().unwrap()).unwrap(),
        ServerMessage::Hello { .. }
    ));
    socket
}

fn creation_response(socket: &mut WebSocket<TcpStream>, expected: Uuid) -> ResponseOutcome {
    loop {
        if let ServerMessage::Response {
            request_id,
            outcome,
        } = serde_json::from_str::<ServerMessage>(socket.read().unwrap().to_text().unwrap())
            .unwrap()
        {
            if request_id == expected {
                return outcome;
            }
        }
    }
}

#[test]
fn concurrent_creation_retransmissions_share_one_child_and_the_same_response() {
    with_creation_daemon(|client, _observer, root, project_path, address| {
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let request_id = Uuid::new_v4();
        let request = serde_json::to_string(&ClientMessage::Request(Request {
            request_id,
            session_id: parent_id,
            runtime_id: Uuid::nil(),
            command: Command::CreateSession {
                provider: ProviderKind::Codex,
                prompt: "hold fixture turn".into(),
                model: None,
                title: None,
                runtime_mode: None,
            },
        }))
        .unwrap();
        let mut first = creation_socket(address);
        let mut second = creation_socket(address);
        first.send(Message::Text(request.clone().into())).unwrap();
        second.send(Message::Text(request.clone().into())).unwrap();
        // Both frames have passed dispatch before this independent response.
        // Provider acceptance stays blocked by the fixture's release file.
        let barrier = Uuid::new_v4();
        second
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Request(Request {
                    request_id: barrier,
                    session_id: parent_id,
                    runtime_id: Uuid::nil(),
                    command: Command::GetSettings,
                }))
                .unwrap()
                .into(),
            ))
            .unwrap();
        assert!(matches!(
            creation_response(&mut second, barrier),
            ResponseOutcome::Ok { .. }
        ));
        let first_result =
            std::thread::spawn(move || (creation_response(&mut first, request_id), first));
        let second_result = std::thread::spawn(move || creation_response(&mut second, request_id));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let store = StateStore::daemon(root.join("app.db"));
            let mut state = store.load().unwrap();
            for child in state
                .sessions
                .iter_mut()
                .filter(|session| session.parent_session_id == Some(parent_id))
            {
                store.hydrate(child).unwrap();
                if let crate::model::SessionWorkspace::Worktree { path, .. } = &child.workspace {
                    if path.join("creation-waiting").exists() {
                        std::fs::write(path.join("creation-release"), "continue").unwrap();
                    }
                }
            }
            if first_result.is_finished() && second_result.is_finished() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "creation requests did not complete"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (first_response, mut first) = first_result.join().unwrap();
        let second_response = second_result.join().unwrap();
        assert_eq!(
            serde_json::to_value(&first_response).unwrap(),
            serde_json::to_value(&second_response).unwrap(),
            "concurrent retransmissions must receive the same result"
        );
        let ResponseOutcome::Ok {
            payload:
                ResponsePayload::SessionCreated {
                    session,
                    runtime_id,
                    workspace_path,
                    ..
                },
        } = &first_response
        else {
            panic!("{first_response:?}")
        };
        first.send(Message::Text(request.into())).unwrap();
        assert_eq!(
            serde_json::to_value(creation_response(&mut first, request_id)).unwrap(),
            serde_json::to_value(&first_response).unwrap()
        );
        client
            .request(session.id, *runtime_id, Command::CloseSession)
            .unwrap();
        let state = StateStore::daemon(root.join("app.db")).load().unwrap();
        assert_eq!(
            state
                .sessions
                .iter()
                .filter(|session| session.parent_session_id == Some(parent_id))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(workspace_path.join("child-calls.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    });
}

#[test]
fn owned_shutdown_waits_for_child_creation_then_drains_its_saved_runtime() {
    with_creation_daemon(|client, observer, root, project_path, _address| {
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let create = |prompt: &str| Command::CreateSession {
            provider: ProviderKind::Codex,
            prompt: prompt.into(),
            model: None,
            title: None,
            runtime_mode: None,
        };
        let creation = std::thread::spawn(move || {
            observer.request(parent_id, Uuid::nil(), create("hold fixture turn"))
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (child_id, path) = loop {
            let store = StateStore::daemon(root.join("app.db"));
            let mut state = store.load().unwrap();
            if let Some(child) = state
                .sessions
                .iter_mut()
                .find(|session| session.parent_session_id == Some(parent_id))
            {
                store.hydrate(child).unwrap();
                if let crate::model::SessionWorkspace::Worktree { path, .. } = &child.workspace {
                    if path.join("creation-waiting").exists() {
                        break (child.id, path.clone());
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "provider never reached the controlled wait"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let exiting_client = client.clone();
        let exiting = std::thread::spawn(move || {
            exiting_client.request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown)
        });
        // An empty prompt has no side effects if it arrives before the exit
        // worker. Its error changes once the daemon has closed the work gate.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let error = client
                .request(parent_id, Uuid::nil(), create(""))
                .unwrap_err();
            if error.to_string().contains("shutting down") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exit never rejected new work: {error:#}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !exiting.is_finished(),
            "safe exit must wait for the accepted creation"
        );
        assert!(
            !creation.is_finished(),
            "the provider has not accepted the first prompt yet"
        );
        let rejected = client
            .request(parent_id, Uuid::nil(), create("write fixture result"))
            .unwrap_err();
        assert!(rejected.to_string().contains("shutting down"));
        std::fs::write(path.join("creation-release"), "continue").unwrap();
        let ResponsePayload::SessionCreated {
            session,
            runtime_id,
            ..
        } = creation.join().unwrap().unwrap()
        else {
            panic!("missing creation result")
        };
        assert_eq!(session.id, child_id);
        assert!(matches!(
            exiting.join().unwrap().unwrap(),
            ResponsePayload::Ack
        ));
        assert!(matches!(
            client
                .request(child_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: None,
                ..
            }
        ));
        let store = StateStore::daemon(root.join("app.db"));
        let mut state = store.load().unwrap();
        assert_eq!(
            state
                .sessions
                .iter()
                .filter(|session| session.parent_session_id == Some(parent_id))
                .count(),
            1
        );
        let child = state
            .sessions
            .iter_mut()
            .find(|session| session.id == child_id)
            .unwrap();
        store.hydrate(child).unwrap();
        assert_eq!(child.messages[0].content, "hold fixture turn");
        assert!(child.active_turn_id().is_none());
        assert_eq!(child.history_save_error, None);
        let saved = child
            .history_saved_cursor
            .expect("exit acknowledges durable history");
        assert_eq!(Some(saved), child.runtime_event_cursor);
        assert_eq!(saved.runtime_id, runtime_id);
        let ResponsePayload::EventReplay { events } = client
            .request(
                child_id,
                runtime_id,
                Command::ReplayEvents {
                    cursor: ReplayCursor {
                        session_id: child_id,
                        runtime_id,
                        epoch: saved.epoch,
                        sequence: 0,
                    },
                },
            )
            .unwrap()
        else {
            panic!("missing durable replay")
        };
        assert!(
            events
                .iter()
                .any(|event| event.event.kind == "processExited")
        );
        assert_eq!(events.last().unwrap().sequence, saved.sequence);
        assert_eq!(
            std::fs::read_to_string(path.join("child-calls.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    });
}

#[test]
fn failed_initial_child_save_disables_new_creation_before_provider_start() {
    with_creation_daemon(|client, _observer, root, project_path, _address| {
        let project = Project::from_path(project_path.to_owned());
        let project_id = project.id;
        let mut parent = AgentSession::new(project_id, ProviderKind::Codex);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate");
        let parent_id = parent.id;
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    live_session_ids: vec![parent_id],
                    sessions: vec![parent],
                },
            )
            .unwrap();
        let connection = rusqlite::Connection::open(root.join("app.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_child_save BEFORE INSERT ON sessions
             WHEN NEW.parent_session_id IS NOT NULL
             BEGIN SELECT RAISE(FAIL, 'fixture child save failure'); END;",
            )
            .unwrap();
        let create = || Command::CreateSession {
            provider: ProviderKind::Codex,
            prompt: "write fixture result".into(),
            model: None,
            title: None,
            runtime_mode: None,
        };
        let failed_save = client
            .request(parent_id, Uuid::nil(), create())
            .unwrap_err();
        assert!(
            failed_save.to_string().contains("could not save child"),
            "{failed_save:#}"
        );
        connection
            .execute_batch("DROP TRIGGER reject_child_save;")
            .unwrap();
        let rejected = client
            .request(parent_id, Uuid::nil(), create())
            .unwrap_err();
        assert!(
            rejected
                .to_string()
                .contains("history saving failed; new work is disabled"),
            "{rejected:#}"
        );
        let state = StateStore::daemon(root.join("app.db")).load().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].id, parent_id);
        let worktrees = std::fs::read_dir(root.join("worktrees").join(project_id.to_string()))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(
            worktrees.len(),
            1,
            "failed save retains its directory; retry allocates nothing"
        );
        assert!(
            !worktrees[0].join("child-calls.jsonl").exists(),
            "no provider prompt may run after the failed initial save"
        );
        assert!(!worktrees[0].join("child-result.txt").exists());
    });
}

#[path = "steward_tests.rs"]
mod steward_tests;
