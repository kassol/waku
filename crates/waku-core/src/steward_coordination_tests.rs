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
        let reviewed = first["results"][1]["receipt"].clone();
        assert!(
            first["results"][1]["reply"]
                .as_str()
                .unwrap()
                .contains("replace initial with accepted")
        );
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
        assert_eq!(tools.repeated_results, 0);
        let unchanged_queries = tools
            .calls
            .iter()
            .filter(|name| name.as_str() == "waku_status")
            .count();
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
            repeated_checks, 1,
            "the implementation check is independently repeated by the fixed reviewer: {checks:?}"
        );
        println!(
            "coordination_metrics={}",
            json!({"fixture_elapsed_ms":started.elapsed().as_millis(),"unchanged_queries":unchanged_queries,"repeated_result_reads":tools.repeated_results,"feedback_cancels":feedback_cancels,"native_steers":native_steers,"native_cancels":native_cancels,"validation_executions":checks.len(),"same_commit_scope_environment_rechecks":repeated_checks,"necessary_independent_review_rechecks":1,"avoidable_repeated_validations":repeated_checks.saturating_sub(1),"combined_acceptance_checks":2,"callbacks":callback_text.lines().count()})
        );
        for id in [parent.id, implementer, reviewer, fixed_reviewer, dependent] {
            client
                .request(id, Uuid::nil(), Command::CloseSession)
                .unwrap();
        }
    });
}
