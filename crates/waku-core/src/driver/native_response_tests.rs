use super::*;
use std::io::{self, Write};
use serde_json::json;

#[test]
fn native_response_transport_confirms_only_after_flush_and_rejects_replay() {
    let pending = NativeRequests::default();
    let request = NativeRequest::new(json!("17"), json!({}), None, None, None);
    let token = request.token;
    pending.lock().insert("request".into(), request);
    struct Writer<'a> { pending: &'a NativeRequests, bytes: Vec<u8>, flushed: bool }
    impl Write for Writer<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(self.pending.lock().contains_key("request"));
            self.bytes.extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            assert!(self.pending.lock()["request"].attempted);
            self.flushed = true; Ok(())
        }
    }
    let mut writer = Writer { pending: &pending, bytes: Vec::new(), flushed: false };
    let (events, received) = unbounded();
    let id = uuid::Uuid::new_v4();
    let message = json!({"id":"17","result":{"decision":"accept"}});
    write_native_response(&mut writer, &pending, "request", token, &message, Some(id), &events).unwrap();
    assert!(writer.flushed);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&writer.bytes).unwrap(), message);
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.id == id && outcome.state == crate::model::InputDeliveryState::Received
        && outcome.confirmation == Some(crate::model::InputConfirmation::Transport)));
    let size = writer.bytes.len();
    write_native_response(&mut writer, &pending, "request", token, &message, Some(id), &events).unwrap();
    assert_eq!(writer.bytes.len(), size);
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.state == crate::model::InputDeliveryState::Failed));
}

#[test]
fn native_response_partial_write_is_uncertain_and_stale_request_writes_nothing() {
    struct BrokenWriter(usize);
    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            if self.0 == 0 { self.0 = 1; Ok(1) } else { Err(io::Error::other("broken pipe")) }
        }
        fn flush(&mut self) -> io::Result<()> { unreachable!() }
    }
    let pending = NativeRequests::default();
    let request = NativeRequest::new(json!(17), json!({}), None, None, None);
    let token = request.token;
    pending.lock().insert("request".into(), request);
    let (events, received) = unbounded();
    let id = uuid::Uuid::new_v4();
    assert!(write_native_response(&mut BrokenWriter(0), &pending, "request", token, &json!({}), Some(id), &events).is_err());
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.state == crate::model::InputDeliveryState::Uncertain && outcome.confirmation.is_none()));
    pending.lock().clear();
    let mut bytes = Vec::new();
    write_native_response(&mut bytes, &pending, "request", token, &json!({}), Some(id), &events).unwrap();
    assert!(bytes.is_empty());
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.state == crate::model::InputDeliveryState::Failed));
}

#[test]
fn native_response_turn_change_during_flush_is_uncertain() {
    let pending = NativeRequests::default();
    let request = NativeRequest::new(json!(17), json!({}), None, None, None);
    let token = request.token;
    pending.lock().insert("request".into(), request);
    struct EndingWriter<'a>(&'a NativeRequests);
    impl Write for EndingWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> { self.0.lock().clear(); Ok(()) }
    }
    let (events, received) = unbounded();
    write_native_response(&mut EndingWriter(&pending), &pending, "request", token, &json!({}), Some(uuid::Uuid::new_v4()), &events).unwrap();
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.state == crate::model::InputDeliveryState::Uncertain && outcome.confirmation.is_none()));
}

#[test]
fn native_response_zero_byte_failure_is_known_unsent() {
    struct ClosedWriter;
    impl Write for ClosedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> { Err(io::Error::other("closed")) }
        fn flush(&mut self) -> io::Result<()> { unreachable!() }
    }
    let pending = NativeRequests::default();
    let request = NativeRequest::new(json!(17), json!({}), None, None, None);
    let token = request.token;
    pending.lock().insert("request".into(), request);
    let (events, received) = unbounded();
    assert!(write_native_response(&mut ClosedWriter, &pending, "request", token, &json!({}), Some(uuid::Uuid::new_v4()), &events).is_err());
    assert!(matches!(received.recv().unwrap(), DriverEvent::InputDeliveryOutcome(outcome)
        if outcome.state == crate::model::InputDeliveryState::Failed && outcome.confirmation.is_none()));
    assert!(pending.lock().contains_key("request"));
}

#[test]
#[ignore = "uses installed authenticated Claude and its configured model"]
fn native_claude_tracked_response() -> anyhow::Result<()> {
    native_response_compatibility(ProviderKind::Claude, "claude")
}

#[test]
#[ignore = "uses installed authenticated Codex and a new Astra session"]
fn native_codex_tracked_response() -> anyhow::Result<()> {
    native_response_compatibility(ProviderKind::Codex, "codex")
}

fn native_response_compatibility(provider: ProviderKind, executable: &str) -> anyhow::Result<()> {
    use crate::model::{InputConfirmation, InputDeliveryState};
    use std::time::{Duration, Instant};
    use uuid::Uuid;
    let binary = crate::command_env::find_executable(executable)
        .ok_or_else(|| anyhow::anyhow!("{executable} executable not found"))?;
    let cwd = std::env::temp_dir().join(format!("waku-native-response-{}", Uuid::new_v4()));
    std::fs::create_dir(&cwd)?;
    let (sender, events) = test_event_channel();
    let driver = start_local(provider, DriverStartOptions {
        binary, cwd: cwd.clone(), mode: RuntimeMode::Ask,
        model: (provider == ProviderKind::Codex).then(|| "gpt-6-astra".into()),
        reasoning_effort: (provider == ProviderKind::Codex).then(|| "medium".into()),
        service_tier: None, context_window: None, agent_preset: None,
        computer_use_enabled: false, provider_cursor: None,
    }, sender)?;
    let result = (|| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(240);
        let next = || -> anyhow::Result<DriverEvent> {
            Ok(events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?)
        };
        loop {
            match next()? {
                DriverEvent::Connected { .. } => break,
                DriverEvent::Error(error) => anyhow::bail!("native startup failed: {error}"),
                DriverEvent::ProcessExited => anyhow::bail!("native startup exited"),
                _ => {}
            }
        }
        let marker = format!("NATIVE_{}", Uuid::new_v4().simple());
        let output = cwd.join("native-response.txt");
        let question = if provider == ProviderKind::Claude {
            "First use the native AskUserQuestion tool to ask which format to use, with exactly two options JSON and CSV. Wait for the answer before continuing. "
        } else { "" };
        driver.prompt(format!("This is an authorized isolated native approval compatibility check. {question}Then invoke the shell tool once to execute exactly: printf %s {marker} > '{}'. Request native user approval for this write; for Codex use sandbox_permissions=require_escalated with a justification asking to write this test file. Do not simulate approval in text. Do not use agents, MCP, network, or inspect or modify any other files or configuration. After the command completes reply exactly DONE.", output.display()));
        let mut attempts = Vec::new();
        let mut receipts = Vec::new();
        let mut permissions = 0;
        let mut questions_seen = 0;
        loop {
            match next()? {
                DriverEvent::Permission { request_id, detail, options, .. } => {
                    anyhow::ensure!(detail.contains("native-response.txt"), "unexpected native permission: {detail}");
                    let option = options.iter().find(|o| matches!(o.id.as_str(), "allow" | "accept"))
                        .ok_or_else(|| anyhow::anyhow!("native permission has no one-time approval option"))?;
                    let id = Uuid::new_v4();
                    driver.respond_tracked(request_id, NativeResponse::Permission { option_id: option.id.clone() }, id)?;
                    attempts.push(id);
                    permissions += 1;
                }
                DriverEvent::UserInputRequested { request_id, questions } => {
                    anyhow::ensure!(provider == ProviderKind::Claude && questions.len() == 1, "unexpected native questions");
                    let answers = questions.iter().map(|q| UserInputAnswer {
                        question_id: q.id.clone(), answers: vec!["JSON".into()],
                    }).collect();
                    let id = Uuid::new_v4();
                    driver.respond_tracked(request_id, NativeResponse::UserInput { answers }, id)?;
                    attempts.push(id);
                    questions_seen += 1;
                }
                DriverEvent::InputDeliveryOutcome(outcome) => {
                    anyhow::ensure!(attempts.contains(&outcome.id) && !receipts.contains(&outcome.id), "unknown or duplicate native response outcome");
                    anyhow::ensure!(outcome.state == InputDeliveryState::Received && outcome.confirmation == Some(InputConfirmation::Transport), "native response not transport-confirmed: {:?}", outcome);
                    receipts.push(outcome.id);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    anyhow::ensure!(success && permissions == 1, "native approval path missing or failed: permissions={permissions}, success={success}");
                    anyhow::ensure!(questions_seen == usize::from(provider == ProviderKind::Claude), "native question path missing");
                    anyhow::ensure!(receipts == attempts, "native response receipts missing");
                    anyhow::ensure!(std::fs::read_to_string(&output)? == marker, "native command output mismatch");
                    break;
                }
                DriverEvent::Error(error) => anyhow::bail!("native response failed: {error}"),
                DriverEvent::ProcessExited | DriverEvent::TurnInterrupted => anyhow::bail!("native response interrupted"),
                _ => {}
            }
        }
        eprintln!("{executable}: native requests answered; Transport confirmed; provider turn and file verified");
        Ok(())
    })();
    drop(driver);
    let deadline = Instant::now() + Duration::from_secs(30);
    let exited = loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(DriverEvent::ProcessExited) => break true,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    anyhow::ensure!(exited, "owned native process did not exit; temporary directory {}", cwd.display());
    std::fs::remove_dir_all(&cwd)?;
    result
}
