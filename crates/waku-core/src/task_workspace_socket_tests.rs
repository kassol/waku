//! Managed workspaces through the socket and the controlled Codex transport.
use super::*;

#[test]
fn managed_children_use_integration_commit_and_reject_shared_runtime_aliases() {
    with_creation_daemon(|client, _, root, repository, _| {
        let project = Project::from_path(repository.to_owned());
        let mut parent = AgentSession::new(project.id, ProviderKind::Codex);
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
            session: managed_parent,
        } = client
            .request(
                parent.id,
                Uuid::nil(),
                serde_json::from_value(json!({"type":"stewardWorkspace", "operation": {
                    "type":"begin", "name":"Parser", "targetBranch":"main", "expectedCommit":""
                }}))
                .unwrap(),
            )
            .unwrap()
        else {
            panic!("missing task");
        };
        let task = managed_parent.managed_workspace.as_ref().unwrap();
        std::fs::write(task.path.join("integration-only"), "required baseline").unwrap();
        git(&task.path, &["add", "integration-only"]);
        git(
            &task.path,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Integration baseline",
            ],
        );
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&task.path)
            .output()
            .unwrap();
        let expected = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        parent.begin_turn("Delegate the managed task");
        // A stale desktop save cannot remove the daemon's ownership or change cwd.
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
        let ResponsePayload::SessionCreated { session: child, runtime_id, workspace_path, .. } = client.request(parent.id, Uuid::nil(),
            serde_json::from_value(json!({"type":"createSession", "provider":"codex", "prompt":"write fixture result"})).unwrap()).unwrap()
        else { panic!("missing managed child"); };
        let resource = child.managed_workspace.as_ref().unwrap();
        assert_eq!(resource.task_id, parent.id);
        assert_eq!(resource.base_commit, expected);
        assert!(resource.owned);
        assert_eq!(
            std::fs::read_to_string(workspace_path.join("integration-only")).unwrap(),
            "required baseline"
        );
        let alias = root.join("child-alias");
        std::os::unix::fs::symlink(&workspace_path, &alias).unwrap();
        let mut other = AgentSession::new(project.id, ProviderKind::Codex);
        other.workspace = crate::model::SessionWorkspace::Worktree {
            path: alias.clone(),
            branch: resource.branch.clone(),
        };
        client
            .request(
                other.id,
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    sessions: vec![other.clone()],
                    live_session_ids: vec![parent.id, child.id, other.id],
                },
            )
            .unwrap();
        let start = Command::Start {
            options: crate::WireDriverStartOptions {
                provider: "codex".into(),
                binary: root.join("codex-fixture"),
                cwd: alias,
                mode: "ask".into(),
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
        };
        let mut reserved_start = start.clone();
        if let Command::Start { options } = &mut reserved_start {
            options.cwd = task.path.clone();
        }
        let reserved_error = client
            .request(other.id, Uuid::new_v4(), reserved_start)
            .unwrap_err()
            .to_string();
        assert!(
            reserved_error.contains("reserved for daemon"),
            "{reserved_error}"
        );
        let error = client
            .request(other.id, Uuid::new_v4(), start.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("already owned"), "{error}");
        client
            .request(child.id, runtime_id, Command::CloseSession)
            .unwrap();
        let other_runtime = Uuid::new_v4();
        assert!(matches!(
            client.request(other.id, other_runtime, start).unwrap(),
            ResponsePayload::Started { .. }
        ));
        client
            .request(other.id, other_runtime, Command::CloseSession)
            .unwrap();
        let ResponsePayload::Session {
            session: Some(restored),
        } = client
            .request(
                parent.id,
                Uuid::nil(),
                Command::HydrateSession {
                    session_id: parent.id,
                },
            )
            .unwrap()
        else {
            panic!("missing parent");
        };
        assert_eq!(restored.managed_workspace, managed_parent.managed_workspace);
        assert_eq!(restored.workspace, managed_parent.workspace);
    });
}
