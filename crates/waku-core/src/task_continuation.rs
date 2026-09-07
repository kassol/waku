//! Explicit user continuation preserves the completed source and reuses durable execution.
use super::steward_decision::{manager_instruction, manager_instruction_pending};
use super::*;
use crate::model::{ChildContinuation, InputDeliveryState, MessageRole, StewardLifecycleOperation};

impl WakuBackend {
    pub(super) fn lifecycle_operation(
        &self,
        manager: Uuid,
        operation: StewardLifecycleOperation,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        match operation {
            StewardLifecycleOperation::Continue {
                session_id,
                completion_id,
                operation_id,
                instruction,
                authority_message_id,
            } => self.continue_child(
                manager,
                session_id,
                completion_id,
                operation_id,
                instruction,
                Some(authority_message_id),
                events,
            ),
            StewardLifecycleOperation::Status {
                session_id,
                operation_id,
            } => self.continuation_status(manager, session_id, operation_id, events),
            complete => self.complete_child(manager, complete, events),
        }
    }

    pub(super) fn continuation_status(
        &self,
        manager: Uuid,
        child: Uuid,
        id: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        events.ensure_steward_active()?;
        let mut state = self.task_state.lock();
        let (session, _) = self.authorized_child(&mut state, manager, child, events)?;
        let mut continuation = session
            .continuations
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("Continuation is unavailable"))?;
        if let Some(delivery) = session.input_deliveries.iter().find(|d| d.id == id) {
            continuation.state = delivery.state;
            continuation.reason = delivery.reason.clone();
        }
        if continuation.result_session_id != Some(child) {
            if let Some(creation) = self
                .task_store
                .creation_by_key(manager, &format!("continuation:{id}"))?
            {
                continuation.result_session_id = creation.session.as_ref().map(|s| s.id);
                match creation.outcome {
                    Some(crate::protocol::ResponseOutcome::Ok {
                        payload: ResponsePayload::SessionCreated { .. },
                    }) => {
                        continuation.state = InputDeliveryState::Received;
                        continuation.reason = None;
                    }
                    Some(crate::protocol::ResponseOutcome::Ok {
                        payload:
                            ResponsePayload::SessionCreationFailed {
                                uncertain, error, ..
                            },
                    }) => {
                        continuation.state = if uncertain || creation.workspace_path.is_some() {
                            InputDeliveryState::Uncertain
                        } else {
                            InputDeliveryState::Failed
                        };
                        continuation.reason = Some(error);
                    }
                    _ => {}
                }
            }
        }
        let current = state.sessions.iter_mut().find(|s| s.id == child).unwrap();
        let before = current.clone();
        if current.continuations.iter().find(|c| c.id == id) != Some(&continuation) {
            *current
                .continuations
                .iter_mut()
                .find(|c| c.id == id)
                .unwrap() = continuation.clone();
            current.lifecycle_revision += 1;
            state.mark_session_dirty(child);
            if let Err(error) = self.task_store.save(&mut state) {
                *state.sessions.iter_mut().find(|s| s.id == child).unwrap() = before;
                return Err(error.into());
            }
        }
        let session = state
            .sessions
            .iter()
            .find(|s| s.id == child)
            .unwrap()
            .clone();
        Ok(ResponsePayload::LifecycleContinued {
            session,
            continuation,
        })
    }

    pub(super) fn validate_continuation_authority(
        &self,
        state: &mut PersistedState,
        child: &AgentSession,
        record: &ChildContinuation,
    ) -> anyhow::Result<()> {
        let parent = state
            .sessions
            .iter_mut()
            .find(|s| s.id == record.manager_session_id)
            .ok_or_else(|| anyhow!("Continuation manager unavailable"))?;
        self.task_store.hydrate(parent)?;
        if parent.parent_session_id.is_some()
            || parent.archived
            || child.parent_session_id != Some(parent.id)
            || child.project_id != parent.project_id
        {
            bail!(
                "Continue this task through its main manager; nested assignments are not user authority"
            );
        }
        if manager_instruction_pending(parent)
            || manager_instruction(parent).map(|m| m.id) != Some(record.authority_message_id)
        {
            bail!("Continuation user instruction changed or has not been received");
        }
        let message = manager_instruction(parent).unwrap();
        if message
            .display_content
            .as_deref()
            .unwrap_or(&message.content)
            != record.instruction
        {
            bail!("Continuation must carry the explicit user instruction unchanged");
        }
        let completion = child
            .completions
            .iter()
            .find(|c| c.id == record.source_completion_id)
            .ok_or_else(|| anyhow!("Source completion unavailable"))?;
        let summary = parent
            .messages
            .iter()
            .position(|m| m.id == completion.summary_message_id)
            .ok_or_else(|| anyhow!("Saved source summary unavailable"))?;
        let authority = parent
            .messages
            .iter()
            .position(|m| m.id == record.authority_message_id)
            .ok_or_else(|| anyhow!("User instruction unavailable"))?;
        if authority <= summary {
            bail!("Continuing old work requires a new user instruction after its completion");
        }
        Ok(())
    }

    pub(super) fn continue_child(
        &self,
        manager: Uuid,
        child_id: Uuid,
        completion_id: Uuid,
        operation_id: Uuid,
        instruction: String,
        authority: Option<Uuid>,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        if instruction.trim().is_empty() || instruction.len() > 20000 {
            bail!("Continuation instruction must contain 1..20000 bytes");
        }
        events.ensure_steward_active()?;
        self.ensure_accepting_work()?;
        let (record, child) = {
            let _child_guard = events.reserve_steward_target(child_id)?;
            let _manager_guard = events.reserve_steward_target(manager)?;
            let _workspace = self.workspace_start_gate.lock();
            let mut state = self.task_state.lock();
            let (child, _) = self.authorized_child(&mut state, manager, child_id, events)?;
            if let Some(existing) = child.continuations.iter().find(|c| {
                c.id == operation_id
                    || (c.source_completion_id == completion_id
                        && c.state != InputDeliveryState::Failed)
            }) {
                if existing.source_completion_id != completion_id
                    || existing.instruction != instruction
                    || existing.manager_session_id != manager
                    || authority.is_some_and(|id| id != existing.authority_message_id)
                {
                    bail!("Source completion already has a different continuation");
                }
                let id = existing.id;
                drop(state);
                return self.continuation_status(manager, child_id, id, events);
            }
            if state.sessions.iter().any(|s| {
                s.continuations.iter().any(|c| c.id == operation_id)
                    || s.input_deliveries.iter().any(|d| d.id == operation_id)
            }) {
                bail!("Operation identity is already used by another task input");
            }
            if !child.archived || child.completions.last().map(|c| c.id) != Some(completion_id) {
                bail!("Only the latest archived completion can be continued");
            }
            self.check_lifecycle_ready(&mut state, child_id)?;
            let retained = self.continuation_workspace(&mut state, manager, &child)?;
            let parent = state.sessions.iter_mut().find(|s| s.id == manager).unwrap();
            self.task_store.hydrate(parent)?;
            if parent.parent_session_id.is_some() || parent.archived {
                bail!("Explicit continuation requires the active main manager");
            }
            let before_parent = parent.clone();
            let authority_message_id = if let Some(id) = authority {
                id
            } else {
                if manager_instruction_pending(parent) {
                    bail!("Wait for the current user instruction receipt");
                }
                parent.push_message(MessageRole::User, instruction.clone());
                parent.messages.last().unwrap().id
            };
            let record = ChildContinuation {
                id: operation_id,
                source_completion_id: completion_id,
                manager_session_id: manager,
                authority_message_id,
                instruction: instruction.clone(),
                result_session_id: retained.then_some(child_id),
                state: InputDeliveryState::Accepted,
                reason: None,
                created_at: crate::model::unix_time(),
            };
            if let Err(error) = self.validate_continuation_authority(&mut state, &child, &record) {
                *state.sessions.iter_mut().find(|s| s.id == manager).unwrap() = before_parent;
                return Err(error);
            }
            let source = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            source.continuations.push(record.clone());
            source.lifecycle_revision += 1;
            state.mark_session_dirty(child_id);
            state.mark_session_dirty(manager);
            if let Err(error) = self.task_store.save(&mut state) {
                *state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == child_id)
                    .unwrap() = child;
                *state.sessions.iter_mut().find(|s| s.id == manager).unwrap() = before_parent;
                return Err(error.into());
            }
            (record, child)
        };
        events.input_state_changed();
        let result = if record.result_session_id == Some(child_id) {
            self.deliver_authorized_input(
                manager,
                child_id,
                instruction,
                None,
                Some(operation_id),
                events,
            )
        } else {
            let command = Command::CreateSession {
                provider: child.provider,
                prompt: replacement_prompt(&child, &record)?,
                model: child.model.clone(),
                title: Some(child.title.clone()),
                runtime_mode: Some(child.runtime_mode),
                idempotency_key: Some(format!("continuation:{operation_id}")),
                workspace: crate::protocol::CreationWorkspace::Worktree,
                dependencies: Vec::new(),
            };
            self.create_session(manager, command, events.clone())
        };
        let mut state = self.task_state.lock();
        let creation_claimed = record.result_session_id.is_none()
            && self
                .task_store
                .creation_by_key(manager, &format!("continuation:{operation_id}"))?
                .is_some();
        let source = state
            .sessions
            .iter_mut()
            .find(|s| s.id == child_id)
            .unwrap();
        let input_claimed = source.input_deliveries.iter().any(|d| d.id == operation_id);
        let latest_input = source
            .input_deliveries
            .iter()
            .find(|d| d.id == operation_id)
            .cloned();
        let before = source.clone();
        let saved = source
            .continuations
            .iter_mut()
            .find(|c| c.id == operation_id)
            .unwrap();
        match result {
            Ok(ResponsePayload::ChildPromptAccepted {
                delivery: Some(delivery),
                ..
            }) => {
                saved.state = delivery.state;
                saved.reason = delivery.reason;
            }
            Ok(ResponsePayload::SessionCreated { session, .. }) => {
                saved.result_session_id = Some(session.id);
                saved.state = InputDeliveryState::Received;
            }
            Ok(ResponsePayload::SessionCreationFailed {
                session_id,
                uncertain,
                error,
                ..
            }) => {
                saved.result_session_id = session_id;
                saved.state = if uncertain || session_id.is_some() {
                    InputDeliveryState::Uncertain
                } else {
                    InputDeliveryState::Failed
                };
                saved.reason = Some(error);
            }
            Err(error) => {
                saved.state = if input_claimed || creation_claimed {
                    InputDeliveryState::Uncertain
                } else {
                    InputDeliveryState::Failed
                };
                saved.reason = Some(error.to_string());
            }
            _ => {
                saved.state = InputDeliveryState::Uncertain;
                saved.reason =
                    Some("Continuation outcome needs inspection; no automatic resend".into());
            }
        }
        if let Some(delivery) = latest_input {
            saved.state = delivery.state;
            saved.reason = delivery.reason;
        }
        let continuation = saved.clone();
        source.lifecycle_revision += 1;
        let session = source.clone();
        state.mark_session_dirty(child_id);
        if let Err(error) = self.task_store.save(&mut state) {
            *state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap() = before;
            return Err(error.into());
        }
        drop(state);
        events.input_state_changed();
        Ok(ResponsePayload::LifecycleContinued {
            session,
            continuation,
        })
    }
}

pub(super) fn recover(store: &StateStore, state: &mut PersistedState) -> anyhow::Result<()> {
    let ids = state
        .sessions
        .iter()
        .filter(|s| {
            s.continuations.iter().any(|c| {
                matches!(
                    c.state,
                    InputDeliveryState::Accepted | InputDeliveryState::Queued
                )
            })
        })
        .map(|s| s.id)
        .collect::<Vec<_>>();
    for id in ids {
        let source = state.sessions.iter_mut().find(|s| s.id == id).unwrap();
        store.hydrate(source)?;
        for record in &mut source.continuations {
            if matches!(
                record.state,
                InputDeliveryState::Accepted | InputDeliveryState::Queued
            ) {
                record.state = InputDeliveryState::Uncertain;
                record.reason = Some("Daemon restarted during continuation; inspect its saved input or creation status, never resend automatically".into());
            }
        }
        source.lifecycle_revision += 1;
        state.mark_session_dirty(id);
    }
    store.save(state)?;
    Ok(())
}

pub(super) fn replacement_prompt(
    child: &AgentSession,
    record: &ChildContinuation,
) -> anyhow::Result<String> {
    let completion = child
        .completions
        .iter()
        .find(|c| c.id == record.source_completion_id)
        .ok_or_else(|| anyhow!("Source completion unavailable"))?;
    let summary = &completion.summary;
    Ok(format!(
        "继续已归档任务。原任务：{}；原摘要：{}。\n目标：{}\n已完成成果：{}\n关键决定：{}\n验证：{}\n待处理：{}\n\n用户本次明确指令：\n{}",
        child.id,
        completion.summary_message_id,
        summary.goal,
        summary.result,
        summary.decisions.as_deref().unwrap_or("未提供"),
        summary.verification.as_deref().unwrap_or("未提供"),
        summary.unresolved.as_deref().unwrap_or("未提供"),
        record.instruction
    ))
}
