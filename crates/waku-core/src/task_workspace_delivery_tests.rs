//! Fixed-commit acceptance through the scoped MCP transport.
use super::*;

fn commit_all(path: &Path, message: &str) -> String {
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            message,
        ],
    );
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn workspace_tool(
    address: std::net::SocketAddr,
    token: &str,
    parent: Uuid,
    runtime: Uuid,
    operation: serde_json::Value,
) -> serde_json::Value {
    let input = format!(
        "{}\n",
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"waku_workspace", "arguments":{"operation":operation}}})
    );
    let initialize = format!(
        "{}\n",
        json!({"jsonrpc":"2.0", "id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}})
    );
    let output = run_mcp(initialize + &input, address, token, parent, runtime);
    let text = String::from_utf8(output).unwrap();
    let response: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn managed_task_accepts_fixed_child_commit_through_mcp_while_parent_runs() {
    with_creation_daemon(|client, _, root, repository, address| {
        let project = Project::from_path(repository.to_owned());
        let parent = AgentSession::new(project.id, ProviderKind::Claude);
        client
            .request(
                parent.id,
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    sessions: vec![parent.clone()],
                    live_session_ids: vec![parent.id],
                },
            )
            .unwrap();
        let ResponsePayload::TaskWorkspace { session:mut parent } = client.request(parent.id, Uuid::nil(), serde_json::from_value(json!({"type":"stewardWorkspace","operation":{"type":"begin","name":"Deliver parser","targetBranch":"main","expectedCommit":""}})).unwrap()).unwrap() else { panic!("task missing"); };
        let task = parent.managed_workspace.clone().unwrap();
        parent.begin_turn("Coordinate the task");
        let (parent, _, config, runtime) = start_steward_saved(
            &client,
            root,
            &task.coordination.as_ref().unwrap().path,
            parent,
            project,
        );
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let ResponsePayload::SessionCreated { session:child, workspace_path, .. } = client.request(parent.id, runtime, serde_json::from_value(json!({"type":"createSession","provider":"codex","prompt":"write fixture result"})).unwrap()).unwrap() else { panic!("child missing"); };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !workspace_path.join("child-result.txt").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let commit = commit_all(&workspace_path, "Validated child result");
        let blocked = client.request(parent.id, runtime, serde_json::from_value(json!({
            "type":"createSession","provider":"codex","prompt":"must wait for accepted dependency", "dependencies":[{"session_id":child.id,"commit":commit}]
        })).unwrap()).unwrap();
        assert!(
            matches!(blocked, ResponsePayload::SessionCreationFailed { .. }),
            "an unintegrated dependency must not start a provider: {blocked:?}"
        );
        let operation = json!({"type":"integrate","sessionId":child.id,"commit":commit,"expectedIntegrationCommit":task.base_commit,
            "evidence":[{"commit":commit,"checks":"fixture output matches expected","environment":"isolated temporary repository","reviewer":"fixture reviewer"}]});
        let result = workspace_tool(address, token, parent.id, runtime, operation.clone());
        assert_eq!(
            result["session"]["managed_workspace"]["results"][0]["commit"],
            commit
        );
        assert_eq!(
            result["session"]["managed_workspace"]["results"][0]["integration_commit"],
            commit
        );
        assert!(task.path.join("child-result.txt").exists());
        assert!(!repository.join("child-result.txt").exists());
        let repeated = workspace_tool(address, token, parent.id, runtime, operation);
        assert_eq!(result, repeated);
        let ResponsePayload::SessionCreated { session:successor, workspace_path:successor_path, .. } = client.request(parent.id, runtime,
            serde_json::from_value(json!({"type":"createSession","provider":"codex","prompt":"use accepted dependency", "dependencies":[{"session_id":child.id,"commit":commit}]})).unwrap()).unwrap()
        else { panic!("integrated dependency should start its successor"); };
        assert_eq!(
            successor.managed_workspace.as_ref().unwrap().base_commit,
            commit
        );
        assert_eq!(
            serde_json::to_value(&successor).unwrap()["managed_workspace"]["dependencies"][0]["session_id"],
            child.id.to_string()
        );
        assert!(successor_path.join("child-result.txt").exists());
        std::fs::write(
            successor_path.join("second-result.txt"),
            "uses the accepted first result",
        )
        .unwrap();
        let second_commit = commit_all(&successor_path, "Validated dependent result");
        workspace_tool(
            address,
            token,
            parent.id,
            runtime,
            json!({"type":"integrate","sessionId":successor.id,"commit":second_commit,"expectedIntegrationCommit":commit,
            "evidence":[{"commit":second_commit,"checks":"both fixture results present","environment":"isolated temporary repository","reviewer":"fixture reviewer"}]}),
        );
        let delivery = json!({"type":"deliver","commit":second_commit,"expectedTargetCommit":task.target_commit,
            "evidence":[{"commit":second_commit,"checks":"combined fixture acceptance","environment":"isolated temporary repository","reviewer":"independent fixture reviewer"}]});
        std::fs::write(repository.join("untracked-user-work"), "preserve this").unwrap();
        let blocked = client
            .request(
                parent.id,
                runtime,
                serde_json::from_value(json!({"type":"stewardWorkspace", "operation":delivery}))
                    .unwrap(),
            )
            .unwrap();
        assert!(
            serde_json::to_value(blocked).unwrap()["session"]["managed_workspace"]["error"]
                .as_str()
                .unwrap()
                .contains("untracked")
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("untracked-user-work")).unwrap(),
            "preserve this"
        );
        std::fs::remove_file(repository.join("untracked-user-work")).unwrap();
        git(
            repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "Target moved",
            ],
        );
        let moved = client
            .request(
                parent.id,
                runtime,
                serde_json::from_value(json!({"type":"stewardWorkspace", "operation":delivery}))
                    .unwrap(),
            )
            .unwrap();
        assert!(
            serde_json::to_value(&moved).unwrap()["session"]["managed_workspace"]["error"]
                .as_str()
                .unwrap()
                .contains("target moved")
        );
        // Model a saved reference followed by a failed target update. A new accepted
        // commit must retain this earlier reference without replacing it.
        let pending = serde_json::to_value(&moved).unwrap();
        let retained = pending["session"]["managed_workspace"]["deliveries"][0]["reference"]
            .as_str()
            .unwrap();
        git(repository, &["update-ref", retained, &second_commit]);
        let read_head = |path: &Path| {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(path)
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let refreshed_target = read_head(repository);
        git(
            &task.path,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "merge",
                "--no-edit",
                &refreshed_target,
            ],
        );
        let accepted = read_head(&task.path);
        assert_ne!(accepted, second_commit);
        let delivery = json!({"type":"deliver","commit":accepted,
            "expectedTargetCommit":refreshed_target,
            "evidence":[{"commit":accepted,"checks":"combined result includes refreshed target",
            "environment":"isolated temporary repository","reviewer":"independent fixture reviewer"}]});
        let delivered = workspace_tool(address, token, parent.id, runtime, delivery.clone());
        assert_eq!(
            delivered["session"]["managed_workspace"]["deliveries"][1]["completed"],
            true
        );
        assert_eq!(
            delivered["session"]["managed_workspace"]["deliveries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let preserved = std::process::Command::new("git")
            .args(["rev-parse", retained])
            .current_dir(repository)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(preserved.stdout).unwrap().trim(),
            second_commit
        );
        assert!(repository.join("second-result.txt").exists());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while workspace_path.exists() || successor_path.exists() || task.path.exists() {
            if std::time::Instant::now() >= deadline {
                let child_state = workspace_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    json!({"type":"inspect","sessionId":child.id}),
                );
                let next_state = workspace_tool(
                    address,
                    token,
                    parent.id,
                    runtime,
                    json!({"type":"inspect","sessionId":successor.id}),
                );
                panic!(
                    "cleanup missing: root={:?} child={:?} successor={:?}",
                    delivered["session"]["managed_workspace"]["cleanup"],
                    child_state["session"]["managed_workspace"]["cleanup"],
                    next_state["session"]["managed_workspace"]["cleanup"]
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(task.coordination.as_ref().unwrap().path.exists());
        assert_eq!(
            workspace_tool(address, token, parent.id, runtime, delivery),
            delivered
        );
    });
}

#[test]
fn managed_task_conflict_preserves_history_and_retries_after_resolution() {
    with_creation_daemon(|client, _, root, repository, address| {
        let project = Project::from_path(repository.to_owned());
        let parent = AgentSession::new(project.id, ProviderKind::Claude);
        client
            .request(
                parent.id,
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    sessions: vec![parent.clone()],
                    live_session_ids: vec![parent.id],
                },
            )
            .unwrap();
        let ResponsePayload::TaskWorkspace { session: mut parent } = client.request(
            parent.id, Uuid::nil(), serde_json::from_value(json!({"type":"stewardWorkspace",
                "operation":{"type":"begin","name":"Conflict recovery","targetBranch":"main","expectedCommit":""}})).unwrap(),
        ).unwrap() else { panic!("task missing"); };
        let task = parent.managed_workspace.clone().unwrap();
        parent.begin_turn("Coordinate conflict recovery");
        let (parent, _, config, runtime) = start_steward_saved(
            &client,
            root,
            &task.coordination.as_ref().unwrap().path,
            parent,
            project,
        );
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        let ResponsePayload::SessionCreated {
            session: child,
            workspace_path,
            ..
        } = client
            .request(
                parent.id,
                runtime,
                serde_json::from_value(json!({"type":"createSession",
                "provider":"codex","prompt":"write fixture result"}))
                .unwrap(),
            )
            .unwrap()
        else {
            panic!("child missing");
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !workspace_path.join("child-result.txt").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(workspace_path.join("README.md"), "child change\n").unwrap();
        let commit = commit_all(&workspace_path, "Child edit");
        std::fs::write(task.path.join("README.md"), "integration change\n").unwrap();
        let expected = commit_all(&task.path, "Concurrent integration edit");
        let mut operation = json!({"type":"integrate","sessionId":child.id,"commit":commit,
            "expectedIntegrationCommit":expected,"evidence":[{"commit":commit,
            "checks":"child output verified","environment":"temporary Git repository","reviewer":"fixture reviewer"}]});
        let failed = client
            .request(
                parent.id,
                runtime,
                serde_json::from_value(json!({"type":"stewardWorkspace","operation":operation}))
                    .unwrap(),
            )
            .unwrap();
        let failed = serde_json::to_value(failed).unwrap();
        assert!(
            failed["session"]["managed_workspace"]["error"]
                .as_str()
                .unwrap()
                .contains("CONFLICT")
        );
        assert!(
            std::fs::read_to_string(task.path.join("README.md"))
                .unwrap()
                .contains("<<<<<<<")
        );
        assert!(
            failed["session"]["managed_workspace"]["results"][0]["integration_commit"].is_null()
        );
        let store = StateStore::daemon(root.join("app.db"));
        let loaded = store.load().unwrap();
        let mut restored = loaded
            .sessions
            .into_iter()
            .find(|session| session.id == child.id)
            .unwrap();
        store.hydrate(&mut restored).unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap()["managed_workspace"],
            failed["session"]["managed_workspace"]
        );
        std::fs::write(
            task.path.join("README.md"),
            "reviewed resolution keeps both changes\n",
        )
        .unwrap();
        let resolved = commit_all(&task.path, "Resolve preserved conflict");
        operation["expectedIntegrationCommit"] = json!(resolved);
        let integrated = workspace_tool(address, token, parent.id, runtime, operation.clone());
        assert_eq!(
            integrated["session"]["managed_workspace"]["results"][0]["integration_commit"],
            resolved
        );
        assert_eq!(
            workspace_tool(address, token, parent.id, runtime, operation),
            integrated
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("README.md")).unwrap(),
            "fixture\n"
        );
    });
}

fn with_managed_steward(
    test: impl FnOnce(
        DaemonClient,
        &Path,
        &Path,
        std::net::SocketAddr,
        AgentSession,
        Project,
        serde_json::Value,
        Uuid,
    ),
) {
    with_creation_daemon(|client, _, root, repository, address| {
        let project = Project::from_path(repository.to_owned());
        let parent = AgentSession::new(project.id, ProviderKind::Claude);
        client
            .request(
                parent.id,
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    sessions: vec![parent.clone()],
                    live_session_ids: vec![parent.id],
                },
            )
            .unwrap();
        let ResponsePayload::TaskWorkspace {
            session: mut parent,
        } = client
            .request(
                parent.id,
                Uuid::nil(),
                serde_json::from_value(
                    json!({"type":"stewardWorkspace","operation":{"type":"begin",
                "name":"Recursive task","targetBranch":"main","expectedCommit":""}}),
                )
                .unwrap(),
            )
            .unwrap()
        else {
            panic!("task missing");
        };
        let coordination = parent
            .managed_workspace
            .as_ref()
            .unwrap()
            .coordination
            .as_ref()
            .unwrap()
            .path
            .clone();
        parent.begin_turn("Coordinate the task");
        let (parent, project, config, runtime) =
            start_steward_saved(&client, root, &coordination, parent, project);
        test(
            client, root, repository, address, parent, project, config, runtime,
        );
    });
}

#[test]
fn managed_task_rejects_new_children_after_delivery_with_retained_integration() {
    with_managed_steward(|client, _, _, address, parent, project, config, runtime| {
        let task = parent.managed_workspace.as_ref().unwrap();
        let oversized = vec![json!({"session_id":parent.id,"commit":task.base_commit}); 129];
        let rejected = client.request(parent.id, runtime, serde_json::from_value(json!({
            "type":"createSession","provider":"codex","prompt":"too many dependencies", "dependencies":oversized
        })).unwrap()).unwrap_err();
        assert!(rejected.to_string().contains("128"));

        let mut reference = AgentSession::new(project.id, ProviderKind::Codex);
        reference.begin_turn("Keep integration reference");
        reference.finish_active_turn(crate::model::TurnStatus::Completed);
        reference.workspace = crate::model::SessionWorkspace::Worktree {
            path: task.path.clone(),
            branch: task.branch.clone(),
        };
        client
            .request(
                parent.id,
                runtime,
                Command::SaveTaskState {
                    projects: vec![project],
                    sessions: vec![parent.clone(), reference.clone()],
                    live_session_ids: vec![parent.id, reference.id],
                },
            )
            .unwrap();
        let token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
            .as_str()
            .unwrap();
        workspace_tool(
            address,
            token,
            parent.id,
            runtime,
            json!({"type":"deliver","commit":task.base_commit,
            "expectedTargetCommit":task.base_commit,"evidence":[{"commit":task.base_commit,
            "checks":"verified","environment":"fixture","reviewer":"fixture reviewer"}]}),
        );
        assert!(task.path.exists());
        let response = client.request(parent.id, runtime, serde_json::from_value(json!({
            "type":"createSession","provider":"codex","prompt":"must not create after delivery"
        })).unwrap()).unwrap();
        let ResponsePayload::SessionCreationFailed {
            error,
            workspace_path,
            ..
        } = response
        else {
            panic!("delivered tasks must reject a new child: {response:?}");
        };
        assert!(error.contains("delivered"), "{error}");
        assert!(workspace_path.is_none());
    });
}

#[test]
fn managed_task_recursively_integrates_only_direct_results_without_moving_runtime() {
    with_managed_steward(
        |client, root, repository, address, parent, _, config, runtime| {
            let root_task = parent.managed_workspace.as_ref().unwrap();
            let root_token = config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap();
            let ResponsePayload::SessionCreated {
                session: manager,
                workspace_path: execution,
                ..
            } = client
                .request(
                    parent.id,
                    runtime,
                    serde_json::from_value(json!({"type":"createSession","provider":"claude",
                "prompt":"Coordinate nested work"}))
                    .unwrap(),
                )
                .unwrap()
            else {
                panic!("manager missing");
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let nested_config: serde_json::Value = loop {
                if let Ok(text) = std::fs::read_to_string(execution.join("mcp-config.json")) {
                    if let Ok(config) = serde_json::from_str(&text) {
                        break config;
                    }
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            };
            let env = &nested_config["mcpServers"]["waku"]["env"];
            let nested_token = env["WAKU_MCP_TOKEN"].as_str().unwrap();
            let nested_runtime =
                Uuid::parse_str(env["WAKU_MCP_RUNTIME"].as_str().unwrap()).unwrap();
            let seed = commit_all(&execution, "Committed manager fixture inputs");
            let reserved = root
                .join("task-worktrees")
                .join(format!("{}-integration", manager.id));
            std::fs::create_dir_all(&reserved).unwrap();
            std::fs::write(reserved.join("user-file"), "preserve existing directory").unwrap();
            let rejected = client.request(manager.id, nested_runtime, serde_json::from_value(json!({
                "type":"createSession","provider":"codex","prompt":"must preserve existing integration directory"
            })).unwrap()).unwrap();
            assert!(matches!(
                rejected,
                ResponsePayload::SessionCreationFailed { .. }
            ));
            assert_eq!(
                std::fs::read_to_string(reserved.join("user-file")).unwrap(),
                "preserve existing directory"
            );
            let unchanged = workspace_tool(
                address,
                nested_token,
                manager.id,
                nested_runtime,
                json!({"type":"inspect","sessionId":manager.id}),
            );
            assert!(unchanged["session"]["managed_workspace"]["coordination"].is_null());
            std::fs::remove_dir_all(&reserved).unwrap();

            let ResponsePayload::SessionCreated {
                session: child,
                workspace_path: child_path,
                ..
            } = client
                .request(
                    manager.id,
                    nested_runtime,
                    serde_json::from_value(json!({"type":"createSession","provider":"codex",
                "prompt":"write fixture result"}))
                    .unwrap(),
                )
                .unwrap()
            else {
                panic!("nested child missing");
            };
            let promoted = workspace_tool(
                address,
                nested_token,
                manager.id,
                nested_runtime,
                json!({"type":"inspect","sessionId":manager.id}),
            );
            let promoted: AgentSession =
                serde_json::from_value(promoted["session"].clone()).unwrap();
            let task = promoted.managed_workspace.as_ref().unwrap();
            assert_eq!(
                task.coordination
                    .as_ref()
                    .expect("nested manager needs its own integration directory")
                    .path,
                execution
            );
            assert_eq!(promoted.workspace, manager.workspace);
            assert_ne!(task.path, execution);
            assert_eq!(child.managed_workspace.as_ref().unwrap().base_commit, seed);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !child_path.join("child-result.txt").exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let child_commit = commit_all(&child_path, "Nested accepted result");
            let accept = json!({"type":"integrate","sessionId":child.id,"commit":child_commit,
            "expectedIntegrationCommit":seed,"evidence":[{"commit":child_commit,"checks":"verified nested output",
            "environment":"temporary repository","reviewer":"nested reviewer"}]});
            assert!(
                client
                    .request(
                        parent.id,
                        runtime,
                        serde_json::from_value(json!({
                            "type":"stewardWorkspace","operation":accept
                        }))
                        .unwrap()
                    )
                    .is_err(),
                "root must not integrate a grandchild directly"
            );
            workspace_tool(address, nested_token, manager.id, nested_runtime, accept);
            let ResponsePayload::SessionCreated { session: successor, workspace_path: successor_path, .. } = client.request(
            manager.id, nested_runtime, serde_json::from_value(json!({"type":"createSession","provider":"codex",
                "prompt":"use nested result","dependencies":[{"session_id":child.id,"commit":child_commit}]})).unwrap(),
        ).unwrap() else { panic!("nested successor missing"); };
            assert_eq!(
                successor.managed_workspace.as_ref().unwrap().base_commit,
                child_commit
            );
            assert!(successor_path.join("child-result.txt").exists());
            let next = commit_all(&successor_path, "Accepted successor fixture inputs");
            workspace_tool(
                address,
                nested_token,
                manager.id,
                nested_runtime,
                json!({"type":"integrate","sessionId":successor.id,
            "commit":next,"expectedIntegrationCommit":child_commit,"evidence":[{"commit":next,"checks":"successor validated",
            "environment":"fixture","reviewer":"nested reviewer"}]}),
            );
            workspace_tool(
                address,
                root_token,
                parent.id,
                runtime,
                json!({"type":"integrate","sessionId":manager.id,
            "commit":next,"expectedIntegrationCommit":root_task.base_commit,"evidence":[{"commit":next,
            "checks":"nested combined result validated","environment":"fixture","reviewer":"root reviewer"}]}),
            );
            workspace_tool(
                address,
                root_token,
                parent.id,
                runtime,
                json!({"type":"deliver","commit":next,
            "expectedTargetCommit":root_task.base_commit,"evidence":[{"commit":next,"checks":"whole task validated",
            "environment":"fixture","reviewer":"overall reviewer"}]}),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while task.path.exists()
                || execution.exists()
                || child_path.exists()
                || successor_path.exists()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "nested delivered resources should clean"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(repository.join("child-result.txt").exists());
        },
    );
}
