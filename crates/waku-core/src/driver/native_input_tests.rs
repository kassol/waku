//! Opt-in compatibility checks for tracked input, using fresh native sessions.
use super::*;
use crate::model::{InputConfirmation, InputDeliveryState};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[test]
#[ignore = "uses the installed authenticated Codex and a new Astra session"]
fn native_codex_tracked_input() -> anyhow::Result<()> {
    tracked_input(ProviderKind::Codex, "codex")
}

#[test]
#[ignore = "uses the installed authenticated Claude and its configured model"]
fn native_claude_tracked_input() -> anyhow::Result<()> {
    tracked_input(ProviderKind::Claude, "claude")
}

fn tracked_input(provider: ProviderKind, executable: &str) -> anyhow::Result<()> {
    let binary = crate::command_env::find_executable(executable)
        .ok_or_else(|| anyhow::anyhow!("{executable} executable not found"))?;
    let cwd = std::env::temp_dir().join(format!("waku-native-input-{}", Uuid::new_v4()));
    std::fs::create_dir(&cwd)?;
    let (sender, events) = test_event_channel();
    let driver = start_local(
        provider,
        DriverStartOptions {
            binary,
            cwd: cwd.clone(),
            mode: RuntimeMode::FullAccess,
            model: (provider == ProviderKind::Codex).then(|| "gpt-6-astra".into()),
            reasoning_effort: (provider == ProviderKind::Codex).then(|| "medium".into()),
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
        },
        sender,
    )?;
    let result = (|| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(180);
        let next = || -> anyhow::Result<DriverEvent> {
            Ok(events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?)
        };
        loop {
            match next()? {
                DriverEvent::Connected { .. } => break,
                DriverEvent::Error(_) | DriverEvent::ProcessExited => {
                    anyhow::bail!("native startup failed")
                }
                _ => {}
            }
        }
        let prompt_id = Uuid::new_v4();
        let steer_id = Uuid::new_v4();
        let marker = format!("RECEIVED_{}", steer_id.simple());
        driver.deliver_input("This is an authorized isolated transport check. Use the shell tool exactly once to run sleep 20 in this temporary directory, then reply DONE. Do not use agents, MCP, network tools, or read or change files or configuration. Keep waiting for the command before replying.".into(), prompt_id, false)?;
        let mut receipts = Vec::new();
        let mut starts = 0;
        let mut steered = false;
        let mut text = String::new();
        loop {
            match next()? {
                DriverEvent::TurnStarted => starts += 1,
                DriverEvent::InputDeliveryOutcome(outcome) => {
                    anyhow::ensure!(
                        outcome.state == InputDeliveryState::Received,
                        "native input was not confirmed"
                    );
                    let expected = if provider == ProviderKind::Codex {
                        InputConfirmation::Provider
                    } else {
                        InputConfirmation::Transport
                    };
                    anyhow::ensure!(
                        outcome.confirmation == Some(expected),
                        "wrong confirmation level"
                    );
                    anyhow::ensure!(
                        !receipts.contains(&outcome.id),
                        "duplicate native confirmation"
                    );
                    receipts.push(outcome.id);
                }
                DriverEvent::RichActivity(ref item) if !item.complete && !steered => {
                    driver.deliver_input(format!("After the current sleep finishes, reply only {marker}. Do not start additional tools or agents."), steer_id, true)?;
                    steered = true;
                }
                DriverEvent::Activity { ref title, .. } if title.contains("sleep") && !steered => {
                    driver.deliver_input(format!("After the current sleep finishes, reply only {marker}. Do not start additional tools or agents."), steer_id, true)?;
                    steered = true;
                }
                DriverEvent::TextDelta(delta) => text.push_str(&delta),
                DriverEvent::TurnFinished { success, .. } => {
                    anyhow::ensure!(
                        success && starts == 1 && steered,
                        "tracked input changed or failed the active turn: success={success}, starts={starts}, steered={steered}"
                    );
                    anyhow::ensure!(
                        receipts == [prompt_id, steer_id],
                        "missing tracked input confirmations"
                    );
                    anyhow::ensure!(
                        text.contains(&marker),
                        "native turn did not include the steered reply"
                    );
                    break;
                }
                DriverEvent::Error(_)
                | DriverEvent::ProcessExited
                | DriverEvent::TurnInterrupted => anyhow::bail!("native turn failed"),
                _ => {}
            }
        }
        eprintln!("{executable}: prompt and steer confirmed; one turn; steered reply present");
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
    anyhow::ensure!(
        exited,
        "owned native process did not exit; temporary directory {}",
        cwd.display()
    );
    std::fs::remove_dir_all(&cwd)?;
    result
}
