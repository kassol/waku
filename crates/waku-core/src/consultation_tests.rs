use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn consultation_is_scoped_and_does_not_change_the_source_turn_or_wait() {
    let root = std::env::temp_dir().join(format!("waku-consult-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let binary = root.join("claude");
    std::fs::write(&binary, r##"#!/usr/bin/env python3
import sys,json,pathlib
args=sys.argv[1:]
assert args[args.index('--tools')+1] == ''
assert '--safe-mode' in args and '--strict-mcp-config' in args
assert json.loads(args[args.index('--mcp-config')+1]) == {'mcpServers':{}}
assert '--no-session-persistence' in args
assert '--resume' not in args and '--model' not in args
prompt=args[-1]
assert 'SOURCE_QUESTION' in prompt and 'CHILD_RESULT' in prompt
assert 'SIBLING_SECRET' not in prompt and 'OTHER_PROJECT_SECRET' not in prompt
assert 'context_at' in prompt and 'last_activity' in prompt
context=json.loads(prompt)
assert context['records'][0]['summary']['session_id'] == context['source_session_id']
assert len(context['records']) == 33 and context['children_truncated'] is True
print(json.dumps({'type':'system','subtype':'init','tools':[],'mcp_servers':[]}))
if json.loads(prompt)['question'] == 'FORBIDDEN':
 print(json.dumps({'type':'control_request','request':{'subtype':'can_use_tool','tool_name':'Bash','input':{'command':'touch forbidden-marker'}}}))
else:
 print(json.dumps({'type':'result','subtype':'success','result':'The child completed; newer progress is unknown.'}))
"##).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = DaemonSettingsStore::open(root.join("settings.json")).unwrap();
    let mut configured = settings.get();
    configured
        .provider_binary_overrides
        .insert(ProviderKind::Claude, binary.to_string_lossy().into());
    settings.replace(configured).unwrap();
    let project = Project::from_path(root.clone());
    let mut source = AgentSession::new(project.id, ProviderKind::Codex);
    let mut child = AgentSession::new(project.id, ProviderKind::Claude);
    child.parent_session_id = Some(source.id);
    child.push_message(crate::model::MessageRole::Assistant, "CHILD_RESULT");
    let mut sibling = AgentSession::new(project.id, ProviderKind::Claude);
    sibling.push_message(crate::model::MessageRole::Assistant, "SIBLING_SECRET");
    let other_project = Project::from_path(root.join("other"));
    let mut foreign = AgentSession::new(other_project.id, ProviderKind::Claude);
    foreign.parent_session_id = Some(source.id);
    foreign.push_message(crate::model::MessageRole::Assistant, "OTHER_PROJECT_SECRET");
    let store = StateStore::daemon(root.join("state.db"));
    let mut seed = PersistedState::empty();
    seed.projects = vec![project.clone(), other_project];
    seed.sessions = vec![
        source.clone(),
        child.clone(),
        sibling.clone(),
        foreign.clone(),
    ];
    for index in 0..34 {
        let mut extra = AgentSession::new(project.id, ProviderKind::Claude);
        extra.parent_session_id = Some(source.id);
        extra.created_at = child.created_at + index + 1;
        extra.updated_at = 1;
        extra.push_message(crate::model::MessageRole::Assistant, "ADDITIONAL_CHILD");
        seed.sessions.push(extra);
    }
    for id in seed.sessions.iter().map(|s| s.id).collect::<Vec<_>>() {
        seed.mark_session_dirty(id);
    }
    store.save(&mut seed).unwrap();
    let backend = Arc::new(WakuBackend::new(settings, store).unwrap());
    let (client, stop, server) = serve_consultation(backend.clone());
    let call = |command| client.request(Uuid::nil(), Uuid::nil(), command);
    source.begin_turn("SOURCE_QUESTION");
    child.begin_turn("CHILD_RESULT");
    let child_id = child.id;
    call(Command::SaveTaskState {
        projects: vec![project],
        sessions: vec![source.clone(), child],
        live_session_ids: vec![source.id],
    })
    .unwrap();
    let registered = client
        .request(
            source.id,
            Uuid::nil(),
            Command::StewardWait {
                session_ids: vec![child_id],
            },
        )
        .unwrap();
    assert!(matches!(
        registered,
        ResponsePayload::StewardWait { wait: Some(_), .. }
    ));
    let before = call(Command::HydrateSession {
        session_id: source.id,
    })
    .unwrap();
    let command:Command=serde_json::from_value(json!({"type":"consult","sourceSessionId":source.id,"question":"How is the task progressing?"})).unwrap();
    let answer = serde_json::to_value(call(command).unwrap()).unwrap();
    assert_eq!(
        answer["consultation"]["source_session_id"],
        source.id.to_string()
    );
    assert_eq!(
        answer["consultation"]["exchanges"][0]["answer"],
        "The child completed; newer progress is unknown."
    );
    assert!(answer["consultation"]["context_at"].as_u64().unwrap() > 0);
    let after = call(Command::HydrateSession {
        session_id: source.id,
    })
    .unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    let denied = call(Command::Consult {
        source_session_id: source.id,
        question: "FORBIDDEN".into(),
    })
    .unwrap();
    let denied = serde_json::to_value(denied).unwrap();
    assert!(
        denied["consultation"]["exchanges"][1]["error"]
            .as_str()
            .unwrap()
            .contains("forbidden")
    );
    assert!(denied["consultation"]["exchanges"][1]["answer"].is_null());
    assert!(!root.join("forbidden-marker").exists());
    let answer = denied;
    let loaded = serde_json::to_value(
        call(Command::LoadConsultation {
            source_session_id: source.id,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loaded, answer);
    drop(client);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    drop(backend);
    let reopened = Arc::new(
        WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("state.db")),
        )
        .unwrap(),
    );
    let (client, stop, server) = serve_consultation(reopened.clone());
    let loaded = client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::LoadConsultation {
                source_session_id: source.id,
            },
        )
        .unwrap();
    assert_eq!(serde_json::to_value(loaded).unwrap(), answer);
    drop(client);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

fn serve_consultation(
    backend: Arc<WakuBackend>,
) -> (
    waku_client::DaemonClient,
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        crate::server::serve(
            listener,
            "fixture".into(),
            backend,
            stopping,
            crate::server::ServerOptions {
                allow_shutdown: true,
                ..crate::server::ServerOptions::default()
            },
        )
        .unwrap()
    });
    let client =
        waku_client::DaemonClient::connect(&address.to_string(), "fixture".into()).unwrap();
    (client, stop, server)
}

#[test]
fn consultation_shutdown_cancels_pipe_drain_after_provider_exit() {
    let root = std::env::temp_dir().join(format!("waku-consult-drain-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let binary = root.join("claude");
    let ready = root.join("ready");
    std::fs::write(
        &binary,
        format!(
            r#"#!/usr/bin/env python3
import os,time,pathlib
pid=os.fork()
if pid==0:
 time.sleep(10)
 os._exit(0)
pathlib.Path({:?}).write_text(str(pid))
os._exit(0)
"#,
            ready.to_string_lossy()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = DaemonSettingsStore::open(root.join("settings.json")).unwrap();
    let mut configured = settings.get();
    configured
        .provider_binary_overrides
        .insert(ProviderKind::Claude, binary.to_string_lossy().into());
    settings.replace(configured).unwrap();
    let backend =
        Arc::new(WakuBackend::new(settings, StateStore::daemon(root.join("state.db"))).unwrap());
    let (client, stop, server) = serve_consultation(backend.clone());
    let project = Project::from_path(root.clone());
    let source = AgentSession::new(project.id, ProviderKind::Claude);
    client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::SaveTaskState {
                projects: vec![project],
                sessions: vec![source.clone()],
                live_session_ids: vec![],
            },
        )
        .unwrap();
    let asking = client.clone();
    let (sent, received) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        let result = asking.request(
            Uuid::nil(),
            Uuid::nil(),
            Command::Consult {
                source_session_id: source.id,
                question: "progress?".into(),
            },
        );
        let _ = sent.send(result);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    client
        .request(Uuid::nil(), Uuid::nil(), Command::PrepareShutdown)
        .unwrap();
    let result = received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("shutdown must also bound pipe drain")
        .unwrap();
    let value = serde_json::to_value(result).unwrap();
    assert!(
        value["consultation"]["exchanges"][0]["error"]
            .as_str()
            .unwrap()
            .contains("cancelled")
    );
    worker.join().unwrap();
    drop(client);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    drop(backend);
    std::fs::remove_dir_all(root).unwrap();
}
