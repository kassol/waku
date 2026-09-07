use super::*;
use std::os::unix::fs::PermissionsExt;

fn seed_continuation() -> (PathBuf, AgentSession, AgentSession, PathBuf) {
    let (root, parent, child) = seed_queue();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-qm",
        "Initial",
    ]);
    let head = git(&["rev-parse", "HEAD"]);
    let integration = root.join("integration");
    let owned = root.join("continued-worktree");
    git(&[
        "worktree",
        "add",
        "-qb",
        "integration",
        integration.to_str().unwrap(),
    ]);
    git(&[
        "worktree",
        "add",
        "-qb",
        "old-child",
        owned.to_str().unwrap(),
    ]);
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap();
    for (id, path, branch, coordination) in [
        (
            parent.id,
            &integration,
            "integration",
            Some(json!({"path":root,"branch":git(&["branch","--show-current"]),"created":true})),
        ),
        (child.id, &owned, "old-child", None),
    ] {
        let session = state.sessions.iter_mut().find(|s| s.id == id).unwrap();
        store.hydrate(session).unwrap();
        session.managed_workspace = Some(serde_json::from_value(json!({"task_id":parent.id,"name":"Continue task","repository":root,"base_commit":head,"target_branch":"main","target_commit":head,"integration_branch":"integration","integration_commit":head,"branch":branch,"path":path,"owned":true,"created":true,"ready":true,"coordination":coordination,"error":null})).unwrap());
        if id == child.id {
            session.workspace = crate::model::SessionWorkspace::Worktree {
                path: owned.clone(),
                branch: branch.into(),
            };
        }
        state.mark_session_dirty(id);
    }
    store.save(&mut state).unwrap();
    let binary = root.join("controlled-provider");
    std::fs::write(
        &binary,
        include_str!("../tests/fixtures/codex_create.py").replace(
            "if prompt in ('write fixture result', 'hold fixture turn'):",
            "if True:",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    (root, parent, child, owned)
}

fn archive_continuation(client: &DaemonClient, parent: Uuid, child: Uuid) -> Uuid {
    let ResponsePayload::LifecycleCompleted { completion, .. } = client
        .request(
            parent,
            Uuid::nil(),
            lifecycle_complete_command(client, parent, child, "accepted"),
        )
        .unwrap()
    else {
        panic!("completion")
    };
    completion.id
}
fn continue_command(child: Uuid, completion: Uuid, operation: Uuid) -> Command {
    Command::ContinueChild {
        child_session_id: child,
        completion_id: completion,
        operation_id: operation,
        instruction: "Continue the explicit old task".into(),
    }
}
fn continuation_snapshot(client: &DaemonClient, child: Uuid) -> AgentSession {
    let ResponsePayload::Session {
        session: Some(session),
    } = client
        .request(
            child,
            Uuid::nil(),
            Command::HydrateSession { session_id: child },
        )
        .unwrap()
    else {
        panic!("session")
    };
    session
}

#[test]
fn continuation_socket_retained_workspace_resumes_once_and_preserves_completion_after_restart() {
    let (root, parent, child, owned) = seed_continuation();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let completion = archive_continuation(&client, parent.id, child.id);
    let before = continuation_snapshot(&client, child.id);
    assert!(before.archived);
    let operation = Uuid::new_v4();
    let ResponsePayload::LifecycleContinued {
        session,
        continuation,
    } = client
        .request(
            parent.id,
            Uuid::nil(),
            continue_command(child.id, completion, operation),
        )
        .unwrap()
    else {
        panic!("continuation")
    };
    assert!(!session.archived, "{:?}", continuation);
    assert_eq!(continuation.result_session_id, Some(child.id));
    assert_eq!(session.completions, before.completions);
    assert!(session.lifecycle_revision > before.lifecycle_revision);
    for id in [operation, Uuid::new_v4()] {
        let ResponsePayload::LifecycleContinued { continuation, .. } = client
            .request(
                parent.id,
                Uuid::nil(),
                continue_command(child.id, completion, id),
            )
            .unwrap()
        else {
            panic!("repeat")
        };
        assert_eq!(continuation.id, operation);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !owned.join("child-calls.jsonl").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(owned.join("child-calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    drop(client);
    drop(server);
    let server = QueueServer::open(&root);
    let client = server.connect();
    let ResponsePayload::LifecycleContinued {
        session,
        continuation,
    } = client
        .request(
            parent.id,
            Uuid::nil(),
            Command::StewardLifecycle {
                operation: crate::model::StewardLifecycleOperation::Status {
                    session_id: child.id,
                    operation_id: operation,
                },
            },
        )
        .unwrap()
    else {
        panic!("status")
    };
    assert_eq!(continuation.result_session_id, Some(child.id));
    assert!(!session.archived);
    assert_eq!(session.completions, before.completions);
    assert_eq!(
        std::fs::read_to_string(owned.join("child-calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn continuation_socket_removed_workspace_creates_one_linked_replacement() {
    let (root, parent, child, owned) = seed_continuation();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let completion = archive_continuation(&client, parent.id, child.id);
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["worktree", "remove", owned.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let operation = Uuid::new_v4();
    let ResponsePayload::LifecycleContinued {
        session,
        continuation,
    } = client
        .request(
            parent.id,
            Uuid::nil(),
            continue_command(child.id, completion, operation),
        )
        .unwrap()
    else {
        panic!("continuation")
    };
    assert!(session.archived);
    assert_eq!(
        continuation.state,
        InputDeliveryState::Received,
        "{:?}",
        continuation
    );
    let replacement = continuation.result_session_id.unwrap();
    assert_ne!(replacement, child.id);
    let created = continuation_snapshot(&client, replacement);
    assert_eq!(created.parent_session_id, Some(parent.id));
    let prompt = &created.messages[0].content;
    assert!(
        prompt.contains("Fixture task")
            && prompt.contains("Explicit completion")
            && prompt.contains("Continue the explicit old task")
    );
    assert!(
        prompt.contains(&completion.to_string())
            || prompt.contains(&session.completions[0].summary_message_id.to_string())
    );
    assert_ne!(created.workspace.path(), Some(root.as_path()));
    assert!(!owned.exists());
    let ResponsePayload::LifecycleContinued {
        continuation: repeat,
        ..
    } = client
        .request(
            parent.id,
            Uuid::nil(),
            continue_command(child.id, completion, Uuid::new_v4()),
        )
        .unwrap()
    else {
        panic!("repeat")
    };
    assert_eq!(repeat.id, operation);
    assert_eq!(repeat.result_session_id, Some(replacement));
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn continuation_socket_rejects_old_authority_dirty_and_nested_assignment_without_effects() {
    let (root, parent, child, owned) = seed_continuation();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let completion = archive_continuation(&client, parent.id, child.id);
    let operation = Uuid::new_v4();
    assert!(
        client
            .request(
                parent.id,
                Uuid::nil(),
                Command::StewardLifecycle {
                    operation: crate::model::StewardLifecycleOperation::Continue {
                        session_id: child.id,
                        completion_id: completion,
                        operation_id: operation,
                        instruction: "Repeat model work".into(),
                        authority_message_id: parent.messages[0].id
                    }
                }
            )
            .is_err()
    );
    std::fs::write(owned.join("unrelated.txt"), "preserve").unwrap();
    assert!(
        client
            .request(
                parent.id,
                Uuid::nil(),
                continue_command(child.id, completion, operation)
            )
            .is_err()
    );
    assert!(
        continuation_snapshot(&client, child.id)
            .continuations
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(owned.join("unrelated.txt")).unwrap(),
        "preserve"
    );
    std::fs::remove_file(owned.join("unrelated.txt")).unwrap();
    // A model-authored nested assignment cannot create user authority.
    {
        let mut state = server.backend.inner.task_state.lock();
        let manager = state
            .sessions
            .iter_mut()
            .find(|s| s.id == parent.id)
            .unwrap();
        manager.parent_session_id = Some(Uuid::new_v4());
        manager.push_message(
            crate::model::MessageRole::User,
            "Continue from model assignment",
        );
        state.mark_session_dirty(parent.id);
        server.backend.inner.task_store.save(&mut state).unwrap();
    }
    assert!(
        client
            .request(
                parent.id,
                Uuid::nil(),
                continue_command(child.id, completion, operation)
            )
            .is_err()
    );
    {
        let mut state = server.backend.inner.task_state.lock();
        state
            .sessions
            .iter_mut()
            .find(|s| s.id == parent.id)
            .unwrap()
            .parent_session_id = None;
        state.mark_session_dirty(parent.id);
        server.backend.inner.task_store.save(&mut state).unwrap();
    }
    let ResponsePayload::LifecycleContinued { session, .. } = client
        .request(
            parent.id,
            Uuid::nil(),
            continue_command(child.id, completion, operation),
        )
        .unwrap()
    else {
        panic!("fixed dirty")
    };
    assert!(!session.archived);
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn continuation_socket_revoked_foreign_and_delivered_parent_preserve_archive() {
    for case in ["revoked", "foreign", "delivered"] {
        let (root, parent, child, owned) = seed_continuation();
        let server = QueueServer::open(&root);
        server.backend.paused.store(true, Ordering::Release);
        let client = server.connect();
        let completion = archive_continuation(&client, parent.id, child.id);
        {
            let mut state = server.backend.inner.task_state.lock();
            if case == "foreign" {
                state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == child.id)
                    .unwrap()
                    .managed_workspace
                    .as_mut()
                    .unwrap()
                    .owned = false;
                state.mark_session_dirty(child.id);
            } else {
                let manager = state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == parent.id)
                    .unwrap();
                if case == "revoked" {
                    manager.runtime_mode = RuntimeMode::Ask;
                } else {
                    manager.managed_workspace.as_mut().unwrap().deliveries.push(
                        crate::model::WorkspaceDelivery {
                            error: None,
                            commit: "delivered".into(),
                            reference: "saved".into(),
                            target_branch: "main".into(),
                            previous_target_commit: "previous".into(),
                            evidence: Vec::new(),
                            completed: true,
                        },
                    );
                }
                state.mark_session_dirty(parent.id);
            }
            server.backend.inner.task_store.save(&mut state).unwrap();
        }
        let before = continuation_snapshot(&client, child.id);
        assert!(
            client
                .request(
                    parent.id,
                    Uuid::nil(),
                    continue_command(child.id, completion, Uuid::new_v4())
                )
                .is_err(),
            "{case}"
        );
        let after = continuation_snapshot(&client, child.id);
        assert!(after.archived);
        assert!(after.continuations.is_empty());
        assert_eq!(
            serde_json::to_value(after.messages).unwrap(),
            serde_json::to_value(before.messages).unwrap()
        );
        assert!(owned.exists());
        assert!(!owned.join("child-calls.jsonl").exists());
        drop(client);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn continuation_socket_intent_save_failure_has_no_effect_and_restart_never_resends_accepted() {
    let (root, parent, child, owned) = seed_continuation();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let completion = archive_continuation(&client, parent.id, child.id);
    let operation = Uuid::new_v4();
    let database = rusqlite::Connection::open(root.join("state.db")).unwrap();
    database.execute_batch(&format!("CREATE TRIGGER reject_continue BEFORE UPDATE ON session_details WHEN NEW.session_id='{}' BEGIN SELECT RAISE(ABORT,'fixture intent failure'); END;",child.id)).unwrap();
    let before = continuation_snapshot(&client, parent.id);
    assert!(
        client
            .request(
                parent.id,
                Uuid::nil(),
                continue_command(child.id, completion, operation)
            )
            .is_err()
    );
    assert!(
        continuation_snapshot(&client, child.id)
            .continuations
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(continuation_snapshot(&client, parent.id).messages).unwrap(),
        serde_json::to_value(before.messages).unwrap()
    );
    database
        .execute_batch("DROP TRIGGER reject_continue")
        .unwrap();
    // A persisted intent with no outcome represents a crash before the caller
    // learned whether execution crossed its boundary. Restart must only report it.
    {
        let mut state = server.backend.inner.task_state.lock();
        let manager = state
            .sessions
            .iter_mut()
            .find(|s| s.id == parent.id)
            .unwrap();
        let authority = manager.push_message(
            crate::model::MessageRole::User,
            "Continue the explicit old task",
        );
        let source = state
            .sessions
            .iter_mut()
            .find(|s| s.id == child.id)
            .unwrap();
        source.continuations.push(crate::model::ChildContinuation {
            id: operation,
            source_completion_id: completion,
            manager_session_id: parent.id,
            authority_message_id: authority,
            instruction: "Continue the explicit old task".into(),
            result_session_id: Some(child.id),
            state: InputDeliveryState::Accepted,
            reason: None,
            created_at: crate::model::unix_time(),
        });
        source.lifecycle_revision += 1;
        state.mark_session_dirty(parent.id);
        state.mark_session_dirty(child.id);
        server.backend.inner.task_store.save(&mut state).unwrap();
    }
    drop(database);
    drop(client);
    drop(server);
    let server = QueueServer::open(&root);
    let client = server.connect();
    let ResponsePayload::LifecycleContinued {
        session,
        continuation,
    } = client
        .request(
            parent.id,
            Uuid::nil(),
            continue_command(child.id, completion, operation),
        )
        .unwrap()
    else {
        panic!("retry")
    };
    assert_eq!(continuation.state, InputDeliveryState::Uncertain);
    assert!(session.archived);
    assert!(!owned.join("child-calls.jsonl").exists());
    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
