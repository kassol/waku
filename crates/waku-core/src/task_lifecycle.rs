//! Explicit acceptance owns archival; provider idleness alone never completes a task.
use super::*;
use crate::model::{
    ChildCompletion, ChildCompletionSummary, CompletionDisposition, DecisionState,
    InputDeliveryState, StewardLifecycleOperation, TurnStatus,
};

pub(super) fn has_unsettled_decisions(session: &AgentSession) -> bool {
    session.continuations.iter().any(|c| matches!(c.state, InputDeliveryState::Accepted | InputDeliveryState::Queued | InputDeliveryState::Uncertain)) || session.decision_requests.iter().any(|request| {
        matches!(
            request.state,
            DecisionState::WaitingManager
                | DecisionState::WaitingUser
                | DecisionState::PendingReceipt
        ) || request
            .native
            .as_ref()
            .and_then(|native| native.outcome.as_ref())
            .is_some_and(|outcome| {
                matches!(
                    outcome.state,
                    InputDeliveryState::Accepted | InputDeliveryState::Uncertain
                )
            })
    })
}

impl WakuBackend {
    pub(super) fn ensure_session_writable(&self, id: Uuid) -> anyhow::Result<()> {
        if self
            .task_state
            .lock()
            .sessions
            .iter()
            .any(|s| s.id == id && s.archived)
        {
            bail!("Archived session is read-only; explicitly continue it before starting work")
        }
        Ok(())
    }

    pub(super) fn guard_archived_command(&self, request: &Request) -> anyhow::Result<()> {
        use crate::model::{StewardDecisionOperation as D, StewardWorkspaceOperation as W};
        let targets = match &request.command {
            Command::Start { .. }
            | Command::Prompt { .. }
            | Command::Steer { .. }
            | Command::Cancel
            | Command::Respond { .. }
            | Command::RespondUserInput { .. }
            | Command::ApplyOptions { .. }
            | Command::CreateSession { .. }
            | Command::StewardWait { .. }
            | Command::OpenTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::Goal { .. }
            | Command::RunComputerTool { .. }
            | Command::RejectComputerTool { .. }
            | Command::Rollback { .. }
            | Command::Fork { .. }
            | Command::ForkSessionFromResponse { .. }
            | Command::RewindSessionToMessage { .. }
            | Command::RefreshBackgroundWork
            | Command::StopBackgroundWork { .. } => vec![request.session_id],
            Command::StewardPrompt {
                child_session_id, ..
            }
            | Command::StewardCancel { child_session_id }
            | Command::AnswerDecision {
                child_session_id, ..
            }
            | Command::AnswerNativeDecision {
                child_session_id, ..
            } => vec![request.session_id, *child_session_id],
            Command::Consult {
                source_session_id, ..
            }
            | Command::ExecuteConsultation {
                source_session_id, ..
            } => vec![*source_session_id],
            Command::StewardDecision {
                operation: D::List { .. },
            }
            | Command::StewardWorkspace {
                operation: W::Inspect { .. } | W::Cleanup { .. },
            } => vec![],
            Command::StewardDecision {
                operation: D::Request { .. },
            } => vec![request.session_id],
            Command::StewardDecision {
                operation:
                    D::Decide { session_id, .. }
                    | D::DecideNative { session_id, .. }
                    | D::Escalate { session_id, .. },
            } => vec![request.session_id, *session_id],
            Command::StewardWorkspace {
                operation: W::Integrate { session_id, .. },
            } => vec![request.session_id, *session_id],
            Command::StewardWorkspace { .. } => vec![request.session_id],
            _ => vec![],
        };
        for id in targets {
            self.ensure_session_writable(id)?;
        }
        Ok(())
    }

    pub(super) fn complete_child(
        &self,
        manager: Uuid,
        operation: StewardLifecycleOperation,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let StewardLifecycleOperation::Complete {
            session_id: child_id,
            receipt,
            disposition,
            summary,
        } = operation else { unreachable!("completion operation") };
        events.ensure_steward_active()?;
        self.ensure_session_writable(manager)?;
        let _child = events.reserve_steward_target(child_id)?;
        let completion = {
            let _workspace = self.workspace_start_gate.lock();
            let mut state = self.task_state.lock();
            let (child, _) = self.authorized_child(&mut state, manager, child_id, events)?;
            if let Some(saved) = child.completions.iter().find(|c| c.receipt == receipt) {
                if saved.manager_session_id != manager
                    || saved.disposition != disposition
                    || saved.summary.goal != summary.goal
                    || saved.summary.result != summary.result
                    || saved.summary.decisions != summary.decisions
                    || saved.summary.verification != summary.verification
                    || saved.summary.unresolved != summary.unresolved
                    || (summary.resource_retention.is_some()
                        && saved.summary.resource_retention != summary.resource_retention)
                {
                    bail!("Result version already has a different completion")
                }
                return Ok(ResponsePayload::LifecycleCompleted {
                    session: child.clone(),
                    completion: saved.clone(),
                });
            }
            if child.archived {
                bail!("Archived session requires explicit continuation")
            }
            if super::steward::result_receipt(&child)? != receipt {
                bail!("Child result changed; read and review its current result version")
            }
            self.check_lifecycle_ready(&mut state, child_id)?;
            validate_summary(&summary, &disposition)?;
            let successful = child
                .turns
                .last()
                .is_some_and(|t| t.status == TurnStatus::Completed)
                && child.last_driver_error.is_none();
            if disposition == CompletionDisposition::Accepted && !successful {
                bail!(
                    "Failed or interrupted work requires a retry, replacement or termination decision"
                )
            }
            if disposition == CompletionDisposition::Accepted
                && child.managed_workspace.as_ref().is_some_and(|w| {
                    w.results
                        .last()
                        .is_some_and(|r| r.integration_commit.is_none())
                })
            {
                bail!("Managed code results must be accepted and integrated before archival")
            }
            let parent = state.sessions.iter_mut().find(|s| s.id == manager).unwrap();
            self.task_store.hydrate(parent)?;
            if parent.archived || parent.history_save_error.is_some() {
                bail!("Manager history must be saved before completing a child")
            }
            let before_parent = parent.clone();
            let mut summary = summary;
            if summary.resource_retention.is_none() {
                summary.resource_retention = Some(
                    if child.managed_workspace.is_some() {
                        "Task resources remain protected until confirmed delivery and safe cleanup"
                    } else {
                        "No task-owned workspace was created; existing resources are retained"
                    }
                    .into(),
                );
            }
            let message = crate::model::Message::new(
                crate::model::MessageRole::Assistant,
                completion_text(&child, &summary, &disposition),
            );
            let completion = ChildCompletion {
                id: Uuid::new_v4(),
                manager_session_id: manager,
                receipt,
                disposition,
                summary,
                summary_message_id: message.id,
                created_at: crate::model::unix_time(),
            };
            parent.messages.push(message);
            if let Some(wait) = &mut parent.steward_wait {
                wait.targets.retain(|t| t.session_id != child_id);
                if wait.targets.is_empty() {
                    parent.steward_wait = None;
                }
            }
            parent.updated_at = crate::model::unix_time();
            let saved = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            saved.archived = true;
            saved.lifecycle_revision += 1;
            saved.completions.push(completion.clone());
            state.mark_session_dirty(manager);
            state.mark_session_dirty(child_id);
            if let Err(error) = self.task_store.save(&mut state) {
                *state.sessions.iter_mut().find(|s| s.id == manager).unwrap() = before_parent;
                *state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == child_id)
                    .unwrap() = child;
                return Err(error.into());
            }
            completion
        };
        events.input_state_changed();
        if let Err(error) = self.close_runtime(child_id, None) {
            // The original completion is durable; preserve any unsaved runtime tail.
            let mut state = self.task_state.lock();
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            let saved = child
                .completions
                .iter_mut()
                .find(|c| c.id == completion.id)
                .unwrap();
            saved.summary.resource_retention = Some(format!("Runtime resources retained: {error}"));
            let completion = saved.clone();
            child.lifecycle_revision += 1;
            let text = completion_text(child, &completion.summary, &completion.disposition);
            if let Some(parent) = state.sessions.iter_mut().find(|s| s.id == manager) {
                if let Some(message) = parent
                    .messages
                    .iter_mut()
                    .find(|m| m.id == completion.summary_message_id)
                {
                    message.content = text;
                }
            }
            state.mark_session_dirty(child_id);
            state.mark_session_dirty(manager);
            self.task_store.save(&mut state)?;
            drop(state);
            events.input_state_changed();
            let session = self
                .task_state
                .lock()
                .sessions
                .iter()
                .find(|s| s.id == child_id)
                .unwrap()
                .clone();
            return Ok(ResponsePayload::LifecycleCompleted {
                session,
                completion,
            });
        }
        // Archive commits first. Cleanup independently retains every unproven resource.
        if self
            .task_state
            .lock()
            .sessions
            .iter()
            .any(|s| s.id == child_id && s.managed_workspace.is_some())
        {
            let _ = self.cleanup_task_resources(child_id, events, Some(child_id), true);
        }
        let session = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|s| s.id == child_id)
            .unwrap()
            .clone();
        Ok(ResponsePayload::LifecycleCompleted {
            session,
            completion,
        })
    }

    pub(super) fn check_lifecycle_ready(
        &self,
        state: &mut PersistedState,
        child_id: Uuid,
    ) -> anyhow::Result<()> {
        let mut pending = vec![child_id];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                bail!("Task relationship contains a cycle")
            }
            let session = state
                .sessions
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or_else(|| anyhow!("Task unavailable"))?;
            self.task_store.hydrate(session)?;
            if !super::task_cleanup::session_can_release_workspace(session) {
                bail!(
                    "Task has active work, unsaved history, unfinished delivery or unresolved decisions"
                )
            }
            if id != child_id && !session.archived {
                bail!("A descendant has not been explicitly completed and archived")
            }
            if self.terminals.lock().contains_key(&id)
                || self.terminal_workspaces.lock().contains_key(&id)
            {
                bail!("Task still owns an open terminal")
            }
            if self
                .live_background
                .lock()
                .iter()
                .any(|((session_id, _), keys)| *session_id == id && !keys.is_empty())
            {
                bail!("Background work is still active")
            }
            pending.extend(
                state
                    .sessions
                    .iter()
                    .filter(|s| s.parent_session_id == Some(id))
                    .map(|s| s.id),
            );
        }
        Ok(())
    }
}
fn validate_summary(
    summary: &ChildCompletionSummary,
    disposition: &CompletionDisposition,
) -> anyhow::Result<()> {
    for value in [&summary.goal, &summary.result]
        .into_iter()
        .chain(summary.decisions.iter())
        .chain(summary.verification.iter())
        .chain(summary.unresolved.iter())
        .chain(summary.resource_retention.iter())
    {
        if value.trim().is_empty() || value.len() > 20_000 {
            bail!("Completion fields must contain 1..20000 bytes; use null for missing information")
        }
    }
    if *disposition == CompletionDisposition::Accepted && summary.verification.is_none() {
        bail!("Acceptance requires recorded verification")
    }
    Ok(())
}
fn completion_text(
    child: &AgentSession,
    s: &ChildCompletionSummary,
    d: &CompletionDisposition,
) -> String {
    let disposition = match d {
        CompletionDisposition::Accepted => "已验收",
        CompletionDisposition::Retry => "已接管，安排重试",
        CompletionDisposition::Replace => "已接管，安排替代",
        CompletionDisposition::Terminate => "已终止",
    };
    let known = |v: &Option<String>| v.as_deref().unwrap_or("未提供").to_owned();
    format!(
        "子任务收尾：{}\n状态：{}\n目标：{}\n结果：{}\n关键决定：{}\n验证：{}\n未解决事项：{}\n资源保留：{}",
        child.title,
        disposition,
        s.goal,
        s.result,
        known(&s.decisions),
        known(&s.verification),
        known(&s.unresolved),
        known(&s.resource_retention)
    )
}
