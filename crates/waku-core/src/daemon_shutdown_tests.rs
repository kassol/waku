use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn assert_shutdown_bounds_fixture(fixture: &str) {
    let root = std::env::temp_dir().join(format!("waku-eof-exit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // The watchdog bounds even a panic before EOF; the release guard avoids
    // leaving the fixture alive when an assertion unwinds.
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("release"), "");
        }
    }
    let release = Release(root.clone());
    let binary = root.join("codex-fixture");
    std::fs::write(&binary, fixture).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = Arc::new(
        WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("state.db")),
        )
        .unwrap(),
    );
    let session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
    let session_id = session.id;
    let runtime_id = Uuid::new_v4();
    let erased: Arc<dyn Backend> = backend.clone();
    let sink = EventSink::for_test(&erased, session_id, runtime_id);
    backend
        .handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id,
                runtime_id,
                command: Command::SaveTaskState {
                    projects: vec![],
                    sessions: vec![session],
                    live_session_ids: vec![session_id],
                },
            },
            sink.clone(),
        )
        .unwrap();
    let (sender, receiver) = driver::test_event_channel();
    let handle = driver::start_local_with_mcp(
        ProviderKind::Codex,
        DriverStartOptions {
            binary,
            cwd: root.clone(),
            mode: RuntimeMode::Ask,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
        },
        sender,
        None,
    )
    .unwrap();
    backend
        .sessions
        .lock()
        .insert(session_id, (runtime_id, handle));
    let forward_sink = sink.clone();
    backend.forwarders.lock().insert(
        session_id,
        (
            runtime_id,
            std::thread::spawn(move || {
                forward_runtime_events(ProviderKind::Codex, session_id, receiver, forward_sink)
            }),
        ),
    );
    let (done, completed) = crossbeam_channel::bounded(1);
    let closing = backend.clone();
    let close_sink = sink.clone();
    let shutdown = std::thread::spawn(move || {
        let _ = done.send(closing.handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id,
                runtime_id,
                command: Command::PrepareShutdown,
            },
            close_sink,
        ));
    });
    let within_grace = completed.recv_timeout(Duration::from_secs(3)).ok();
    let saw_eof = root.join("eof").exists();
    // Release before asserting, then join the actual provider-exit drain.
    drop(release);
    let bounded = within_grace.is_some();
    let result =
        within_grace.unwrap_or_else(|| completed.recv_timeout(Duration::from_secs(20)).unwrap());
    shutdown.join().unwrap();
    drop(sink);
    drop(erased);
    drop(backend);
    std::fs::remove_dir_all(root).unwrap();
    assert!(saw_eof, "fixture must observe the driver's shutdown EOF");
    assert!(
        result.is_ok(),
        "provider exit must drain and save: {result:?}"
    );
    assert!(
        bounded,
        "PrepareShutdown waited indefinitely after Codex received stdin EOF; it completed only after the test released the provider"
    );
}

#[test]
fn prepare_shutdown_bounds_a_codex_process_that_ignores_stdin_eof() {
    assert_shutdown_bounds_fixture(
        r#"#!/usr/bin/env python3
import os, pathlib, sys, threading, time
root = pathlib.Path.cwd()
threading.Timer(15, lambda: os._exit(0)).start()
root.joinpath('started').write_text(str(os.getpid()))
for line in sys.stdin:
    pass
root.joinpath('eof').write_text('received')
while not root.joinpath('release').exists():
    time.sleep(0.01)
os._exit(0)
"#,
    );
}

#[test]
fn prepare_shutdown_bounds_inherited_pipes_after_codex_parent_exits() {
    assert_shutdown_bounds_fixture(
        r#"#!/usr/bin/env python3
import os, pathlib, sys, threading, time
root = pathlib.Path.cwd()
if os.fork() == 0:
    # Keep stdout/stderr open after the main provider exits. The deadline
    # remains effective if the Rust test panics before sending release.
    os.close(0)
    deadline = time.monotonic() + 15
    while not root.joinpath('release').exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    os._exit(0)
threading.Timer(15, lambda: os._exit(0)).start()
for line in sys.stdin:
    pass
root.joinpath('eof').write_text('received')
os._exit(0)
"#,
    );
}
