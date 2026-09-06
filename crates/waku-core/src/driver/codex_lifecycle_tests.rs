//! Opt-in native Codex regression. Uses only a new temporary working directory.
use super::*;
use crate::model::{BackgroundWorkEvent, BackgroundWorkKind};
use std::io::Write;
use std::time::{Duration, Instant};

fn receive(
    events: &Receiver<DriverEvent>,
    deadline: Instant,
    evidence: &mut std::fs::File,
) -> anyhow::Result<DriverEvent> {
    let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
    let name = match &event {
        DriverEvent::Connected { provider_cursor } => {
            if let Some(ProviderResumeCursor::Codex { thread_id }) = provider_cursor {
                writeln!(evidence, "native_thread_id={thread_id}")?;
            }
            "connected"
        }
        DriverEvent::TurnStarted => "turn_started",
        DriverEvent::TurnFinished { success: true, .. } => "turn_completed",
        DriverEvent::TurnFinished { success: false, .. } => "turn_failed",
        DriverEvent::TurnInterrupted => "turn_interrupted",
        DriverEvent::SteerAccepted { .. } => "steer_accepted",
        DriverEvent::SteerRejected { .. } => "steer_rejected",
        DriverEvent::ProcessExited => "process_exited",
        DriverEvent::Error(_) => "provider_error",
        DriverEvent::TextDelta(_) => "text_delta",
        DriverEvent::Activity { .. } | DriverEvent::RichActivity(_) => "activity",
        DriverEvent::BackgroundWork(_) => "background_work",
        _ => "other",
    };
    writeln!(evidence, "{name}")?;
    evidence.flush()?;
    anyhow::ensure!(
        !matches!(event, DriverEvent::Error(_)),
        "provider error; inspect native thread read-only"
    );
    Ok(event)
}

fn completed_turn(
    events: &Receiver<DriverEvent>,
    marker: &str,
    forbidden: &str,
    require_collaboration: bool,
    evidence: &mut std::fs::File,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut starts = 0;
    let mut text = String::new();
    let mut collaboration = false;
    loop {
        match receive(events, deadline, evidence)? {
            DriverEvent::TurnStarted => {
                starts += 1;
                anyhow::ensure!(starts == 1, "child start leaked into main turn");
            }
            DriverEvent::TextDelta(delta) => text.push_str(&delta),
            DriverEvent::BackgroundWork(BackgroundWorkEvent::Upsert(item)) => {
                collaboration |= item.key.kind == BackgroundWorkKind::Subagent;
            }
            DriverEvent::RichActivity(item) => {
                collaboration |=
                    item.complete && !item.failed && item.title.eq_ignore_ascii_case("wait");
            }
            DriverEvent::TurnFinished { success, .. } => {
                anyhow::ensure!(
                    success && starts == 1,
                    "main turn did not complete successfully"
                );
                anyhow::ensure!(text.contains(marker), "main completion marker missing");
                anyhow::ensure!(
                    !text.contains(forbidden),
                    "child text leaked into main transcript"
                );
                anyhow::ensure!(
                    !require_collaboration || collaboration,
                    "native collaboration evidence missing"
                );
                return Ok(());
            }
            DriverEvent::TurnInterrupted | DriverEvent::ProcessExited => {
                anyhow::bail!("unexpected turn/process termination")
            }
            _ => {}
        }
    }
}

fn native_trace(thread_id: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    fn locate(root: &std::path::Path, suffix: &str, depth: usize) -> Option<PathBuf> {
        for entry in std::fs::read_dir(root).ok()?.flatten() {
            let kind = entry.file_type().ok()?;
            if kind.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
                return Some(entry.path());
            }
            if kind.is_dir() && depth > 0 {
                if let Some(path) = locate(&entry.path(), suffix, depth - 1) {
                    return Some(path);
                }
            }
        }
        None
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".codex")))
        .ok_or_else(|| anyhow::anyhow!("native Codex home unavailable"))?;
    let path = locate(&home.join("sessions"), &format!("{thread_id}.jsonl"), 3)
        .ok_or_else(|| anyhow::anyhow!("exact native thread trace unavailable"))?;
    std::fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn verify_native_collaboration(
    thread_id: &str,
    evidence: &mut std::fs::File,
) -> anyhow::Result<()> {
    let trace = native_trace(thread_id)?;
    let calls = trace
        .iter()
        .filter_map(|record| {
            let payload = record.get("payload")?;
            (payload.get("type")?.as_str()? == "function_call").then_some(payload)
        })
        .collect::<Vec<_>>();
    let spawns = calls
        .iter()
        .filter(|call| call["name"] == "spawn_agent")
        .collect::<Vec<_>>();
    anyhow::ensure!(spawns.len() == 1, "expected exactly one native spawn call");
    let arguments: serde_json::Value =
        serde_json::from_str(spawns[0]["arguments"].as_str().unwrap_or(""))?;
    anyhow::ensure!(
        arguments["model"] == "gpt-6-astra",
        "child spawn did not select Astra"
    );
    anyhow::ensure!(
        calls.iter().any(|call| call["name"] == "wait_agent"),
        "native wait call missing"
    );
    let child = trace
        .iter()
        .filter_map(|record| record.pointer("/payload/item"))
        .find(|item| item["type"] == "SubAgentActivity" && item["kind"] == "completed")
        .and_then(|item| item["agent_thread_id"].as_str())
        .ok_or_else(|| anyhow::anyhow!("native child completion evidence missing"))?;
    let child_trace = native_trace(child)?;
    let models = child_trace
        .iter()
        .filter(|record| record["type"] == "turn_context")
        .filter_map(|record| {
            record
                .pointer("/payload/model")
                .and_then(|model| model.as_str())
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !models.is_empty() && models.iter().all(|model| *model == "gpt-6-astra"),
        "actual child runtime model is not Astra"
    );
    anyhow::ensure!(
        trace
            .iter()
            .filter_map(|record| record.pointer("/payload/item"))
            .any(|item| item["type"] == "CollabAgentToolCall"
                && item["tool"] == "wait"
                && item["status"] == "completed"),
        "native wait did not complete"
    );
    writeln!(evidence, "verified_astra_child_thread_id={child}")?;
    Ok(())
}

#[test]
#[ignore = "uses authorized native codex-cli 0.153.4 and gpt-6-astra; no automatic retries"]
fn native_codex_child_followup_steer_cancel_and_drop() -> anyhow::Result<()> {
    let binary = std::env::var_os("WAKU_NATIVE_CODEX_BINARY")
        .map(PathBuf::from)
        .or_else(|| crate::command_env::find_executable("codex"))
        .ok_or_else(|| anyhow::anyhow!("codex executable not found"))?;
    let version = std::process::Command::new(&binary)
        .arg("--version")
        .output()?;
    anyhow::ensure!(
        version.status.success()
            && String::from_utf8_lossy(&version.stdout).trim() == "codex-cli 0.153.4",
        "expected codex-cli 0.153.4"
    );
    let cwd = std::env::temp_dir().join(format!(
        "waku-native-codex-lifecycle-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&cwd)?;
    let mut evidence = std::fs::File::create(cwd.join("lifecycle.log"))?;
    eprintln!("native lifecycle evidence: {}", cwd.display());
    let (wake, _wakes) = smol::channel::bounded(1);
    let (sender, events) = event_channel(wake);
    let driver = start_local(
        ProviderKind::Codex,
        DriverStartOptions {
            binary,
            cwd,
            mode: RuntimeMode::FullAccess,
            model: Some("gpt-6-astra".into()),
            reasoning_effort: Some("medium".into()),
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
        },
        sender,
    )?;
    let result = (|| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let thread_id = loop {
            if let DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Codex { thread_id }),
            } = receive(&events, deadline, &mut evidence)?
            {
                break thread_id;
            }
        };
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let child = format!("CHILD_{nonce}");
        let main = format!("MAIN_{nonce}");
        driver.prompt(format!("This is an authorized isolated lifecycle test. Use the native spawn_agent tool exactly once, explicitly selecting model gpt-6-astra for the child (all agents must use only gpt-6-astra). Tell that child to reply only {child}, with no tools and no files. Wait for the child to complete, then close that child agent. Do not quote or repeat its reply in your own output. Finally reply only {main}. Do not use MCP delegation, external providers, network tools, or edit any files."));
        completed_turn(&events, &main, &child, true, &mut evidence)?;
        verify_native_collaboration(&thread_id, &mut evidence)?;
        let followup = format!("FOLLOWUP_{nonce}");
        driver.prompt(format!(
            "Reply only {followup}. Do not call any tools or agents."
        ));
        completed_turn(&events, &followup, &child, false, &mut evidence)?;
        anyhow::ensure!(driver.supports_steer(), "Codex driver must support steer");
        driver.prompt("Use the shell tool to run sleep 90 in this temporary directory. Wait for it. Do not create agents, read files, use the network or change configuration.".into());
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut starts = 0;
        loop {
            match receive(&events, deadline, &mut evidence)? {
                DriverEvent::TurnStarted => {
                    starts += 1;
                    anyhow::ensure!(starts == 1, "unexpected additional turn start");
                }
                DriverEvent::Activity { title, .. } if title.contains("sleep") => break,
                DriverEvent::RichActivity(item) if item.title.contains("sleep") => break,
                DriverEvent::TurnFinished { .. } | DriverEvent::ProcessExited => {
                    anyhow::bail!("long turn ended before control probe")
                }
                _ => {}
            }
        }
        driver.steer(
            "Keep waiting for the same sleep command; do not start a new command or agent.".into(),
        );
        loop {
            match receive(&events, deadline, &mut evidence)? {
                DriverEvent::SteerAccepted { .. } => break,
                DriverEvent::SteerRejected { .. }
                | DriverEvent::TurnFinished { .. }
                | DriverEvent::ProcessExited => {
                    anyhow::bail!("steer was not accepted into active main turn")
                }
                _ => {}
            }
        }
        driver.cancel();
        loop {
            match receive(&events, deadline, &mut evidence)? {
                DriverEvent::TurnInterrupted => break,
                DriverEvent::TurnFinished { .. } | DriverEvent::ProcessExited => {
                    anyhow::bail!("cancel did not confirm interruption")
                }
                _ => {}
            }
        }
        Ok(())
    })();
    drop(driver);
    let deadline = Instant::now() + Duration::from_secs(30);
    let cleanup = loop {
        match receive(&events, deadline, &mut evidence) {
            Ok(DriverEvent::ProcessExited) => break Ok(()),
            Ok(_) => {}
            Err(error) => break Err(error.context("drop did not confirm process exit")),
        }
    };
    result?;
    cleanup
}
