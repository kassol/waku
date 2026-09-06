use super::*;

fn git_at(path: &Path, args: &[&str]) -> String {
    let output = crate::command_env::plain_command("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn task_workspace_starts_at_selected_commit_without_moving_dirty_checkout() {
    let root = std::env::temp_dir().join(format!("waku-task-workspace-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_at(&repo, &["init", "-b", "main"]);
    git_at(&repo, &["config", "user.name", "Fixture"]);
    git_at(&repo, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(repo.join("tracked"), "base").unwrap();
    git_at(&repo, &["add", "."]);
    git_at(&repo, &["commit", "-m", "Base"]);
    let base = git_at(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("tracked"), "keep dirty").unwrap();
    std::fs::write(repo.join("untracked"), "keep untracked").unwrap();
    let backend = Arc::new(
        WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("state.db")),
        )
        .unwrap(),
    );
    let project = Project::from_path(repo.clone());
    let session = AgentSession::new(project.id, ProviderKind::Codex);
    let erased: Arc<dyn Backend> = backend.clone();
    let sink = EventSink::for_test(&erased, session.id, Uuid::nil());
    backend
        .handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id: session.id,
                runtime_id: Uuid::nil(),
                command: Command::SaveTaskState {
                    projects: vec![project],
                    sessions: vec![session.clone()],
                    live_session_ids: vec![session.id],
                },
            },
            sink.clone(),
        )
        .unwrap();
    let command: Command = serde_json::from_value(json!({
        "type":"stewardWorkspace", "operation": {
            "type":"begin", "name":"Implement parser", "targetBranch":"main", "expectedCommit":base,
        }
    }))
    .expect("managed code tasks must be available through the public daemon command");
    let response = backend
        .handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id: session.id,
                runtime_id: Uuid::nil(),
                command: command.clone(),
            },
            sink.clone(),
        )
        .unwrap();
    let value = serde_json::to_value(response).unwrap();
    let managed = &value["session"]["managed_workspace"];
    assert_eq!(managed["base_commit"], base);
    assert_eq!(managed["target_branch"], "main");
    let path = PathBuf::from(managed["path"].as_str().unwrap());
    assert_eq!(git_at(&path, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked")).unwrap(),
        "keep dirty"
    );
    assert!(repo.join("untracked").exists());
    assert_eq!(git_at(&repo, &["branch", "--show-current"]), "main");
    let repeated = backend
        .handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id: session.id,
                runtime_id: Uuid::nil(),
                command,
            },
            sink,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(repeated).unwrap()["session"]["managed_workspace"],
        *managed
    );
    drop(erased);
    drop(backend);
    let reopened = WakuBackend::new(
        DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
        StateStore::daemon(root.join("state.db")),
    )
    .unwrap();
    let state = reopened.task_state.lock();
    assert_eq!(
        serde_json::to_value(&state.sessions[0]).unwrap()["managed_workspace"],
        *managed
    );
    drop(state);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn task_workspace_preserves_preexisting_coordination_resources() {
    for occupied_branch in [true, false] {
        let root = std::env::temp_dir().join(format!("waku-task-preexisting-{}", Uuid::new_v4()));
        let repository = root.join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        git_at(&repository, &["init", "-b", "main"]);
        git_at(&repository, &["config", "user.name", "Fixture"]);
        git_at(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repository.join("base"), "keep").unwrap();
        git_at(&repository, &["add", "."]);
        git_at(&repository, &["commit", "-m", "Base"]);
        let backend = Arc::new(
            WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("state.db")),
            )
            .unwrap(),
        );
        let project = Project::from_path(repository.clone());
        let session = AgentSession::new(project.id, ProviderKind::Codex);
        let erased: Arc<dyn Backend> = backend.clone();
        let sink = EventSink::for_test(&erased, session.id, Uuid::nil());
        backend
            .handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: session.id,
                    runtime_id: Uuid::nil(),
                    command: Command::SaveTaskState {
                        projects: vec![project],
                        sessions: vec![session.clone()],
                        live_session_ids: vec![session.id],
                    },
                },
                sink.clone(),
            )
            .unwrap();
        let coordination_branch = format!(
            "waku/task-parser-{}-coordination",
            &session.id.simple().to_string()[..8]
        );
        let coordination_path = root
            .join("task-worktrees")
            .join(format!("{}-coordination", session.id));
        if occupied_branch {
            git_at(&repository, &["branch", &coordination_branch]);
        } else {
            std::fs::create_dir_all(&coordination_path).unwrap();
            std::fs::write(coordination_path.join("keep"), "unrelated resource").unwrap();
        }
        let result = backend.handle(Request { request_id:Uuid::new_v4(), session_id:session.id, runtime_id:Uuid::nil(), command:serde_json::from_value(json!({"type":"stewardWorkspace", "operation":{"type":"begin", "name":"Parser", "targetBranch":"main", "expectedCommit":""}})).unwrap() }, sink);
        assert!(
            result.is_err(),
            "preexisting coordination resources must be rejected before recording ownership: {result:?}"
        );
        assert!(
            backend.task_state.lock().sessions[0]
                .managed_workspace
                .is_none()
        );
        if occupied_branch {
            assert_eq!(
                git_at(&repository, &["rev-parse", &coordination_branch]),
                git_at(&repository, &["rev-parse", "main"])
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(coordination_path.join("keep")).unwrap(),
                "unrelated resource"
            );
        }
        drop(erased);
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }
}
