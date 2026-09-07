use super::*;

#[test]
fn lifecycle_socket_global_workspace_guards_owned_archive_and_preserves_unrelated_paths() {
    let (root, parent, child) = seed_queue();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "Fixture"]);
    let owned = root.join("archived-worktree");
    git(&["worktree", "add", "-qb", "archived-work", owned.to_str().unwrap()]);
    std::fs::write(owned.join("kept.txt"), "original").unwrap();
    let store = StateStore::daemon(root.join("state.db"));
    let mut state = store.load().unwrap();
    let saved = state.sessions.iter_mut().find(|s| s.id == child.id).unwrap();
    store.hydrate(saved).unwrap();
    saved.archived = true;
    saved.managed_workspace = Some(serde_json::from_value(serde_json::json!({
        "task_id":parent.id,"name":"Archived child","repository":root,"base_commit":"HEAD",
        "target_branch":"main","target_commit":"HEAD","integration_branch":"main","integration_commit":"HEAD",
        "branch":"archived-work","path":owned,"owned":true,"created":true,"ready":true,"coordination":null,"error":null
    })).unwrap());
    // A legacy archived conversation pointing at the user's checkout does not
    // own that checkout and must not prevent unrelated user operations there.
    let mut legacy = AgentSession::new(child.project_id, ProviderKind::Codex);
    legacy.archived = true;
    legacy.workspace = child.workspace.clone();
    let legacy_id = legacy.id;
    state.sessions.push(legacy);
    state.mark_session_dirty(child.id); state.mark_session_dirty(legacy_id);
    store.save(&mut state).unwrap();
    let server = QueueServer::open(&root);
    server.backend.paused.store(true, Ordering::Release);
    let client = server.connect();
    let request = |operation| client.request(Uuid::nil(), Uuid::nil(), Command::Workspace { operation });
    let write = |path:PathBuf, relative:&str| WorkspaceOperation::WriteTextFile { root:path, relative_path:relative.into(), content:"changed".into() };
    assert!(request(write(owned.clone(), "kept.txt")).is_err());
    assert!(request(write(root.clone(), "archived-worktree/new.txt")).is_err());
    assert!(request(WorkspaceOperation::CheckoutBranch { cwd:owned.clone(), branch:"forbidden".into(), create:true }).is_err());
    assert!(request(WorkspaceOperation::DeleteSessionRefs { cwd:root.clone(), session_id:child.id }).is_err());
    assert!(request(WorkspaceOperation::DeleteRef { cwd:root.clone(), git_ref:crate::checkpoint::checkpoint_ref(child.id, 1) }).is_err());
    let ResponsePayload::Workspace { result:WorkspaceResult::TextFile { content } } = request(WorkspaceOperation::ReadTextFile { root:owned.clone(), relative_path:"kept.txt".into() }).unwrap() else { panic!("read-only history file expected") };
    assert_eq!(content, "original");
    assert!(request(WorkspaceOperation::InspectBranches { cwd:owned.clone() }).is_ok());
    assert!(request(write(root.clone(), "unrelated.txt")).is_ok());
    assert_eq!(std::fs::read_to_string(root.join("unrelated.txt")).unwrap(), "changed");
    assert!(request(WorkspaceOperation::CheckoutBranch { cwd:root.clone(), branch:"independent-work".into(), create:true }).is_ok());
    #[cfg(unix)] {
        let alias=root.join("owned-alias");
        std::os::unix::fs::symlink(&owned,&alias).unwrap();
        assert!(request(write(root.clone(), "owned-alias/new.txt")).is_err());
    }
    assert_eq!(std::fs::read_to_string(owned.join("kept.txt")).unwrap(), "original");
    assert!(!owned.join("new.txt").exists());
    drop(client); drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
