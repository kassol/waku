use super::*;

#[test]
fn claude_child_runs_and_exposes_its_own_scoped_mcp() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let listed = run_mcp(
            format!(
                "{}\n{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
            ),
            address,
            config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap(),
            parent.id,
            runtime,
        );
        let listed: serde_json::Value =
            serde_json::from_str(String::from_utf8(listed).unwrap().lines().last().unwrap())
                .unwrap();
        assert_eq!(
            listed["result"]["tools"][0]["inputSchema"]["properties"]["provider"]["enum"],
            json!(["claude", "codex"])
        );
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let created = mcp_tool(
            address,
            token,
            parent.id,
            runtime,
            "waku_spawn_session",
            json!({"provider":"claude","prompt":"child task"}),
        );
        let child_id = Uuid::parse_str(created["session_id"].as_str().unwrap()).unwrap();
        let path = PathBuf::from(created["workspace_path"].as_str().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let child = loop {
            let ResponsePayload::Session {
                session: Some(child),
            } = client
                .request(
                    child_id,
                    Uuid::nil(),
                    Command::HydrateSession {
                        session_id: child_id,
                    },
                )
                .unwrap()
            else {
                panic!("child unavailable")
            };
            if child
                .turns
                .last()
                .is_some_and(|turn| turn.status == crate::model::TurnStatus::Completed)
            {
                break child;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(child.provider, ProviderKind::Claude);
        assert_eq!(child.parent_session_id, Some(parent.id));
        assert_eq!(child.runtime_mode, RuntimeMode::Ask);
        assert_eq!(
            child.messages.last().unwrap().content,
            "Claude child finished."
        );
        assert!(path.starts_with(root.join("worktrees")));
        let child_config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.join("mcp-config.json")).unwrap())
                .unwrap();
        let env = &child_config["mcpServers"]["waku"]["env"];
        assert_eq!(env["WAKU_MCP_SESSION"], child_id.to_string());
        assert_ne!(env["WAKU_MCP_TOKEN"], token);
        let child_runtime = Uuid::parse_str(env["WAKU_MCP_RUNTIME"].as_str().unwrap()).unwrap();
        let options = |mode: &str| Command::ApplyOptions {
            options: crate::WireSessionOptions {
                mode: mode.into(),
                model: Some("fixture-model".into()),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
            },
        };
        assert!(matches!(
            client
                .request(child_id, child_runtime, options("ask"))
                .unwrap(),
            ResponsePayload::OptionsApplied { applied: true }
        ));
        assert!(
            client
                .request(child_id, child_runtime, options("fullAccess"))
                .is_err()
        );
        let args: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(path.join("claude-args.json")).unwrap())
                .unwrap();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-mode", "default"])
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

#[test]
fn codex_child_can_delegate_but_ancestors_cannot_query_grandchildren() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let created = mcp_tool(
            address,
            token,
            parent.id,
            runtime,
            "waku_spawn_session",
            json!({"provider":"codex", "prompt":"child task"}),
        );
        let child = Uuid::parse_str(created["session_id"].as_str().unwrap()).unwrap();
        let path = PathBuf::from(created["workspace_path"].as_str().unwrap());
        let args: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(path.join("codex-args.json")).unwrap())
                .unwrap();
        let config_arg = args
            .iter()
            .find(|arg| arg.starts_with("mcp_servers.waku="))
            .expect("Codex must receive a scoped MCP server");
        let config: toml::Value = toml::from_str(config_arg).unwrap();
        let env = &config["mcp_servers"]["waku"]["env"];
        let child_token = env["WAKU_MCP_TOKEN"].as_str().unwrap();
        let child_runtime = Uuid::parse_str(env["WAKU_MCP_RUNTIME"].as_str().unwrap()).unwrap();
        assert_eq!(env["WAKU_MCP_SESSION"].as_str().unwrap(), child.to_string());
        let grandchild = mcp_tool(
            address,
            child_token,
            child,
            child_runtime,
            "waku_spawn_session",
            json!({"provider":"claude", "prompt":"grandchild task"}),
        );
        let grandchild_id = Uuid::parse_str(grandchild["session_id"].as_str().unwrap()).unwrap();
        let direct = mcp_tool(
            address,
            child_token,
            child,
            child_runtime,
            "waku_list_sessions",
            json!({}),
        );
        assert!(direct.to_string().contains(&grandchild_id.to_string()));
        let forbidden = mcp_response(
            address,
            token,
            parent.id,
            runtime,
            "waku_result",
            json!({"session_id":grandchild_id}),
        );
        assert_eq!(forbidden["result"]["isError"], true);
        let grandchild_path = PathBuf::from(grandchild["workspace_path"].as_str().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let grandchild_config: serde_json::Value = loop {
            if let Ok(text) = std::fs::read_to_string(grandchild_path.join("mcp-config.json")) {
                if let Ok(config) = serde_json::from_str(&text) {
                    break config;
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        };
        let env = &grandchild_config["mcpServers"]["waku"]["env"];
        let forbidden = mcp_response(
            address,
            env["WAKU_MCP_TOKEN"].as_str().unwrap(),
            grandchild_id,
            Uuid::parse_str(env["WAKU_MCP_RUNTIME"].as_str().unwrap()).unwrap(),
            "waku_result",
            json!({"session_id":child}),
        );
        assert_eq!(forbidden["result"]["isError"], true);
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

#[test]
fn cross_provider_permissions_reject_unmapped_modes_in_both_directions() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (mut parent, project, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        for (provider, child_provider) in [
            (ProviderKind::Codex, "claude"),
            (ProviderKind::Claude, "codex"),
        ] {
            parent.provider = provider;
            parent.runtime_mode = RuntimeMode::Auto;
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
            let rejected = mcp_response(
                address,
                token,
                parent.id,
                runtime,
                "waku_spawn_session",
                json!({"provider":child_provider,"prompt":"must not start","runtime_mode":"auto"}),
            );
            assert_eq!(rejected["result"]["isError"], true, "{rejected}");
        }
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}

#[test]
fn claude_child_keeps_native_request_pending_without_explicit_decision() {
    with_creation_daemon(|client, _, root, project_path, address| {
        let (parent, _, config, runtime) = start_steward(&client, root, project_path);
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        for (prompt, reason) in [
            ("wait for approval", "permission"),
            ("wait for answer", "userInput"),
        ] {
            let created = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_spawn_session",
                json!({"provider":"claude","prompt":prompt}),
            );
            let child = Uuid::parse_str(created["session_id"].as_str().unwrap()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let status = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_status",
                    json!({"session_ids":[child]}),
                );
                if status["sessions"][0]["status"] == "waiting" {
                    assert_eq!(status["sessions"][0]["waiting_for"], json!(["managerDecision", reason]));
                    assert_eq!(status["sessions"][0]["turn"]["status"], "running");
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{status}");
                std::thread::sleep(Duration::from_millis(20));
            }
            let result = mcp_tool(
                address,
                token,
                parent.id,
                runtime,
                "waku_result",
                json!({"session_id":child}),
            );
            assert_eq!(result["session"]["turn"]["status"], "running");
            let ResponsePayload::Session { session: Some(pending) } = client
                .request(Uuid::nil(), Uuid::nil(), Command::HydrateSession { session_id: child })
                .unwrap()
            else { panic!("pending child unavailable") };
            let expected_id = if reason == "permission" { "approval" } else { "answer" };
            if reason == "permission" {
                assert_eq!(pending.pending_permission.as_ref().unwrap().request_id, expected_id);
            } else {
                assert_eq!(pending.pending_user_input.as_ref().unwrap().request_id, expected_id);
            }
            let native = pending.decision_requests.iter()
                .find_map(|request| request.native.as_ref()).unwrap();
            assert_eq!(native.request.request_id(), expected_id);
            assert!(native.response.is_none());
            assert!(native.outcome.is_none());
            let command = if reason == "permission" {
                Command::Respond {
                    request_id: "approval".into(),
                    option_id: "allow".into(),
                }
            } else {
                Command::RespondUserInput {
                    request_id: "answer".into(),
                    answers: vec![crate::model::UserInputAnswer {
                        question_id: "Continue?".into(),
                        answers: vec!["Yes".into()],
                    }],
                }
            };
            let mut scoped = scoped_socket(address, token);
            assert!(matches!(
                scoped_request(
                    &mut scoped,
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: child,
                        runtime_id: runtime,
                        command: command.clone()
                    }
                ),
                ResponseOutcome::Error { .. }
            ));
            let path = PathBuf::from(created["workspace_path"].as_str().unwrap());
            let child_config: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(path.join("mcp-config.json")).unwrap(),
            )
            .unwrap();
            let child_runtime = Uuid::parse_str(
                child_config["mcpServers"]["waku"]["env"]["WAKU_MCP_RUNTIME"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert!(client.request(child, child_runtime, command).is_err());
            let ResponsePayload::StewardDecisions { requests } = client.request(parent.id, runtime, Command::StewardDecision { operation: crate::model::StewardDecisionOperation::List { session_id: Some(child) } }).unwrap() else { panic!("native decision") };
            let response = if reason == "permission" {
                crate::model::NativeDecisionResponse::Permission { option_id: "allow".into() }
            } else {
                crate::model::NativeDecisionResponse::UserInput { answers: vec![crate::model::UserInputAnswer { question_id:"Continue?".into(),answers:vec!["Yes".into()] }] }
            };
            let ResponsePayload::StewardDecisions { requests: escalated } = client.request(
                parent.id,
                runtime,
                Command::StewardDecision {
                    operation: crate::model::StewardDecisionOperation::Escalate {
                        session_id: child,
                        request_id: requests[0].id,
                        reason: "Fixture requires an explicit user decision".into(),
                        options: vec![crate::model::DecisionOption {
                            label: "Continue".into(),
                            impact: "Resume the pending fixture operation".into(),
                        }],
                        impact: "Resume the pending fixture operation".into(),
                    },
                },
            ).unwrap() else { panic!("native escalation") };
            assert_eq!(escalated[0].state, crate::model::DecisionState::WaitingUser);
            client.request(parent.id,runtime,Command::AnswerNativeDecision {child_session_id:child,request_id:requests[0].id,response}).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let result = mcp_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    "waku_result",
                    json!({"session_id":child}),
                );
                if result["session"]["turn"]["status"] == "completed" {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "{result}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        client
            .request(parent.id, runtime, Command::CloseSession)
            .unwrap();
    });
}
