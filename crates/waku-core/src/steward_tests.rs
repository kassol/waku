use super::*;
use std::io::Cursor;

fn start_steward(
    client: &DaemonClient,
    root: &Path,
    project_path: &Path,
) -> (AgentSession, Project, serde_json::Value, Uuid) {
    let project = Project::from_path(project_path.to_owned());
    let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
    parent.runtime_mode = RuntimeMode::Ask;
    parent.begin_turn("Delegate");
    start_steward_saved(client, root, project_path, parent, project)
}

fn start_steward_saved(
    client: &DaemonClient,
    root: &Path,
    project_path: &Path,
    parent: AgentSession,
    project: Project,
) -> (AgentSession, Project, serde_json::Value, Uuid) {
    start_steward_saved_with_script(client, root, project_path, parent, project, None)
}

fn start_steward_saved_with_script(
    client: &DaemonClient,
    root: &Path,
    project_path: &Path,
    parent: AgentSession,
    project: Project,
    script: Option<&str>,
) -> (AgentSession, Project, serde_json::Value, Uuid) {
    client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![project.clone()],
                sessions: vec![parent.clone()],
                live_session_ids: vec![parent.id],
            },
        )
        .unwrap();
    let binary = root.join("claude-fixture");
    std::fs::write(&binary, script.unwrap_or("#!/usr/bin/env python3\nimport sys,pathlib\npathlib.Path('mcp-config.json').write_text(sys.argv[sys.argv.index('--mcp-config')+1])\nassert '--strict-mcp-config' not in sys.argv\nfor line in sys.stdin: pass\n")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runtime = Uuid::new_v4();
    client
        .request(
            parent.id,
            runtime,
            Command::Start {
                options: crate::WireDriverStartOptions {
                    provider: "claude".into(),
                    binary,
                    cwd: project_path.into(),
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
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let config = loop {
        if let Ok(text) = std::fs::read_to_string(project_path.join("mcp-config.json")) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                break value;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Claude did not receive session MCP config"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(config["mcpServers"]["waku"]["args"], json!(["mcp"]));
    assert!(!config.to_string().contains("WAKU_DAEMON_TOKEN"));
    (parent, project, config, runtime)
}

fn scoped_socket(address: std::net::SocketAddr, token: &str) -> WebSocket<TcpStream> {
    let stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let (mut socket, _) = tungstenite::client(format!("ws://{address}/v1"), stream).unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token: token.into(),
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
fn scoped_request(socket: &mut WebSocket<TcpStream>, request: Request) -> ResponseOutcome {
    let id = request.request_id;
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientMessage::Request(request))
                .unwrap()
                .into(),
        ))
        .unwrap();
    // A scoped connection must never return a catalog broadcast, persistence
    // notification or replay while waiting for its response.
    match serde_json::from_str::<ServerMessage>(socket.read().unwrap().to_text().unwrap()).unwrap()
    {
        ServerMessage::Response {
            request_id,
            outcome,
        } if request_id == id => outcome,
        message => panic!("unexpected scoped data: {message:?}"),
    }
}
fn spawn_command(prompt: &str) -> Command {
    Command::CreateSession {
        provider: ProviderKind::Codex,
        prompt: prompt.into(),
        model: None,
        title: None,
        runtime_mode: None,
        idempotency_key: None,
        workspace: crate::protocol::CreationWorkspace::Worktree,
    }
}

#[test]
fn mcp_stdio_creates_real_child_and_rejects_spoofed_arguments() {
    with_creation_daemon(|client, observer, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"waku_spawn_session","arguments":{"provider":"codex","prompt":"delegate","parent_session_id":Uuid::new_v4()}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"waku_spawn_session","arguments":{"provider":"codex","prompt":"delegate","runtime_mode":"fullAccess"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"waku_spawn_session","arguments":{"provider":"codex","prompt":"write fixture result"}}}),
            json!([]),
        ].iter().map(|value| format!("{value}\n")).collect::<String>();
        let output = run_mcp(input, address, token, parent.id, runtime);
        let replies: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 6);
        assert_eq!(replies[5]["error"]["code"], -32600);
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 7);
        assert_eq!(replies[2]["error"]["code"], -32602);
        assert_eq!(replies[3]["result"]["isError"], true);
        assert_eq!(replies[4]["result"]["isError"], false);
        let child: serde_json::Value =
            serde_json::from_str(replies[4]["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        let id = Uuid::parse_str(child["session_id"].as_str().unwrap()).unwrap();
        let ResponsePayload::Session {
            session: Some(session),
        } = observer
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::HydrateSession { session_id: id },
            )
            .unwrap()
        else {
            panic!("child not visible")
        };
        assert_eq!(session.parent_session_id, Some(parent.id));
        assert_eq!(session.project_id, parent.project_id);
        let path = PathBuf::from(child["workspace_path"].as_str().unwrap());
        assert!(path.starts_with(root.join("worktrees")));
        assert!(child["branch"].as_str().unwrap().starts_with("waku/"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.join("child-result.txt").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(path.join("child-result.txt")).unwrap(),
            "created in isolated worktree\n"
        );
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

#[test]
fn scoped_socket_denies_management_spoofing_cached_leaks_and_revoked_runtime() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, project, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let mut socket = scoped_socket(address, token);
        for command in [
            Command::LoadTaskState,
            Command::GetSettings,
            Command::PrepareShutdown,
            Command::ShutdownDaemon,
            Command::CloseSession,
        ] {
            let result = scoped_request(
                &mut socket,
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: parent.id,
                    runtime_id: runtime,
                    command,
                },
            );
            assert!(matches!(result, ResponseOutcome::Error { .. }));
        }
        assert!(matches!(
            scoped_request(
                &mut socket,
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: Uuid::new_v4(),
                    runtime_id: runtime,
                    command: spawn_command("spoof")
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        // Reuse a desktop request UUID with an otherwise authorized command.
        let collision = Uuid::new_v4();
        let mut desktop = creation_socket(address);
        desktop
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Request(Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: Command::GetSettings,
                }))
                .unwrap()
                .into(),
            ))
            .unwrap();
        assert!(matches!(
            creation_response(&mut desktop, collision),
            ResponseOutcome::Ok { .. }
        ));
        assert!(matches!(
            scoped_request(
                &mut socket,
                Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("")
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        let collision = Uuid::new_v4();
        assert!(matches!(
            scoped_request(
                &mut socket,
                Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("write fixture result")
                }
            ),
            ResponseOutcome::Ok { .. }
        ));
        let replacement = Project::from_path(root.join("other-project"));
        let ResponsePayload::Session {
            session: Some(mut moved),
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
            panic!("parent missing")
        };
        moved.project_id = replacement.id;
        moved.updated_at = moved.updated_at.saturating_add(1);
        let mut restored = moved.clone();
        restored.project_id = project.id;
        restored.updated_at = restored.updated_at.saturating_add(1);
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![replacement],
                    sessions: vec![moved],
                    live_session_ids: vec![parent.id],
                },
            )
            .unwrap();
        assert!(matches!(
            scoped_request(
                &mut socket,
                Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("")
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    sessions: vec![restored],
                    live_session_ids: vec![parent.id],
                },
            )
            .unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Shutdown)
                    .unwrap()
                    .into(),
            ))
            .unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(socket.read().unwrap().to_text().unwrap())
                .unwrap(),
            ServerMessage::Rejected { .. }
        ));
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
        assert!(matches!(
            scoped_request(
                &mut socket,
                Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("")
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        let client = DaemonClient::connect(&address.to_string(), token.into());
        assert!(client.is_err(), "revoked token reauthenticated");
    });
}

#[test]
fn scoped_inflight_uuid_cannot_join_a_desktop_request() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let mut scoped = scoped_socket(address, token);
        scoped
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut desktop = creation_socket(address);
        let collision = Uuid::new_v4();
        desktop
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Request(Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("hold fixture turn"),
                }))
                .unwrap()
                .into(),
            ))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let path = loop {
            let waiting =
                std::fs::read_dir(root.join("worktrees").join(parent.project_id.to_string()))
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| path.join("creation-waiting").exists());
            if let Some(path) = waiting {
                break path;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(matches!(
            scoped_request(
                &mut scoped,
                Request {
                    request_id: collision,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: spawn_command("")
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        std::fs::write(path.join("creation-release"), "").unwrap();
        assert!(matches!(
            creation_response(&mut desktop, collision),
            ResponseOutcome::Ok { .. }
        ));
        // A provider event and catalog change have happened on another channel.
        assert!(matches!(
            scoped_request(
                &mut scoped,
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: Command::LoadTaskState
                }
            ),
            ResponseOutcome::Error { .. }
        ));
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

fn mcp_response(
    address: std::net::SocketAddr,
    token: &str,
    parent: Uuid,
    runtime: Uuid,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let input = format!(
        "{}\n{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}})
    );
    let output = run_mcp(input, address, token, parent, runtime);
    let response: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().lines().last().unwrap()).unwrap();
    response
}

fn mcp_tool(
    address: std::net::SocketAddr,
    token: &str,
    parent: Uuid,
    runtime: Uuid,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let response = mcp_response(address, token, parent, runtime, name, arguments);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn mcp_query_lists_only_direct_children() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let ResponsePayload::SessionCreated { session: child, .. } = client
            .request(parent.id, runtime, spawn_command("write fixture result"))
            .unwrap()
        else {
            panic!("child missing")
        };
        let result = mcp_tool(
            address,
            token,
            parent.id,
            runtime,
            "waku_list_sessions",
            json!({}),
        );
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["sessions"][0]["session_id"], child.id.to_string());
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

fn seed_query_sessions(
    root: &Path,
    project_path: &Path,
) -> (Project, AgentSession, Vec<AgentSession>) {
    use crate::model::{DriverEvent, MessageRole, TurnStatus};
    let project = Project::from_path(project_path.into());
    let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
    parent.runtime_mode = RuntimeMode::Ask;
    parent.begin_turn("delegate");
    parent.finish_active_turn(TurnStatus::Completed);
    parent.status = SessionStatus::Idle;
    let mut child = AgentSession::new(project.id, ProviderKind::Codex);
    child.parent_session_id = Some(parent.id);
    child.runtime_mode = RuntimeMode::Ask;
    child.begin_turn("previous task");
    child.push_message(MessageRole::Assistant, "previous answer");
    child.finish_active_turn(TurnStatus::Completed);
    child.begin_turn("current task");
    let mut reducer = waku_protocol::history::HistoryReducer::default();
    reducer.apply(&mut child, DriverEvent::TurnStarted);
    reducer.apply(&mut child, DriverEvent::TextDelta("前段".into()));
    reducer.apply(
        &mut child,
        DriverEvent::Activity {
            id: Some("tool".into()),
            kind: crate::model::ActivityKind::Command,
            title: "Run check".into(),
            detail: Some("check output".into()),
            complete: true,
        },
    );
    reducer.apply(&mut child, DriverEvent::TextDelta("后段🌍".into()));
    reducer.apply(
        &mut child,
        DriverEvent::TurnFinished {
            success: true,
            summary: None,
        },
    );
    let mut empty = AgentSession::new(project.id, ProviderKind::Codex);
    empty.parent_session_id = Some(parent.id);
    empty.runtime_mode = RuntimeMode::Ask;
    let mut grandchild = AgentSession::new(project.id, ProviderKind::Codex);
    grandchild.parent_session_id = Some(child.id);
    grandchild.runtime_mode = RuntimeMode::Ask;
    let mut foreign = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    foreign.parent_session_id = Some(parent.id);
    foreign.runtime_mode = RuntimeMode::Ask;
    for session in [&mut empty, &mut grandchild, &mut foreign] {
        session.push_message(MessageRole::User, "legacy history without a turn");
    }
    let children = vec![child, empty, grandchild, foreign];
    let store = StateStore::daemon(root.join("app.db"));
    let mut state = store.load().unwrap();
    state.projects = vec![project.clone()];
    state.sessions = std::iter::once(parent.clone())
        .chain(children.clone())
        .collect();
    for id in state
        .sessions
        .iter()
        .map(|session| session.id)
        .collect::<Vec<_>>()
    {
        state.mark_session_dirty(id);
    }
    store.save(&mut state).unwrap();
    (project, parent, children)
}

#[test]
fn mcp_status_waits_for_changes_and_distinguishes_no_turn() {
    with_creation_daemon_seed(
        seed_query_sessions,
        |client, _, root, project_path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, project_path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let ids = vec![children[0].id, children[1].id];
            let snapshot = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_status",
                json!({"session_ids":ids}),
            );
            let summaries = snapshot["sessions"].as_array().unwrap();
            let completed = summaries
                .iter()
                .find(|s| s["session_id"] == children[0].id.to_string())
                .unwrap();
            let empty = summaries
                .iter()
                .find(|s| s["session_id"] == children[1].id.to_string())
                .unwrap();
            assert_eq!(completed["status"], "idle");
            assert_eq!(completed["turn"]["status"], "completed");
            assert!(empty["turn"].is_null());
            let started = std::time::Instant::now();
            let timeout = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_status",
                json!({"session_ids":ids,"wait_ms":100}),
            );
            assert_eq!(timeout["timed_out"], true);
            assert!(started.elapsed() >= Duration::from_millis(100));
            let token_owned = token.to_owned();
            let parent_id = parent.id;
            let targets = ids.clone();
            let waiting = std::thread::spawn(move || {
                mcp_tool(
                    address,
                    &token_owned,
                    parent_id,
                    runtime,
                    "waku_status",
                    json!({"session_ids":targets,"wait_ms":3000}),
                )
            });
            std::thread::sleep(Duration::from_millis(250));
            let mut changed = children[1].clone();
            changed.begin_turn("new task");
            changed.status = SessionStatus::Background;
            changed.updated_at += 1;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![changed],
                        live_session_ids: vec![children[1].id],
                    },
                )
                .unwrap();
            let updated = waiting.join().unwrap();
            assert_eq!(updated["timed_out"], false);
            let changed = updated["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["session_id"] == children[1].id.to_string())
                .unwrap();
            assert_eq!(changed["status"], "background");
            assert_eq!(changed["turn"]["status"], "running");
            assert_eq!(changed["turn_open"], true);
            let mut waiting_child = children[1].clone();
            waiting_child.begin_turn("needs approval");
            waiting_child.status = SessionStatus::Waiting;
            waiting_child.updated_at += 2;
            waiting_child.pending_permission = Some(crate::model::PendingPermission {
                request_id: "permission".into(),
                title: "Run check".into(),
                detail: "check".into(),
                options: vec![],
            });
            waiting_child.pending_user_input = Some(crate::model::UserInputRequest {
                request_id: "question".into(),
                questions: vec![],
            });
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![waiting_child],
                        live_session_ids: vec![children[1].id],
                    },
                )
                .unwrap();
            let waiting = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_status",
                json!({"session_ids":[children[1].id]}),
            );
            assert_eq!(waiting["sessions"][0]["status"], "waiting");
            assert_eq!(waiting["sessions"][0]["turn"]["status"], "running");
            assert_eq!(
                waiting["sessions"][0]["waiting_for"],
                json!(["permission", "userInput"])
            );
            client
                .request(parent.id, runtime, Command::CloseSession)
                .unwrap();
        },
    );
}

#[test]
fn mcp_result_preserves_turn_order_and_unicode_boundaries() {
    with_creation_daemon_seed(
        seed_query_sessions,
        |client, _, root, project_path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, project_path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let result = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[0].id,"include_transcript":true}),
            );
            assert_eq!(result["reply"], "前段\n\n后段🌍");
            assert_eq!(result["session"]["turn"]["status"], "completed");
            assert_eq!(result["reply_truncated"], false);
            let transcript = result["transcript"].as_str().unwrap();
            assert!(transcript.find("前段").unwrap() < transcript.find("Run check").unwrap());
            assert!(transcript.find("Run check").unwrap() < transcript.find("后段🌍").unwrap());
            let short = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[0].id,"max_chars":5,"include_transcript":true}),
            );
            assert_eq!(short["reply"], "前段\n\n后");
            assert_eq!(short["reply_truncated"], true);
            assert_eq!(short["transcript_truncated"], true);
            let empty = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[1].id}),
            );
            assert!(empty["session"]["turn"].is_null());
            assert_eq!(empty["reply"], "");
            assert!(empty["transcript"].is_null());
            let mut next = children[0].clone();
            next.begin_turn("next unfinished task");
            next.status = SessionStatus::Working;
            next.updated_at += 1;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![next.clone()],
                        live_session_ids: vec![next.id],
                    },
                )
                .unwrap();
            let running = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":next.id}),
            );
            assert_eq!(running["session"]["turn"]["status"], "running");
            assert_eq!(running["reply"], "");
            for status in [
                crate::model::TurnStatus::Failed,
                crate::model::TurnStatus::Interrupted,
            ] {
                let mut ended = next.clone();
                ended.finish_active_turn(status);
                ended.updated_at += 2;
                ended.status = SessionStatus::Idle;
                client
                    .request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::SaveTaskState {
                            projects: vec![],
                            sessions: vec![ended],
                            live_session_ids: vec![next.id],
                        },
                    )
                    .unwrap();
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":next.id}),
                );
                assert_eq!(
                    result["session"]["turn"]["status"],
                    serde_json::to_value(status).unwrap()
                );
                assert_eq!(result["reply"], "");
            }
            client
                .request(parent.id, runtime, Command::CloseSession)
                .unwrap();
        },
    );
}

#[test]
fn steward_queries_recheck_targets_and_do_not_cache_results() {
    use waku_protocol::StewardQuery;
    with_creation_daemon_seed(
        seed_query_sessions,
        |client, _, root, project_path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, project_path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let mut socket = scoped_socket(address, token);
            let request_id = Uuid::new_v4();
            let request = |query| Request {
                request_id,
                session_id: parent.id,
                runtime_id: runtime,
                command: Command::StewardQuery { query },
            };
            let result = scoped_request(&mut socket, request(StewardQuery::ListSessions {}));
            let ResponseOutcome::Ok {
                payload: ResponsePayload::ChildSessions { sessions },
            } = result
            else {
                panic!("list failed")
            };
            assert_eq!(sessions.len(), 2);
            for target in [parent.id, children[2].id, children[3].id, Uuid::new_v4()] {
                for query in [
                    StewardQuery::Status {
                        session_ids: vec![children[0].id, target],
                        wait_ms: 0,
                    },
                    StewardQuery::Result {
                        session_id: target,
                        include_transcript: true,
                        max_chars: None,
                    },
                ] {
                    assert!(matches!(
                        scoped_request(&mut socket, request(query)),
                        ResponseOutcome::Error { .. }
                    ));
                }
            }
            let query = StewardQuery::Result {
                session_id: children[0].id,
                include_transcript: false,
                max_chars: None,
            };
            assert!(matches!(
                scoped_request(&mut socket, request(query.clone())),
                ResponseOutcome::Ok {
                    payload: ResponsePayload::ChildResult { .. }
                }
            ));
            let mut changed = children[0].clone();
            changed.project_id = Uuid::new_v4();
            changed.updated_at += 1;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![changed],
                        live_session_ids: vec![children[0].id],
                    },
                )
                .unwrap();
            assert!(
                matches!(
                    scoped_request(&mut socket, request(query)),
                    ResponseOutcome::Error { .. }
                ),
                "cached result bypassed changed project"
            );
            for (name, args) in [
                ("waku_list_sessions", json!({"parent_only":false})),
                ("waku_list_sessions", json!({"parent_session_id":parent.id})),
                (
                    "waku_status",
                    json!({"session_ids":[children[1].id],"wait_ms":-1}),
                ),
                (
                    "waku_status",
                    json!({"session_ids":[children[1].id],"wait_ms":60001}),
                ),
                ("waku_status", json!({"session_ids":[]})),
                (
                    "waku_result",
                    json!({"session_id":children[1].id,"max_chars":0}),
                ),
                (
                    "waku_result",
                    json!({"session_id":children[1].id,"max_chars":100001}),
                ),
            ] {
                let response = mcp_response(address, token, parent.id, runtime, name, args);
                assert!(
                    response.get("error").is_some() || response["result"]["isError"] == true,
                    "{response}"
                );
            }
            client
                .request(parent.id, runtime, Command::CloseSession)
                .unwrap();
            assert!(matches!(
                scoped_request(&mut socket, request(StewardQuery::ListSessions {})),
                ResponseOutcome::Error { .. }
            ));
        },
    );
}

#[test]
fn steward_status_wait_is_revoked_when_parent_runtime_ends() {
    with_creation_daemon_seed(
        seed_query_sessions,
        |client, _, root, project_path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, project_path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap()
                .to_owned();
            let parent_id = parent.id;
            let target = children[0].id;
            let waiting = std::thread::spawn(move || {
                mcp_response(
                    address,
                    &token,
                    parent_id,
                    runtime,
                    "waku_status",
                    json!({"session_ids":[target],"wait_ms":60000}),
                )
            });
            std::thread::sleep(Duration::from_millis(250));
            let started = std::time::Instant::now();
            client
                .request(parent.id, runtime, Command::CloseSession)
                .unwrap();
            let result = waiting.join().unwrap();
            assert_eq!(result["result"]["isError"], true, "{result}");
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "revocation waited for status timeout"
            );
        },
    );
}

fn run_mcp(
    input: String,
    address: std::net::SocketAddr,
    token: &str,
    parent: Uuid,
    runtime: Uuid,
) -> Vec<u8> {
    let mut output = Vec::new();
    if let Ok(binary) = std::env::var("WAKU_TEST_MCP_BINARY") {
        use std::io::Write;
        let mut child = std::process::Command::new(binary)
            .arg("mcp")
            .env("WAKU_MCP_ADDRESS", address.to_string())
            .env("WAKU_MCP_TOKEN", token)
            .env("WAKU_MCP_SESSION", parent.to_string())
            .env("WAKU_MCP_RUNTIME", runtime.to_string())
            .env_remove(crate::protocol::DAEMON_TOKEN_ENV)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        output = result.stdout;
    } else {
        crate::mcp::run_stdio(
            Cursor::new(input),
            &mut output,
            &address.to_string(),
            token,
            parent,
            runtime,
        )
        .unwrap();
    }

    output
}

#[test]
fn result_queries_progress_while_provider_events_are_persisted() {
    use waku_protocol::StewardQuery;
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let ResponsePayload::SessionCreated { session: child, .. } = client
            .request(parent.id, runtime, spawn_command("stream query result"))
            .unwrap()
        else {
            panic!("child missing")
        };
        let mut socket = scoped_socket(address, token);
        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request_id = Uuid::new_v4();
        let mut saw_running = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let outcome = scoped_request(
                &mut socket,
                Request {
                    request_id,
                    session_id: parent.id,
                    runtime_id: runtime,
                    command: Command::StewardQuery {
                        query: StewardQuery::Result {
                            session_id: child.id,
                            include_transcript: true,
                            max_chars: None,
                        },
                    },
                },
            );
            let ResponseOutcome::Ok {
                payload: ResponsePayload::ChildResult { session, reply, .. },
            } = outcome
            else {
                panic!("result query failed")
            };
            if session
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status == crate::model::TurnStatus::Completed)
            {
                assert_eq!(reply, "x".repeat(100));
                break;
            }
            saw_running = true;
            assert!(
                std::time::Instant::now() < deadline,
                "query or provider made no progress"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(saw_running);
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

#[test]
fn query_preserves_failed_turn_reason_across_storage_restart() {
    fn seed_failed(root: &Path, path: &Path) -> (Project, AgentSession, Vec<AgentSession>) {
        let (project, parent, mut children) = seed_query_sessions(root, path);
        let child = &mut children[0];
        child.begin_turn("fail after partial output");
        let mut reducer = waku_protocol::history::HistoryReducer::default();
        reducer.apply(child, crate::model::DriverEvent::TurnStarted);
        reducer.apply(
            child,
            crate::model::DriverEvent::TextDelta("partial".into()),
        );
        reducer.apply(
            child,
            crate::model::DriverEvent::Error("provider lost connection".into()),
        );
        reducer.apply(child, crate::model::DriverEvent::ProcessExited);
        reducer.apply(
            child,
            crate::model::DriverEvent::Connected {
                provider_cursor: None,
            },
        );
        let store = StateStore::daemon(root.join("app.db"));
        let mut state = store.load().unwrap();
        *state
            .sessions
            .iter_mut()
            .find(|s| s.id == child.id)
            .unwrap() = child.clone();
        state.mark_session_dirty(child.id);
        store.save(&mut state).unwrap();
        (project, parent, children)
    }
    with_creation_daemon_seed(
        seed_failed,
        |client, _, root, project_path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, project_path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let result = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[0].id}),
            );
            assert_eq!(result["session"]["error"], "provider lost connection");
            assert_eq!(result["session"]["turn"]["status"], "failed");
            assert_eq!(result["reply"], "partial");
            let status = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_status",
                json!({"session_ids":[children[0].id]}),
            );
            assert_eq!(status["sessions"][0]["error"], "provider lost connection");
            let mut next = children[0].clone();
            let mut reducer = waku_protocol::history::HistoryReducer::default();
            reducer.apply(
                &mut next,
                crate::model::DriverEvent::PromptSubmitted {
                    message: "retry".into(),
                    turn_id: Uuid::new_v4(),
                    message_id: Uuid::new_v4(),
                },
            );
            next.updated_at += 1;
            client
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: vec![],
                        sessions: vec![next],
                        live_session_ids: vec![children[0].id],
                    },
                )
                .unwrap();
            let next = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[0].id}),
            );
            assert!(next["session"]["error"].is_null());
            assert_eq!(next["session"]["turn"]["status"], "running");
            assert_eq!(next["reply"], "");
            client
                .request(parent.id, runtime, Command::CloseSession)
                .unwrap();
        },
    );
}

#[path = "recursion_tests.rs"]
mod recursion_tests;

#[test]
fn mcp_spawn_advertises_and_deduplicates_explicit_workspace_requests() {
    with_creation_daemon(|client, _observer, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let arguments = json!({"provider":"codex", "prompt":"write fixture result", "workspace":"local", "idempotency_key":"mcp-key"});
        let input = [json!({"jsonrpc":"2.0","id":0,"method":"initialize"}),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"waku_spawn_session","arguments":arguments}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"waku_spawn_session","arguments":arguments}}),
        ].iter().map(|value| format!("{value}\n")).collect::<String>();
        let mut output = Vec::new();
        crate::mcp::run_stdio(
            Cursor::new(input),
            &mut output,
            &address.to_string(),
            token,
            parent.id,
            runtime,
        )
        .unwrap();
        let replies = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let properties = &replies[1]["result"]["tools"][0]["inputSchema"]["properties"];
        assert_eq!(
            properties["workspace"]["enum"],
            json!(["worktree", "inherit", "local"])
        );
        assert_eq!(properties["idempotency_key"]["type"], "string");
        assert_eq!(replies[2]["result"]["isError"], false);
        assert_eq!(replies[2]["result"], replies[3]["result"]);
        let created: serde_json::Value =
            serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(created["workspace_path"], project_path.to_str().unwrap());
        assert!(created["branch"].is_null());
        assert_eq!(
            std::fs::read_to_string(project_path.join("child-calls.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    });
}

#[test]
fn mcp_prompt_and_cancel_preserve_durable_turn_boundaries() {
    with_creation_daemon_seed(
        |root, path| {
            std::fs::write(
                root.join("codex-fixture"),
                include_str!("../tests/fixtures/codex_prompt_cancel.py"),
            )
            .unwrap();
            let seeded = seed_query_sessions(root, path);
            let store = StateStore::daemon(root.join("app.db"));
            let mut state = store.load().unwrap();
            let waiting = state
                .sessions
                .iter_mut()
                .find(|s| s.id == seeded.2[1].id)
                .unwrap();
            store.hydrate(waiting).unwrap();
            waiting.pending_user_input = Some(crate::model::UserInputRequest {
                request_id: "question".into(),
                questions: vec![],
            });
            state.mark_session_dirty(seeded.2[1].id);
            store.save(&mut state).unwrap();
            seeded
        },
        |client, _, root, path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let child = children[0].id;
            let waiting = mcp_response(
                address,
                token,
                parent.id,
                runtime,
                "waku_prompt",
                json!({"session_id":children[1].id,"prompt":"wait for the user"}),
            );
            assert_eq!(waiting["result"]["isError"], true, "{waiting}");
            for target in [parent.id, children[2].id, children[3].id] {
                for (name, args) in [
                    (
                        "waku_prompt",
                        json!({"session_id":target,"prompt":"forbidden"}),
                    ),
                    ("waku_cancel", json!({"session_id":target})),
                ] {
                    let reply = mcp_response(address, token, parent.id, runtime, name, args);
                    assert_eq!(reply["result"]["isError"], true, "{reply}");
                }
            }
            let results = std::thread::scope(|scope| {
                let requests = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            mcp_response(
                                address,
                                token,
                                parent.id,
                                runtime,
                                "waku_prompt",
                                json!({"session_id":child,"prompt":"continue"}),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                requests
                    .into_iter()
                    .map(|request| request.join().unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                results
                    .iter()
                    .filter(|r| r["result"]["isError"] == false)
                    .count(),
                1,
                "{results:?}"
            );
            let accepted = results
                .iter()
                .find(|r| r["result"]["isError"] == false)
                .unwrap();
            let accepted: serde_json::Value =
                serde_json::from_str(accepted["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            let turn_id = accepted["turn_id"].as_str().unwrap();
            let store = StateStore::daemon(root.join("app.db"));
            let mut saved = store.load().unwrap();
            let saved = saved.sessions.iter_mut().find(|s| s.id == child).unwrap();
            store.hydrate(saved).unwrap();
            assert_eq!(saved.turns.last().unwrap().id.to_string(), turn_id);
            assert_eq!(saved.messages.last().unwrap().content, "continue");
            let result = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":child}),
            );
            assert_eq!(result["reply"], "");
            assert_eq!(result["session"]["turn"]["status"], "running");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !path.join("prompt-ready").exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            for _ in 0..2 {
                let cancelled = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_cancel",
                    json!({"session_id":child}),
                );
                assert_eq!(cancelled["accepted"], true);
                assert_eq!(cancelled["stopped"], false);
                assert_eq!(cancelled["session"]["turn"]["status"], "running");
            }
            assert_eq!(
                mcp_response(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_prompt",
                    json!({"session_id":child,"prompt":"too early"})
                )["result"]["isError"],
                true
            );
            std::fs::write(path.join("cancel-release"), "").unwrap();
            loop {
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":child,"include_transcript":true}),
                );
                if result["session"]["turn"]["status"] == "interrupted" {
                    assert!(
                        result["transcript"]
                            .as_str()
                            .unwrap()
                            .contains("previous answer")
                    );
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{result}");
                std::thread::sleep(Duration::from_millis(20));
            }
            let cancelled = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_cancel",
                json!({"session_id":child}),
            );
            assert_eq!(cancelled["stopped"], true);
            // Lose the next prompt's response. The saved turn is discoverable
            // through a fresh connection; no transport retries the submission.
            let mut socket = scoped_socket(address, token);
            socket
                .send(Message::Text(
                    serde_json::to_string(&ClientMessage::Request(Request {
                        request_id: Uuid::new_v4(),
                        session_id: parent.id,
                        runtime_id: runtime,
                        command: Command::StewardPrompt {
                            child_session_id: child,
                            prompt: "after disconnect".into(),
                        },
                    }))
                    .unwrap()
                    .into(),
                ))
                .unwrap();
            drop(socket);
            loop {
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":child}),
                );
                if result["session"]["turn"]["turn_id"] != turn_id {
                    assert_eq!(result["reply"], "");
                    assert_eq!(result["session"]["turn"]["status"], "running");
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
            loop {
                let calls = std::fs::read_to_string(path.join("prompt-calls.jsonl")).unwrap();
                if calls.lines().count() == 2 {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{calls}");
                std::thread::sleep(Duration::from_millis(20));
            }
            mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_cancel",
                json!({"session_id":child}),
            );
            loop {
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":child}),
                );
                if result["session"]["turn"]["status"] == "interrupted" {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
            let accepted = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_prompt",
                json!({"session_id":child,"prompt":"reject"}),
            );
            loop {
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":child}),
                );
                if result["session"]["turn"]["status"] == "failed" {
                    assert_eq!(result["session"]["turn"]["turn_id"], accepted["turn_id"]);
                    assert_eq!(result["session"]["error"], "fixture rejected");
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{result}");
                std::thread::sleep(Duration::from_millis(20));
            }
            // A live child can retain higher permissions after its parent is
            // lowered. Reject new work before creating or submitting a turn.
            let save_mode = |id, mode| {
                let mut state = store.load().unwrap();
                let session = state.sessions.iter_mut().find(|s| s.id == id).unwrap();
                store.hydrate(session).unwrap();
                session.runtime_mode = mode;
                let sessions = vec![session.clone()];
                client
                    .request(
                        Uuid::nil(),
                        Uuid::nil(),
                        Command::SaveTaskState {
                            projects: state.projects,
                            sessions,
                            live_session_ids: vec![parent.id, child],
                        },
                    )
                    .unwrap();
            };
            save_mode(parent.id, RuntimeMode::FullAccess);
            let ResponsePayload::SessionRuntime {
                runtime_id: Some(child_runtime),
                ..
            } = client
                .request(child, Uuid::nil(), Command::AttachSession)
                .unwrap()
            else {
                panic!("child runtime must remain alive");
            };
            client
                .request(
                    child,
                    child_runtime,
                    Command::ApplyOptions {
                        options: crate::WireSessionOptions {
                            mode: "fullAccess".into(),
                            model: None,
                            reasoning_effort: None,
                            service_tier: None,
                            context_window: None,
                        },
                    },
                )
                .unwrap();
            save_mode(child, RuntimeMode::FullAccess);
            save_mode(parent.id, RuntimeMode::Ask);
            let calls_before = std::fs::read_to_string(path.join("prompt-calls.jsonl")).unwrap();
            let denied = mcp_response(
                address,
                token,
                parent.id,
                runtime,
                "waku_prompt",
                json!({"session_id":child,"prompt":"must not execute"}),
            );
            assert_eq!(denied["result"]["isError"], true, "{denied}");
            assert!(denied.to_string().contains("child permissions exceed"));
            let result = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":child}),
            );
            assert_eq!(result["session"]["turn"]["turn_id"], accepted["turn_id"]);
            assert_eq!(
                std::fs::read_to_string(path.join("prompt-calls.jsonl")).unwrap(),
                calls_before
            );
            let cancelled = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_cancel",
                json!({"session_id":child}),
            );
            assert_eq!(cancelled["stopped"], true);
        },
    );
}

#[test]
fn mcp_prompt_save_failure_stops_new_work_before_provider_submission() {
    with_creation_daemon_seed(
        seed_query_sessions,
        |client, _, root, path, address, (project, parent, children)| {
            let (parent, _, config, runtime) =
                start_steward_saved(&client, root, path, parent, project);
            let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let connection = rusqlite::Connection::open(root.join("app.db")).unwrap();
            connection.execute_batch(&format!(
                "CREATE TRIGGER reject_prompt_save BEFORE UPDATE ON sessions WHEN NEW.id = '{}' BEGIN SELECT RAISE(FAIL, 'fixture prompt save failure'); END;",
                children[0].id
            )).unwrap();
            let failure = mcp_response(
                address,
                token,
                parent.id,
                runtime,
                "waku_prompt",
                json!({"session_id":children[0].id,"prompt":"write fixture result"}),
            );
            assert_eq!(failure["result"]["isError"], true, "{failure}");
            assert!(failure.to_string().contains("fixture prompt save failure"));
            let again = mcp_response(
                address,
                token,
                parent.id,
                runtime,
                "waku_prompt",
                json!({"session_id":children[1].id,"prompt":"also must not execute"}),
            );
            assert_eq!(again["result"]["isError"], true, "{again}");
            assert!(again.to_string().contains("new work is disabled"));
            let ResponsePayload::SessionRuntime { runtime_id, .. } = client
                .request(children[0].id, Uuid::nil(), Command::AttachSession)
                .unwrap()
            else {
                panic!("unexpected attachment");
            };
            assert!(runtime_id.is_none());
            let saved = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":children[0].id}),
            );
            assert!(
                saved["session"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("fixture prompt save failure")
            );
            assert!(!path.join("child-calls.jsonl").exists());
            connection
                .execute_batch("DROP TRIGGER reject_prompt_save")
                .unwrap();
        },
    );
}

#[test]
fn mcp_wait_provider_completion_wakes_parent_once_through_server_worker() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let script = include_str!("../tests/fixtures/steward_wait.py");
        std::fs::write(root.join("codex-fixture"), script).unwrap();
        let project = Project::from_path(project_path.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Claude);
        parent.runtime_mode = RuntimeMode::Ask;
        parent.begin_turn("Delegate and wait for the child");
        let (parent, _, config, runtime) = start_steward_saved_with_script(
            &client,
            root,
            project_path,
            parent,
            project,
            Some(script),
        );
        client
            .request(
                parent.id,
                runtime,
                Command::Prompt {
                    prompt: "Delegate and wait for the child".into(),
                    turn_id: parent.active_turn_id(),
                    message_id: parent.messages.last().map(|message| message.id),
                },
            )
            .unwrap();
        let input_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.join("parent-prompt-received").exists() {
            assert!(
                std::time::Instant::now() < input_deadline,
                "parent provider did not receive its initial prompt"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        ]
        .into_iter()
        .map(|message| format!("{message}\n"))
        .collect();
        let output = run_mcp(input, address, token, parent.id, runtime);
        let messages = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            messages[0]["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("waku_wait")
        );
        assert!(
            messages[1]["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "waku_wait")
        );
        let ResponsePayload::SessionCreated { session: child, .. } = client
            .request(parent.id, runtime, spawn_command("controlled wait child"))
            .unwrap()
        else {
            panic!("child missing")
        };
        let result = mcp_tool(
            address,
            token,
            parent.id,
            runtime,
            "waku_wait",
            json!({"session_ids":[child.id]}),
        );
        assert_eq!(result["waiting"], true);
        let callbacks = root.join("callback-prompts.jsonl");
        assert!(
            !callbacks.exists(),
            "registration must not start a new parent turn"
        );
        std::fs::write(root.join("finish-parent"), "").unwrap();
        std::fs::write(root.join("finish-child"), "").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if std::fs::read_to_string(&callbacks).is_ok_and(|text| !text.is_empty()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "server worker did not resume parent"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let callback = std::fs::read_to_string(&callbacks).unwrap();
        assert_eq!(callback.lines().count(), 1);
        assert!(callback.contains(&child.id.to_string()));
        std::fs::write(root.join("repeat-child"), "").unwrap();
        while !root.join("repeat-sent").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Allow the worker to process both the callback result and duplicate child completion.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            std::fs::read_to_string(callbacks).unwrap().lines().count(),
            1
        );
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
        client
            .request(child.id, Uuid::nil(), Command::CloseSession)
            .unwrap();
    });
}
