//! A controlled complete task through MCP/socket, real temporary Git and SQLite.
use super::*;
use std::collections::HashSet;

struct CoordinationTools {
    address: std::net::SocketAddr,
    token: String,
    parent: Uuid,
    runtime: Uuid,
    calls: Vec<String>,
    seen: HashSet<String>,
    repeated_results: usize,
    last_status: Option<serde_json::Value>,
    unchanged_queries: usize,
}
impl CoordinationTools {
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.calls.push(name.into());
        let result = mcp_tool(
            self.address,
            &self.token,
            self.parent,
            self.runtime,
            name,
            arguments,
        );
        if name == "waku_results" {
            for row in result["results"].as_array().unwrap() {
                if row["handled"] == false
                    && !row["receipt"].is_null()
                    && !self.seen.insert(row["receipt"].to_string())
                {
                    self.repeated_results += 1;
                }
            }
        }
        if name == "waku_result" {
            let version = json!({"session":result["session"]["session_id"],"turn":result["session"]["turn"],"reply":result["reply"],"error":result["session"]["error"],"waiting":result["session"]["waiting_for"]});
            if !self.seen.insert(version.to_string()) {
                self.repeated_results += 1;
            }
        }
        if name == "waku_status" {
            let states = result["sessions"].as_array().unwrap().iter().map(|session| json!({"session":session["session_id"],"turn":session["turn"],"error":session["error"],"waiting":session["waiting_for"]})).collect::<Vec<_>>();
            let snapshot = json!(states);
            if self.last_status.as_ref() == Some(&snapshot) {
                self.unchanged_queries += 1;
            }
            self.last_status = Some(snapshot);
        }
        result
    }
    fn spawn(
        &mut self,
        assignment: serde_json::Value,
        dependencies: serde_json::Value,
    ) -> (Uuid, PathBuf) {
        let child = self.call("waku_spawn_session", json!({"provider":"codex", "title":assignment["owner"], "prompt":assignment.to_string(), "dependencies":dependencies}));
        (
            Uuid::parse_str(child["session_id"].as_str().unwrap()).unwrap(),
            PathBuf::from(child["workspace_path"].as_str().unwrap()),
        )
    }
}

fn settled(client: &DaemonClient, id: Uuid, ready: impl Fn(&AgentSession) -> bool) -> AgentSession {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
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
            panic!("missing task");
        };
        if ready(&session) {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task did not reach fixture barrier: {id}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn fixture_result(root: &Path, path: &Path) -> serde_json::Value {
    let file = root.join(format!(
        "{}.ready",
        path.file_name().unwrap().to_str().unwrap()
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(result) = serde_json::from_str(&text) {
                return result;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not reach {file:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn release(root: &Path, path: &Path) {
    std::fs::write(
        root.join(format!(
            "{}.release",
            path.file_name().unwrap().to_str().unwrap()
        )),
        "",
    )
    .unwrap();
}
fn completed(session: &AgentSession) -> bool {
    session
        .turns
        .last()
        .is_some_and(|turn| turn.status == crate::model::TurnStatus::Completed)
}

#[test]
fn mcp_coordination_batches_reviews_feedback_wait_and_dependency_evidence() {
    let current = run_coordination_strategy(false);
    let repetitive = run_coordination_strategy(true);
    for metric in [
        "unchanged_queries",
        "repeated_result_reads",
        "avoidable_repeated_validations",
    ] {
        assert!(
            current[metric].as_u64().unwrap() < repetitive[metric].as_u64().unwrap(),
            "strategy contrast did not measure a difference in {metric}"
        );
    }
    // This controlled comparison exercises query/read/check policy changes. Both
    // send feedback with native steer; it makes no cancellation reduction claim.
    assert_eq!(current["native_cancels"], repetitive["native_cancels"]);
}

fn run_coordination_strategy(repetitive: bool) -> serde_json::Value {
    let mut measured = serde_json::Value::Null;
    with_creation_daemon(|client, _, root, repository, address| {
        let started = std::time::Instant::now();
        std::fs::write(
            root.join("codex-fixture"),
            include_str!("../tests/fixtures/steward_coordination.py"),
        )
        .unwrap();
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
        let ResponsePayload::TaskWorkspace {session:mut parent} = client.request(parent.id, Uuid::nil(), serde_json::from_value(json!({"type":"stewardWorkspace","operation":{"type":"begin","name":"Controlled coordination","targetBranch":"main","expectedCommit":""}})).unwrap()).unwrap() else { panic!("missing task"); };
        let task = parent.managed_workspace.clone().unwrap();
        parent.begin_turn("Owners: implementer feature.txt, reviewer fixed commits, dependent dependent.txt. Accept exact contents and combined task.");
        let (parent, _, config, runtime) = start_steward_saved_with_script(
            &client,
            root,
            &task.coordination.as_ref().unwrap().path,
            parent,
            project,
            Some(include_str!("../tests/fixtures/steward_wait.py")),
        );
        client
            .request(
                parent.id,
                runtime,
                Command::Prompt {
                    prompt: parent.messages.last().unwrap().content.clone(),
                    turn_id: parent.active_turn_id(),
                    message_id: parent.messages.last().map(|m| m.id),
                },
            )
            .unwrap();
        let mut tools = CoordinationTools {
            address,
            token: config["mcpServers"]["waku"]["env"]["WAKU_MCP_TOKEN"]
                .as_str()
                .unwrap()
                .into(),
            parent: parent.id,
            runtime,
            calls: vec![],
            seen: HashSet::new(),
            repeated_results: 0,
            last_status: None,
            unchanged_queries: 0,
        };
        let assignment = json!({"owner":"implementer","scope":"feature.txt","baseline":task.base_commit,"acceptance":"feature.txt contains accepted; independent fixed-commit review"});
        let (implementer, implementation_path) = tools.spawn(assignment, json!([]));
        let initial = fixture_result(root, &implementation_path);
        let (reviewer, review_path) = tools.spawn(json!({"owner":"reviewer","scope":"feature.txt","baseline":task.base_commit,"commit":initial["commit"],"expected":"initial","acceptance":"review exact commit","findings":["replace initial with accepted"]}), json!([]));
        fixture_result(root, &review_path);
        release(root, &review_path);
        settled(&client, reviewer, completed);
        let first = tools.call(
            "waku_results",
            json!({"session_ids":[implementer,reviewer]}),
        );
        assert!(first["results"][0]["receipt"].is_null());
        let mut reviewed = first["results"][1]["receipt"].clone();
        assert!(
            first["results"][1]["reply"]
                .as_str()
                .unwrap()
                .contains("replace initial with accepted")
        );
        if repetitive {
            // Replay the old coordination habits through real tools against the
            // identical fixed task, without pretending this is an old binary.
            tools.call("waku_status", json!({"session_ids":[implementer,reviewer]}));
            tools.call("waku_status", json!({"session_ids":[implementer,reviewer]}));
            tools.call("waku_result", json!({"session_id":reviewer}));
            tools.call("waku_result", json!({"session_id":reviewer}));
            tools.call("waku_prompt", json!({"session_id":reviewer,"delivery_id":Uuid::new_v4(),"prompt":json!({"owner":"reviewer","scope":"feature.txt","baseline":task.base_commit,"commit":initial["commit"],"expected":"initial","acceptance":"repeat the already completed identical check","findings":["replace initial with accepted"]}).to_string()}));
            settled(&client, reviewer, |session| {
                session.turns.len() == 2 && completed(session)
            });
            let repeated = tools.call(
                "waku_results",
                json!({"session_ids":[reviewer],"handled":[reviewed]}),
            );
            reviewed = repeated["results"][0]["receipt"].clone();
        }
        let delivery_id = Uuid::new_v4();
        tools.call("waku_prompt", json!({"session_id":implementer,"delivery_id":delivery_id,"prompt":json!({"commit":initial["commit"],"findings":["replace initial with accepted"],"acceptance":"accepted content and fixed-commit review"}).to_string()}));
        settled(&client, implementer, |s| {
            s.input_deliveries.iter().any(|d| {
                d.id == delivery_id && d.state == crate::model::InputDeliveryState::Received
            })
        });
        let delivery = tools.call(
            "waku_prompt_status",
            json!({"session_id":implementer,"delivery_id":delivery_id}),
        );
        assert_eq!(delivery["delivery"]["mode"], "steer");
        let fixed = fixture_result(root, &implementation_path);
        assert_ne!(fixed["commit"], initial["commit"]);
        let waiting = tools.call("waku_wait", json!({"session_ids":[implementer]}));
        assert_eq!(waiting["waiting"], true);
        std::fs::write(root.join("finish-parent"), "").unwrap();
        let asleep = settled(&client, parent.id, |s| s.is_waiting_for_children());
        assert!(asleep.steward_wait.is_some());
        release(root, &implementation_path);
        settled(&client, parent.id, |s| s.turns.len() == 2 && completed(s));
        let callback_text = std::fs::read_to_string(root.join("callback-prompts.jsonl")).unwrap();
        assert_eq!(callback_text.lines().count(), 1);
        let second = tools.call(
            "waku_results",
            json!({"session_ids":[implementer,reviewer],"handled":[reviewed]}),
        );
        assert_eq!(second["results"][1]["handled"], true);
        let implementation_receipt = second["results"][0]["receipt"].clone();
        let (fixed_reviewer, fixed_review_path) = tools.spawn(json!({"owner":"reviewer","scope":"feature.txt","baseline":task.base_commit,"commit":fixed["commit"],"expected":"accepted","acceptance":"independent review of changed fixed commit"}), json!([]));
        fixture_result(root, &fixed_review_path);
        release(root, &fixed_review_path);
        settled(&client, fixed_reviewer, completed);
        let review = tools.call("waku_results", json!({"session_ids":[fixed_reviewer]}));
        let review_receipt = review["results"][0]["receipt"].clone();
        let accepted = tools.call("waku_workspace", json!({"operation":{"type":"integrate","sessionId":implementer,"commit":fixed["commit"],"expectedIntegrationCommit":task.base_commit,"evidence":[{"commit":fixed["commit"],"checks":"feature.txt equals accepted","environment":"controlled-git-v1","reviewer":fixed_reviewer.to_string()}]}}));
        assert_eq!(
            accepted["session"]["managed_workspace"]["results"][0]["owner"],
            implementer.to_string()
        );
        let baseline =
            accepted["session"]["managed_workspace"]["results"][0]["integration_commit"].clone();
        let (dependent, dependent_path) = tools.spawn(json!({"owner":"dependent","scope":"dependent.txt","baseline":baseline,"acceptance":"dependent.txt contains dependent result; reuse accepted feature evidence only at its recorded commit"}), json!([{"session_id":implementer,"commit":fixed["commit"]}]));
        let dependent_result = fixture_result(root, &dependent_path);
        release(root, &dependent_path);
        let dependent_session = settled(&client, dependent, completed);
        assert_eq!(
            dependent_session
                .managed_workspace
                .as_ref()
                .unwrap()
                .dependencies[0]
                .commit,
            fixed["commit"].as_str().unwrap()
        );
        let third = tools.call("waku_results", json!({"session_ids":[implementer,fixed_reviewer,dependent],"handled":[implementation_receipt,review_receipt]}));
        assert_eq!(
            third["results"][0]["handled"], false,
            "new integration evidence invalidates the pre-acceptance receipt"
        );
        assert_eq!(
            third["results"][0]["workspace"]["results"][0]["evidence"][0]["commit"],
            fixed["commit"]
        );
        assert_eq!(third["results"][1]["handled"], true);
        assert_eq!(third["results"][2]["workspace"]["base_commit"], baseline);
        let accepted = tools.call("waku_workspace", json!({"operation":{"type":"integrate","sessionId":dependent,"commit":dependent_result["commit"],"expectedIntegrationCommit":baseline,"evidence":[{"commit":dependent_result["commit"],"checks":"dependent.txt equals dependent result","environment":"controlled-git-v1","reviewer":"controlled dependency check"}]}}));
        assert_eq!(
            accepted["session"]["managed_workspace"]["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            accepted["session"]["managed_workspace"]["results"][0]["owner"],
            dependent.to_string()
        );
        // Combined acceptance checks both outputs at the final commit. Earlier evidence
        // is retained with its original commit and does not replace this acceptance.
        assert_eq!(
            std::fs::read_to_string(task.path.join("feature.txt")).unwrap(),
            "accepted\n"
        );
        assert_eq!(
            std::fs::read_to_string(task.path.join("dependent.txt")).unwrap(),
            "dependent result\n"
        );
        let delivered = tools.call("waku_workspace", json!({"operation":{"type":"deliver","commit":dependent_result["commit"],"expectedTargetCommit":task.target_commit,"evidence":[{"commit":dependent_result["commit"],"checks":"combined feature.txt and dependent.txt exact contents","environment":"controlled-git-v1","reviewer":"controlled independent overall acceptance"}]}}));
        assert_eq!(
            delivered["session"]["managed_workspace"]["deliveries"][0]["completed"],
            true
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("feature.txt")).unwrap(),
            "accepted\n"
        );
        assert_eq!(tools.repeated_results > 0, repetitive);
        let unchanged_queries = tools.unchanged_queries;
        let feedback_cancels = tools
            .calls
            .iter()
            .filter(|name| name.as_str() == "waku_cancel")
            .count();
        let mut checks = Vec::new();
        let mut native_steers = 0;
        let mut native_cancels = 0;
        for path in [
            &implementation_path,
            &review_path,
            &fixed_review_path,
            &dependent_path,
        ] {
            let log = std::fs::read_to_string(root.join(format!(
                "{}.calls",
                path.file_name().unwrap().to_str().unwrap()
            )))
            .unwrap();
            for line in log.lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                native_steers += usize::from(value["method"] == "turn/steer");
                native_cancels += usize::from(value["method"] == "turn/interrupt");
                if value.get("check").is_some() {
                    checks.push(value);
                }
            }
        }
        assert_eq!(native_steers, 1);
        assert_eq!(native_cancels, 0);
        let unique_checks = checks
            .iter()
            .map(|check| check.to_string())
            .collect::<HashSet<_>>()
            .len();
        let repeated_checks = checks.len() - unique_checks;
        assert_eq!(
            repeated_checks,
            1 + usize::from(repetitive),
            "the implementation check is independently repeated by the fixed reviewer: {checks:?}"
        );
        let metrics = json!({"strategy":if repetitive {"repeated-query-read-check-policy"} else {"event-batch-policy"},"fixture_elapsed_ms":started.elapsed().as_millis(),"unchanged_queries":unchanged_queries,"repeated_result_reads":tools.repeated_results,"feedback_cancels":feedback_cancels,"native_steers":native_steers,"native_cancels":native_cancels,"validation_executions":checks.len(),"same_commit_scope_environment_rechecks":repeated_checks,"necessary_independent_review_rechecks":1,"avoidable_repeated_validations":repeated_checks.saturating_sub(1),"combined_acceptance_checks":2,"callbacks":callback_text.lines().count()});
        println!("coordination_metrics={metrics}");
        measured = metrics;
        for id in [parent.id, implementer, reviewer, fixed_reviewer, dependent] {
            client
                .request(id, Uuid::nil(), Command::CloseSession)
                .unwrap();
        }
        // No explicit cleanup command: completion and runtime close events wake
        // the existing worker. Only accepted clean child resources may disappear.
        for id in [implementer, dependent] {
            let removed = settled(&client, id, |session| {
                session.managed_workspace.as_ref().is_some_and(|workspace| {
                    workspace
                        .cleanup
                        .iter()
                        .any(|item| item.status == crate::model::WorkspaceCleanupStatus::Removed)
                })
            });
            let workspace = removed.managed_workspace.as_ref().unwrap();
            assert!(!workspace.path.exists());
            assert_eq!(workspace.results[0].owner, id);
            assert!(workspace.results[0].integration_commit.is_some());
            assert!(!workspace.results[0].evidence.is_empty());
        }
        let parent_after_cleanup = settled(&client, parent.id, |session| {
            session.managed_workspace.as_ref().is_some_and(|workspace| {
                workspace.cleanup.len() == 2
                    && workspace.cleanup.iter().all(|item| {
                        matches!(
                            item.status,
                            crate::model::WorkspaceCleanupStatus::Removed
                                | crate::model::WorkspaceCleanupStatus::Retained
                        )
                    })
            })
        });
        let workspace = parent_after_cleanup.managed_workspace.as_ref().unwrap();
        assert!(workspace.cleanup.iter().any(|item| item.path == task.path
            && item.status == crate::model::WorkspaceCleanupStatus::Removed));
        // The fixture's MCP configuration is an untracked coordination file.
        // Automatic cleanup must retain it, just as it retains user files.
        assert!(
            workspace
                .cleanup
                .iter()
                .any(|item| item.path == task.coordination.as_ref().unwrap().path
                    && item.status == crate::model::WorkspaceCleanupStatus::Retained)
        );
        assert!(
            task.coordination
                .as_ref()
                .unwrap()
                .path
                .join("mcp-config.json")
                .exists()
        );
        let reference = workspace.deliveries[0].reference.clone();
        let fixed_delivery = workspace.deliveries[0].commit.clone();
        let ref_output = std::process::Command::new("git")
            .args(["rev-parse", "--verify", &reference])
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(ref_output.status.success());
        assert_eq!(
            String::from_utf8(ref_output.stdout).unwrap().trim(),
            fixed_delivery
        );
        assert!(matches!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown)
                .unwrap(),
            ResponsePayload::Ack
        ));
        let ids = [parent.id, implementer, reviewer, fixed_reviewer, dependent];
        let history = |session: &AgentSession| {
            json!({
                "id":session.id, "parent":session.parent_session_id,
                "messages":session.messages, "turns":session.turns,
                "activities":session.transcript_blocks,
                "deliveries":session.input_deliveries, "workspace":session.managed_workspace,
                "permission":session.pending_permission, "question":session.pending_user_input,
            })
        };
        let snapshots = ids
            .into_iter()
            .map(|id| history(&settled(&client, id, |_| true)))
            .collect::<Vec<_>>();
        assert!(matches!(
            client
                .request(Uuid::nil(), Uuid::nil(), Command::ShutdownDaemon)
                .unwrap(),
            ResponsePayload::Ack
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !client.is_disconnected() {
            assert!(
                std::time::Instant::now() < deadline,
                "old daemon did not close its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        with_reopened_creation_daemon(root, |reopened| {
            for (id, expected) in ids.into_iter().zip(snapshots) {
                let restored = settled(&reopened, id, |_| true);
                assert_eq!(
                    history(&restored),
                    expected,
                    "cleanup/restart changed saved history for {id}"
                );
            }
            let retained = std::process::Command::new("git")
                .args(["rev-parse", "--verify", &reference])
                .current_dir(repository)
                .output()
                .unwrap();
            assert!(retained.status.success());
            assert_eq!(
                String::from_utf8(retained.stdout).unwrap().trim(),
                fixed_delivery
            );
            assert!(!implementation_path.exists());
            assert!(!dependent_path.exists());
        });
        println!(
            "coordination_cleanup_restart=passed accepted_children_removed=2 history_snapshots_equal=5 retained_delivery_ref=true"
        );
    });
    measured
}
