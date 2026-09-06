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
    std::fs::write(&binary, "#!/usr/bin/env python3\nimport sys,pathlib\npathlib.Path('mcp-config.json').write_text(sys.argv[sys.argv.index('--mcp-config')+1])\nassert '--strict-mcp-config' not in sys.argv\nfor line in sys.stdin: pass\n").unwrap();
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
        let mut output = Vec::new();
        if let Ok(binary) = std::env::var("WAKU_TEST_MCP_BINARY") {
            use std::io::Write;
            let mut child = std::process::Command::new(binary)
                .arg("mcp")
                .env("WAKU_MCP_ADDRESS", address.to_string())
                .env("WAKU_MCP_TOKEN", token)
                .env("WAKU_MCP_SESSION", parent.id.to_string())
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
                parent.id,
                runtime,
            )
            .unwrap();
        }
        let replies: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 6);
        assert_eq!(replies[5]["error"]["code"], -32600);
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 1);
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
