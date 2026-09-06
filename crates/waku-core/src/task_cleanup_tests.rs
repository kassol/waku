use super::task_workspace::git;
use super::*;
use crate::model::{SessionWorkspace, TurnStatus};

struct CleanupDriver;
impl crate::driver::DriverControl for CleanupDriver {
    fn prompt(&self, _: String) {
        panic!("cleanup must not send a prompt");
    }
    fn cancel(&self) {
        panic!("cleanup must not cancel active work");
    }
    fn respond(&self, _: String, _: String) {}
    fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        unreachable!()
    }
}

#[test]
fn task_workspace_cleanup_retains_unsafe_resources_and_rechecks_retries() {
    for case in [
        "untracked",
        "dirty",
        "shared",
        "active",
        "unaccepted",
        "partial",
        "partial-new-work",
        "background",
        "terminal",
        "durable-input",
    ] {
        let root = std::env::temp_dir().join(format!("waku-cleanup-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "Fixture"]).unwrap();
        git(&repo, &["config", "user.email", "fixture@example.invalid"]).unwrap();
        std::fs::write(repo.join("tracked"), "base").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "Base"]).unwrap();
        let backend = Arc::new(
            WakuBackend::new(
                DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                StateStore::daemon(root.join("app.db")),
            )
            .unwrap(),
        );
        let erased: Arc<dyn Backend> = backend.clone();
        let project = Project::from_path(repo.clone());
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        let sink = EventSink::for_test(&erased, session.id, Uuid::nil());
        let session_id = session.id;
        let call = |command| {
            backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id,
                        runtime_id: Uuid::nil(),
                        command,
                    },
                    sink.clone(),
                )
                .unwrap()
        };
        call(Command::SaveTaskState {
            projects: vec![project.clone()],
            sessions: vec![session.clone()],
            live_session_ids: vec![session.id],
        });
        let operation = |value| {
            serde_json::from_value(json!({"type":"stewardWorkspace","operation":value})).unwrap()
        };
        let ResponsePayload::TaskWorkspace { session: saved } = call(operation(json!({
            "type":"begin", "name":"Cleanup fixture", "targetBranch":"main", "expectedCommit":""
        }))) else {
            panic!("task missing");
        };
        let task = saved.managed_workspace.clone().unwrap();
        let coordination = task.coordination.as_ref().unwrap();
        session = saved;
        session.begin_turn("Saved history survives cleanup");
        if case != "active" && case != "durable-input" {
            session.finish_active_turn(TurnStatus::Completed);
        }
        let mut sessions = vec![session.clone()];
        match case {
            "untracked" => {
                std::fs::write(coordination.path.join("keep"), "user work").unwrap();
            }
            "dirty" => {
                std::fs::write(coordination.path.join("tracked"), "user edit").unwrap();
            }
            "shared" => {
                let mut other = AgentSession::new(project.id, ProviderKind::Codex);
                other.begin_turn("Other session");
                other.finish_active_turn(TurnStatus::Completed);
                other.workspace = SessionWorkspace::Worktree {
                    path: coordination.path.clone(),
                    branch: coordination.branch.clone(),
                };
                sessions.push(other);
            }
            "unaccepted" => {
                std::fs::write(coordination.path.join("new"), "unaccepted result").unwrap();
                git(&coordination.path, &["add", "."]).unwrap();
                git(&coordination.path, &["commit", "-m", "Unaccepted work"]).unwrap();
            }
            "background" => {
                backend.sessions.lock().insert(
                    session.id,
                    (
                        Uuid::nil(),
                        DriverHandle::from_control(Arc::new(CleanupDriver)),
                    ),
                );
                backend.runtime_workspaces.lock().insert(
                    session.id,
                    std::fs::canonicalize(&coordination.path).unwrap(),
                );
                backend.track_cleanup_background(
                    session.id,
                    Uuid::nil(),
                    &DriverEvent::BackgroundWork(crate::model::BackgroundWorkEvent::Upsert(
                        crate::model::BackgroundWorkItem::new(
                            crate::model::BackgroundWorkKind::Process,
                            "fixture-live",
                            "Still writing",
                            crate::model::BackgroundWorkStatus::Running,
                        ),
                    )),
                );
            }
            "terminal" => {
                backend.terminal_workspaces.lock().insert(
                    Uuid::new_v4(),
                    std::fs::canonicalize(&coordination.path).unwrap(),
                );
            }
            "partial" | "partial-new-work" => {
                let mut task = task.clone();
                task.cleanup.push(crate::model::WorkspaceCleanup {
                    path: coordination.path.clone(),
                    branch: coordination.branch.clone(),
                    commit: Some(task.base_commit.clone()),
                    status: crate::model::WorkspaceCleanupStatus::Ready,
                    reason: None,
                });
                backend.save_task_workspace(session.id, task).unwrap();
                if case == "partial-new-work" {
                    std::fs::write(
                        coordination.path.join("new-user-work"),
                        "retain after restart",
                    )
                    .unwrap();
                } else {
                    git(
                        &repo,
                        &["worktree", "remove", coordination.path.to_str().unwrap()],
                    )
                    .unwrap();
                }
            }
            _ => {}
        }
        let ids = sessions.iter().map(|session| session.id).collect();
        call(Command::SaveTaskState {
            projects: vec![project.clone()],
            sessions,
            live_session_ids: ids,
        });
        if case == "durable-input" {
            backend.sessions.lock().insert(
                session.id,
                (
                    Uuid::nil(),
                    DriverHandle::from_control(Arc::new(CleanupDriver)),
                ),
            );
            backend.runtime_workspaces.lock().insert(
                session.id,
                std::fs::canonicalize(&coordination.path).unwrap(),
            );
            let mut controller = AgentSession::new(project.id, ProviderKind::Codex);
            controller.begin_turn("Manage queued input");
            {
                let mut state = backend.task_state.lock();
                state
                    .sessions
                    .iter_mut()
                    .find(|item| item.id == session.id)
                    .unwrap()
                    .parent_session_id = Some(controller.id);
                state.sessions.push(controller.clone());
                state.mark_session_dirty(session.id);
                state.mark_session_dirty(controller.id);
                backend.task_store.save(&mut state).unwrap();
            }
            let response = backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: controller.id,
                        runtime_id: Uuid::nil(),
                        command: Command::StewardPrompt {
                            child_session_id: session.id,
                            prompt: "Pending durable feedback".into(),
                            delivery_id: Some(Uuid::new_v4()),
                        },
                    },
                    sink.clone(),
                )
                .unwrap();
            assert_eq!(
                serde_json::to_value(response).unwrap()["delivery"]["state"],
                "queued"
            );
            sink.send(
                event_to_wire(DriverEvent::TurnFinished {
                    success: true,
                    summary: None,
                })
                .unwrap(),
            )
            .unwrap();
            let (saved, _) = backend.managed_task(session.id).unwrap();
            assert!(saved.active_turn_id().is_none());
            assert!(saved.queued_messages.is_empty());
        }
        let ResponsePayload::TaskWorkspace { session: delivered } = call(operation(json!({
            "type":"deliver", "commit":task.base_commit, "expectedTargetCommit":task.base_commit,
            "evidence":[{"commit":task.base_commit,"checks":"base verified","environment":"temporary repository","reviewer":"fixture reviewer"}]
        }))) else {
            panic!("delivery missing");
        };
        assert!(
            !task.path.exists(),
            "safe integration resource should clean in {case}"
        );
        if case == "partial" {
            assert!(!coordination.path.exists());
            assert!(
                git(
                    &repo,
                    &[
                        "show-ref",
                        "--verify",
                        &format!("refs/heads/{}", coordination.branch)
                    ]
                )
                .is_err()
            );
        } else {
            assert!(
                coordination.path.exists(),
                "unsafe resource must survive {case}"
            );
            let saved = delivered
                .managed_workspace
                .as_ref()
                .unwrap()
                .cleanup
                .iter()
                .find(|item| item.path == coordination.path)
                .unwrap();
            assert!(saved.reason.is_some(), "retention must explain {case}");
        }
        let reference = &delivered.managed_workspace.as_ref().unwrap().deliveries[0].reference;
        assert_eq!(
            git(&repo, &["rev-parse", reference]).unwrap(),
            task.base_commit
        );
        let store = StateStore::daemon(root.join("app.db"));
        let mut restored = store
            .load()
            .unwrap()
            .sessions
            .into_iter()
            .find(|item| item.id == session.id)
            .unwrap();
        store.hydrate(&mut restored).unwrap();
        assert!(!restored.messages.is_empty());
        assert_eq!(restored.managed_workspace, delivered.managed_workspace);
        if case == "untracked" || case == "dirty" {
            if case == "untracked" {
                std::fs::remove_file(coordination.path.join("keep")).unwrap();
            } else {
                std::fs::write(coordination.path.join("tracked"), "base").unwrap();
            }
            call(operation(json!({"type":"cleanup","sessionId":session.id})));
            assert!(!coordination.path.exists());
            assert_eq!(
                git(&repo, &["rev-parse", reference]).unwrap(),
                task.base_commit
            );
        }
        if case == "partial-new-work" {
            let reopened = Arc::new(
                WakuBackend::new(
                    DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
                    StateStore::daemon(root.join("app.db")),
                )
                .unwrap(),
            );
            let service: Arc<dyn Backend> = reopened.clone();
            let events = EventSink::for_test(&service, session.id, Uuid::nil());
            reopened
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: session.id,
                        runtime_id: Uuid::nil(),
                        command: operation(json!({"type":"cleanup","sessionId":session.id})),
                    },
                    events,
                )
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(coordination.path.join("new-user-work")).unwrap(),
                "retain after restart"
            );
        }
        if case == "background" {
            assert!(backend.sessions.lock().contains_key(&session.id));
            backend.track_cleanup_background(
                session.id,
                Uuid::nil(),
                &DriverEvent::BackgroundWork(crate::model::BackgroundWorkEvent::ReconcileLive {
                    items: Vec::new(),
                }),
            );
            backend.resume_stewards(sink.clone());
            assert!(!coordination.path.exists());
            assert!(!backend.sessions.lock().contains_key(&session.id));
        }
        drop(erased);
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }
}
