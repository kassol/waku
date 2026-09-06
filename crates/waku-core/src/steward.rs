//! Scoped operations over daemon-owned, hydrated child sessions.
use super::*;
use std::time::Duration;
use waku_protocol::{ChildSessionSummary, ChildTurnSummary, ChildWaitingReason, StewardQuery};

impl WakuBackend {
    pub(super) fn steward_query(
        &self,
        parent_id: Uuid,
        query: &StewardQuery,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let (targets, wait_ms) = match query {
            StewardQuery::ListSessions {} => (None, 0),
            StewardQuery::Result {
                session_id,
                max_chars,
                ..
            } => {
                if max_chars.is_some_and(|limit| limit == 0 || limit > 100_000) {
                    bail!("max_chars must be within 1..100000");
                }
                (Some(std::slice::from_ref(session_id)), 0)
            }
            StewardQuery::Results { session_ids, handled, max_chars } => {
                if session_ids.is_empty() || session_ids.len() > 128 || handled.len() > 128 {
                    bail!("session_ids must contain 1..128 direct children; handled accepts at most 128 receipts");
                }
                if max_chars.is_some_and(|limit| limit == 0 || limit > 100_000) {
                    bail!("max_chars must be within 1..100000");
                }
                (Some(session_ids.as_slice()), 0)
            }
            StewardQuery::Status {
                session_ids,
                wait_ms,
            } => {
                if session_ids.is_empty() || session_ids.len() > 128 {
                    bail!("session_ids must contain 1..128 direct children");
                }
                if *wait_ms > 60_000 {
                    bail!("wait_ms must be within 0..60000");
                }
                (Some(session_ids.as_slice()), *wait_ms)
            }
        };
        let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms);
        let mut baseline = None;
        loop {
            events.ensure_steward_active()?;
            if self.quitting.load(Ordering::Acquire) {
                bail!("daemon is shutting down");
            }
            let mut state = self.task_state.lock();
            let project_id = state
                .sessions
                .iter()
                .find(|session| session.id == parent_id)
                .ok_or_else(|| anyhow!("steward session is unavailable"))?
                .project_id;
            if events
                .scoped_project
                .is_some_and(|expected| expected != project_id)
                || !state
                    .projects
                    .iter()
                    .any(|project| project.id == project_id)
            {
                bail!("steward project is no longer available");
            }
            if let Some(ids) = targets {
                for id in ids {
                    if !state.sessions.iter().any(|s| {
                        s.id == *id
                            && s.parent_session_id == Some(parent_id)
                            && s.project_id == project_id
                    }) {
                        bail!("target is not a direct child in the steward project");
                    }
                }
            }
            if let StewardQuery::Results { session_ids, handled, max_chars } = query {
                let mut results = Vec::new();
                let mut remaining = max_chars.unwrap_or(20_000);
                for id in session_ids {
                    if results.iter().any(|r: &waku_protocol::ChildBatchResult| r.session.session_id == *id) {
                        continue;
                    }
                    let session = state.sessions.iter_mut().find(|s| s.id == *id).expect("authorized target exists");
                    self.task_store.hydrate(session)?;
                    let summary = self.child_summary(session);
                    let actionable = summary.turn.as_ref().is_some_and(|t| t.status != crate::model::TurnStatus::Running)
                        || !summary.waiting_for.is_empty() || summary.error.is_some();
                    let receipt = actionable.then(|| result_receipt(session)).transpose()?;
                    let was_handled = receipt.as_ref().is_some_and(|receipt| handled.contains(receipt));
                    let mut reply = None;
                    let mut reply_truncated = false;
                    if actionable && !was_handled {
                        if let ResponsePayload::ChildResult { reply:text, reply_truncated:truncated, .. } =
                            self.child_result(session, false, remaining) {
                            remaining = remaining.saturating_sub(text.chars().count());
                            reply = Some(text);
                            reply_truncated = truncated;
                        }
                    }
                    results.push(waku_protocol::ChildBatchResult {
                        session: summary, receipt, handled: was_handled, reply, reply_truncated,
                        workspace: (actionable && !was_handled).then(|| session.managed_workspace.clone()).flatten(),
                    });
                }
                drop(state);
                events.ensure_steward_active()?;
                return Ok(ResponsePayload::ChildResults { results });
            }
            if let StewardQuery::Result {
                session_id,
                include_transcript,
                max_chars,
            } = query
            {
                let session = state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == *session_id)
                    .expect("authorized target exists");
                self.task_store.hydrate(session)?;
                let result =
                    self.child_result(session, *include_transcript, max_chars.unwrap_or(20_000));
                drop(state);
                events.ensure_steward_active()?;
                return Ok(result);
            }
            let mut snapshots = Vec::new();
            for session in state.sessions.iter_mut().filter(|s| {
                s.parent_session_id == Some(parent_id)
                    && s.project_id == project_id
                    && targets.is_none_or(|ids| ids.contains(&s.id))
            }) {
                self.task_store.hydrate(session)?;
                snapshots.push((self.child_summary(session), session.runtime_event_cursor));
            }
            snapshots.sort_by_key(|(session, _)| (session.created_at, session.session_id));
            drop(state);
            events.ensure_steward_active()?;
            let timed_out = wait_ms > 0 && std::time::Instant::now() >= deadline;
            if wait_ms == 0 || timed_out || baseline.as_ref().is_some_and(|old| old != &snapshots) {
                let sessions = snapshots.into_iter().map(|(summary, _)| summary).collect();
                return Ok(match query {
                    StewardQuery::ListSessions {} => ResponsePayload::ChildSessions { sessions },
                    _ => ResponsePayload::ChildStatus {
                        sessions,
                        timed_out,
                    },
                });
            }
            baseline.get_or_insert(snapshots);
            std::thread::sleep(
                Duration::from_millis(50)
                    .min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
        }
    }

    pub(super) fn child_summary(&self, session: &AgentSession) -> ChildSessionSummary {
        let turn = session.turns.last().map(|turn| ChildTurnSummary {
            turn_id: turn.id,
            status: turn.status,
            started_at: turn.started_at,
            completed_at: turn.completed_at,
        });
        let mut waiting_for = Vec::new();
        if session.pending_permission.is_some() {
            waiting_for.push(ChildWaitingReason::Permission);
        }
        if session.pending_user_input.is_some() {
            waiting_for.push(ChildWaitingReason::UserInput);
        }
        let error = session
            .history_save_error
            .clone()
            .or_else(|| session.last_driver_error.clone());
        ChildSessionSummary {
            session_id: session.id,
            title: session.display_title().to_owned(),
            provider: session.provider,
            status: session.status,
            created_at: session.created_at,
            last_activity: session.updated_at,
            turn_open: session.active_turn_id().is_some(),
            turn,
            waiting_for,
            error,
        }
    }
}

impl WakuBackend {
    fn child_result(
        &self,
        session: &AgentSession,
        include_transcript: bool,
        max_chars: usize,
    ) -> ResponsePayload {
        let mut reply = BoundedText::new(max_chars);
        if let Some(turn) = session.turns.last() {
            for message in session.messages.iter().filter(|message| {
                message.turn_id == Some(turn.id)
                    && message.role == crate::model::MessageRole::Assistant
            }) {
                reply.paragraph(&message.content);
            }
        }
        let mut transcript = BoundedText::new(max_chars);
        if include_transcript {
            // Anchors count messages before the native activity block, matching transcript presentation.
            let mut blocks_after = vec![Vec::new(); session.messages.len() + 1];
            for block in &session.transcript_blocks {
                blocks_after[block.after_message.min(session.messages.len())].push(block);
            }
            for (index, blocks) in blocks_after.iter().enumerate() {
                if index > 0 {
                    let message = &session.messages[index - 1];
                    transcript.paragraph(match message.role {
                        crate::model::MessageRole::User => "User:",
                        crate::model::MessageRole::Assistant => "Assistant:",
                        crate::model::MessageRole::System => "System:",
                    });
                    transcript.push(" ");
                    transcript.push(&message.content);
                }
                for block in blocks {
                    for activity in &block.activities {
                        transcript.paragraph(&activity.title);
                        for text in [
                            activity.detail.as_deref(),
                            activity.arguments.as_deref(),
                            activity.output.as_deref(),
                            activity.reasoning.as_ref().map(|r| r.content.as_str()),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            transcript.paragraph(text);
                        }
                    }
                }
            }
        }
        ResponsePayload::ChildResult {
            session: self.child_summary(session),
            reply: reply.text,
            reply_truncated: reply.truncated,
            transcript: include_transcript.then_some(transcript.text),
            transcript_truncated: transcript.truncated,
        }
    }
}

/// Bounds the returned text without copying the full history or cutting UTF-8 characters.
struct BoundedText {
    text: String,
    remaining: usize,
    truncated: bool,
}
impl BoundedText {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            remaining: limit,
            truncated: false,
        }
    }
    fn push(&mut self, text: &str) {
        let mut chars = text.chars();
        for ch in chars.by_ref().take(self.remaining) {
            self.text.push(ch);
            self.remaining -= 1;
        }
        if chars.next().is_some() {
            self.truncated = true;
        }
    }
    fn paragraph(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.push("\n\n");
        }
        self.push(text);
    }
}

impl WakuBackend {
    pub(super) fn authorized_child(
        &self,
        state: &mut PersistedState,
        parent_id: Uuid,
        child_id: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<(AgentSession, PathBuf)> {
        let parent = state
            .sessions
            .iter()
            .find(|s| s.id == parent_id)
            .ok_or_else(|| anyhow!("steward session is unavailable"))?;
        let project = state
            .projects
            .iter()
            .find(|p| p.id == parent.project_id)
            .ok_or_else(|| anyhow!("steward project is unavailable"))?;
        if events.scoped_project.is_some_and(|id| id != project.id) {
            bail!("steward project is no longer available");
        }
        let project_id = project.id;
        let project_path = project.path.clone();
        let child = state
            .sessions
            .iter_mut()
            .find(|s| {
                s.id == child_id
                    && s.parent_session_id == Some(parent_id)
                    && s.project_id == project_id
            })
            .ok_or_else(|| anyhow!("target is not a direct child in the steward project"))?;
        self.task_store.hydrate(child)?;
        if !matches!(child.provider, ProviderKind::Claude | ProviderKind::Codex) {
            bail!("child provider is unsupported");
        }
        Ok((child.clone(), project_path))
    }

    pub(super) fn steward_prompt(
        &self, parent_id: Uuid, child_id: Uuid, prompt: String,
        delivery_id: Option<Uuid>, events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        events.ensure_steward_active()?;
        self.authorized_child(&mut self.task_state.lock(), parent_id, child_id, events)?;
        self.deliver_authorized_input(parent_id, child_id, prompt, None, delivery_id, events)
    }

    pub(super) fn send_saved_steward_turn(
        &self,
        child: AgentSession,
        project_path: PathBuf,
        prompt: String,
        turn_id: Uuid,
        message_id: Uuid,
        delivery_id: Option<Uuid>,
        events: &EventSink,
    ) -> anyhow::Result<()> {
        let child_id = child.id;
        let active = self.sessions.lock().get(&child_id).map(|(id, _)| *id);
        let runtime_id = active.unwrap_or_else(Uuid::new_v4);
        let child_events = if active.is_some() {
            events.child_sink(child_id, runtime_id)
        } else {
            events.begin_child(child_id, runtime_id).0
        };
        let send = || -> anyhow::Result<()> {
            self.ensure_accepting_work()?;
            events.ensure_steward_active()?;
            if active.is_none() {
                self.handle_accepted(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: child_id,
                        runtime_id,
                        command: Command::Start {
                            options: crate::WireDriverStartOptions {
                                provider: serde_json::to_value(child.provider)?
                                    .as_str()
                                    .unwrap()
                                    .into(),
                                binary: self.provider_binary(child.provider)?,
                                cwd: child.workspace.path().unwrap_or(&project_path).to_owned(),
                                mode: serde_json::to_value(child.runtime_mode)?
                                    .as_str()
                                    .unwrap()
                                    .into(),
                                model: child.model,
                                reasoning_effort: child.reasoning_effort,
                                service_tier: child.service_tier,
                                context_window: child.context_window,
                                agent_preset: child.agent_preset,
                                computer_use_enabled: false,
                                provider_cursor: child
                                    .provider_cursor
                                    .map(serde_json::to_value)
                                    .transpose()?,
                            },
                        },
                    },
                    child_events.clone(),
                )?;
            }
            self.ensure_accepting_work()?;
            if let Some(id) = delivery_id {
                let delivery = child.input_deliveries.iter().find(|d| d.id == id).cloned()
                    .ok_or_else(|| anyhow!("saved input is unavailable"))?;
                child_events.send(event_to_wire(DriverEvent::InputDeliveryChanged(delivery))?)?;
                child_events.send(event_to_wire(DriverEvent::PromptSubmitted {
                    message: prompt.clone(), turn_id, message_id,
                })?)?;
                child_events.send(event_to_wire(DriverEvent::InputDeliveryOutcome(crate::model::InputDeliveryOutcome {
                    id, state: crate::model::InputDeliveryState::Uncertain, confirmation: None,
                    reason: Some("Awaiting provider confirmation; do not resend automatically".into()),
                }))?)?;
                let driver = self.sessions.lock().get(&child_id).map(|(_, driver)| driver.clone())
                    .ok_or_else(|| anyhow!("provider runtime is unavailable"))?;
                driver.deliver_input(prompt, id, false)?;
            } else {
            self.handle_accepted(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: child_id,
                    runtime_id,
                    command: Command::Prompt {
                        prompt,
                        turn_id: Some(turn_id),
                        message_id: Some(message_id),
                    },
                },
                child_events.clone(),
            )?;
            }
            Ok(())
        };
        if let Err(error) = send() {
            if let Some(id) = delivery_id {
                child_events.send(event_to_wire(DriverEvent::InputDeliveryOutcome(crate::model::InputDeliveryOutcome {
                    id, state: crate::model::InputDeliveryState::Failed, confirmation: None, reason: Some(error.to_string()),
                }))?)?;
            }

            child_events.send_batch(vec![
                event_to_wire(DriverEvent::Error(format!(
                    "could not run saved turn {turn_id}: {error:#}"
                )))?,
                event_to_wire(DriverEvent::TurnFinished {
                    success: false,
                    summary: None,
                })?,
            ])?;
        }
        Ok(())
    }

    pub(super) fn steward_cancel(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        events.ensure_steward_active()?;
        let _operation = events.reserve_steward_target(child_id)?;
        let child = self
            .authorized_child(&mut self.task_state.lock(), parent_id, child_id, events)?
            .0;
        let accepted = child.active_turn_id().is_some();
        if accepted && child.cancellation_requested_turn_id != child.active_turn_id() {
            let (runtime_id, driver) = self
                .sessions
                .lock()
                .get(&child_id)
                .cloned()
                .ok_or_else(|| anyhow!("child runtime is unavailable; stopping is unconfirmed"))?;
            let child_events = events.child_sink(child_id, runtime_id);
            let saved = child_events.send(event_to_wire(DriverEvent::CancelRequested)?);
            handle_driver_command(&driver, Command::Cancel, saved)?;
        }
        let child = self
            .authorized_child(&mut self.task_state.lock(), parent_id, child_id, events)?
            .0;
        Ok(ResponsePayload::ChildCancel {
            stopped: child.active_turn_id().is_none(),
            accepted,
            session: self.child_summary(&child),
        })
    }
}

/// Hash full result inputs, independently of display truncation. Late output, changed questions,
/// failures or fixed-commit evidence must invalidate an older handled receipt.
fn result_receipt(session: &AgentSession) -> anyhow::Result<waku_protocol::ChildResultReceipt> {
    use sha2::{Digest, Sha256};
    struct Writer(Sha256);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let turn = session.turns.last();
    let turn_id = turn.map(|t| t.id);
    let mut digest = Writer(Sha256::new());
    serde_json::to_writer(&mut digest, &(session.id, turn,
        &session.pending_permission, &session.pending_user_input,
        &session.history_save_error, &session.last_driver_error, &session.managed_workspace))?;
    for message in session.messages.iter().filter(|m| m.turn_id == turn_id) {
        serde_json::to_writer(&mut digest, message)?;
    }
    for block in session.transcript_blocks.iter().filter(|b| b.turn_id == turn_id) {
        serde_json::to_writer(&mut digest, block)?;
    }
    Ok(waku_protocol::ChildResultReceipt {
        session_id: session.id, turn_id,
        snapshot: format!("v1:{:x}", digest.0.finalize()),
    })
}
