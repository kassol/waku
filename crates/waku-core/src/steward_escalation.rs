//! Escalations follow saved direct-child links. A user answer is scoped to one chain.
use super::steward_decision::{
    decision_projection, manager_instruction, manager_instruction_pending,
};
use super::*;
use crate::model::{
    DecisionEscalation, DecisionRequest, DecisionState, Message, MessageRole,
    StewardDecisionOperation, StewardWait, StewardWaitTarget, TurnStatus,
};

impl WakuBackend {
    pub(super) fn escalate_decision(
        &self,
        caller: Uuid,
        child_id: Uuid,
        request_id: Uuid,
        escalation: DecisionEscalation,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        if escalation.options.is_empty() || escalation.options.len() > 8 {
            bail!("Escalation requires 1..8 options");
        }
        for text in std::iter::once(&escalation.reason)
            .chain(std::iter::once(&escalation.impact))
            .chain(
                escalation
                    .options
                    .iter()
                    .flat_map(|o| [&o.label, &o.impact]),
            )
        {
            if text.trim().is_empty() || text.len() > 20_000 {
                bail!("Escalation fields must contain 1..20000 bytes");
            }
        }
        let _operation = events.reserve_steward_target(caller)?;
        let mut state = self.task_state.lock();
        let (child, _) = self.authorized_child(&mut state, caller, child_id, events)?;
        let request = child
            .decision_requests
            .iter()
            .find(|r| r.id == request_id)
            .ok_or_else(|| anyhow!("Decision request unavailable"))?
            .clone();
        if matches!(
            request.state,
            DecisionState::Invalidated | DecisionState::Failed
        ) {
            bail!("Decision request is no longer active");
        }
        if let Some(existing) = &request.escalation {
            if existing != &escalation {
                bail!("Request already has different escalation details");
            }
            if request.upstream_request_id.is_some()
                || request.state != DecisionState::WaitingManager
            {
                return Ok(ResponsePayload::StewardDecisions {
                    requests: vec![decision_projection(&child, &request)],
                });
            }
        }
        if request.decision.is_some() {
            bail!("Request already has a decision");
        }
        let manager = state.sessions.iter_mut().find(|s| s.id == caller).unwrap();
        self.task_store.hydrate(manager)?;
        let manager = manager.clone();
        let manager_turn = manager
            .turns
            .last()
            .filter(|t| !matches!(t.status, TurnStatus::Failed | TurnStatus::Interrupted))
            .ok_or_else(|| anyhow!("Manager execution context is unavailable"))?
            .id;
        let upstream = if let Some(grandparent_id) = manager.parent_session_id {
            self.authorized_child(&mut state, grandparent_id, caller, events)?;
            let grandparent = state
                .sessions
                .iter_mut()
                .find(|s| s.id == grandparent_id)
                .unwrap();
            self.task_store.hydrate(grandparent)?;
            let instruction = manager_instruction(grandparent);
            let mut forwarded = request.clone();
            forwarded.id = Uuid::new_v4();
            forwarded.child_session_id = caller;
            forwarded.parent_session_id = grandparent_id;
            forwarded.turn_id = manager_turn;
            forwarded.instruction_message_id = instruction.map(|m| m.id);
            forwarded.instruction =
                instruction.map(|m| m.display_content.as_ref().unwrap_or(&m.content).clone());
            forwarded.state = DecisionState::WaitingManager;
            forwarded.escalation = Some(escalation.clone());
            forwarded.upstream_request_id = None;
            forwarded.notified = false;
            let id = forwarded.id;
            state
                .sessions
                .iter_mut()
                .find(|s| s.id == caller)
                .unwrap()
                .decision_requests
                .push(forwarded);
            Some(id)
        } else {
            None
        };
        let updated = state
            .sessions
            .iter_mut()
            .find(|s| s.id == child_id)
            .unwrap()
            .decision_requests
            .iter_mut()
            .find(|r| r.id == request_id)
            .unwrap();
        updated.escalation = Some(escalation);
        updated.upstream_request_id = upstream;
        updated.state = DecisionState::WaitingUser;
        updated.reason = Some(
            if upstream.is_some() {
                "Waiting for the user through the direct manager"
            } else {
                "Waiting for the user's answer in the main session"
            }
            .into(),
        );
        let updated = updated.clone();
        ensure_result_wait(&mut state, caller, child_id, request.turn_id)?;
        state.mark_session_dirty(caller);
        state.mark_session_dirty(child_id);
        self.save_steward_wait(&mut state, caller)?;
        drop(state);
        events.input_state_changed();
        Ok(ResponsePayload::StewardDecisions {
            requests: vec![updated],
        })
    }

    pub(super) fn answer_decision(
        &self,
        child_id: Uuid,
        request_id: Uuid,
        answer: String,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        self.answer_decision_with_native(child_id, request_id, answer, None, events)
    }

    pub(super) fn answer_decision_with_native(&self, child_id: Uuid, request_id: Uuid, answer: String, response: Option<crate::model::NativeDecisionResponse>, events: &EventSink) -> anyhow::Result<ResponsePayload> {
        if events.scoped_project.is_some() {
            bail!("Only the user can answer escalated decisions");
        }
        let answer = if let Some(response) = &response {
            let state = self.task_state.lock();
            let request = state.sessions.iter().flat_map(|s|&s.decision_requests).find(|r|r.id==request_id).ok_or_else(||anyhow!("Native decision unavailable"))?;
            super::steward_native::native_response_summary(request,response)?
        } else { answer };
        if answer.trim().is_empty() || answer.len() > 20_000 {
            bail!("Answer must contain 1..20000 bytes");
        }
        self.refresh_decisions()?;
        let root_id = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|s| s.id == child_id)
            .and_then(|s| s.parent_session_id)
            .ok_or_else(|| anyhow!("Decision manager unavailable"))?;
        let _operation = events.reserve_steward_target(root_id)?;
        let leaf = {
            let mut state = self.task_state.lock();
            let (child, _) = self.authorized_child(&mut state, root_id, child_id, events)?;
            let root = state.sessions.iter_mut().find(|s| s.id == root_id).unwrap();
            self.task_store.hydrate(root)?;
            if manager_instruction_pending(root) {
                bail!("Wait for the current user instruction receipt before answering");
            }
            if root.parent_session_id.is_some() {
                bail!(
                    "User answers must be submitted in the main session after direct-manager escalation"
                );
            }
            let request = child
                .decision_requests
                .iter()
                .find(|r| r.id == request_id)
                .ok_or_else(|| anyhow!("Decision request unavailable"))?
                .clone();
            if matches!(
                request.state,
                DecisionState::Invalidated | DecisionState::Failed
            ) {
                bail!("Decision request is no longer active");
            }
            if let Some(saved) = &request.user_answer {
                if saved != &answer || request.native.as_ref().and_then(|n|n.response.as_ref()) != response.as_ref() {
                    bail!("Request already has a different user answer");
                }
                drop(state);
                return self.steward_decision(
                    root_id,
                    StewardDecisionOperation::List {
                        session_id: Some(child_id),
                    },
                    events,
                );
            }
            if request.state != DecisionState::WaitingUser || request.upstream_request_id.is_some()
            {
                bail!("Request has not reached the user in the main session");
            }
            let chain = self.decision_descendants(&mut state, child_id, request_id)?;
            for (_, request) in &chain {
                match (&request.native, &response) {
                    (Some(_), Some(response)) => super::steward_native::validate_native_response(request, response)?,
                    (None, None) => {},
                    _ => bail!("Native requests require a typed native answer"),
                }
            }
            for (id, request) in &chain {
                let session = state.sessions.iter().find(|s| s.id == *id).unwrap();
                if !matches!(
                    decision_projection(session, request).state,
                    DecisionState::WaitingManager | DecisionState::WaitingUser
                ) {
                    bail!("A linked request is no longer waiting for an answer");
                }
            }
            let root = state.sessions.iter_mut().find(|s| s.id == root_id).unwrap();
            let message = Message::new(MessageRole::User, answer.clone())
                .with_presentation(Some(format!("对子会话问题的答复：{answer}")), Vec::new());
            let authority = message.id;
            root.messages.push(message);
            for (id, request) in &chain {
                let child = state.sessions.iter_mut().find(|s| s.id == *id).unwrap();
                let saved = child
                    .decision_requests
                    .iter_mut()
                    .find(|r| r.id == request.id)
                    .unwrap();
                if let Some(native) = &mut saved.native { native.response = response.clone(); }
                saved.user_answer = Some(answer.clone());
                saved.decision = Some(answer.clone());
                saved.authority_message_id = Some(authority);
                saved.state = DecisionState::PendingReceipt;
                saved.reason = None;
                state.mark_session_dirty(*id);
            }
            ensure_result_wait(&mut state, root_id, child_id, request.turn_id)?;
            state.mark_session_dirty(root_id);
            self.save_steward_wait(&mut state, root_id)?;
            let leaf = chain.last().unwrap();
            (leaf.0, leaf.1.id)
        };
        self.deliver_decision(leaf.0, leaf.1, events)?;
        events.input_state_changed();
        self.steward_decision(
            root_id,
            StewardDecisionOperation::List {
                session_id: Some(child_id),
            },
            events,
        )
    }

    fn decision_descendants(
        &self,
        state: &mut PersistedState,
        child_id: Uuid,
        request_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, DecisionRequest)>> {
        let mut chain = Vec::new();
        let mut current = (child_id, request_id);
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.1) {
                bail!("Decision escalation contains a cycle");
            }
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == current.0)
                .ok_or_else(|| anyhow!("Linked child unavailable"))?;
            self.task_store.hydrate(child)?;
            let request = child
                .decision_requests
                .iter()
                .find(|r| r.id == current.1)
                .ok_or_else(|| anyhow!("Linked request unavailable"))?
                .clone();
            chain.push((current.0, request));
            let next = state
                .sessions
                .iter()
                .flat_map(|s| s.decision_requests.iter().map(move |r| (s.id, r)))
                .find(|(_, r)| r.upstream_request_id == Some(current.1))
                .map(|(id, r)| (id, r.id, r.parent_session_id));
            let Some((id, request_id, parent)) = next else {
                break;
            };
            let expected_project = state
                .sessions
                .iter()
                .find(|s| s.id == current.0)
                .unwrap()
                .project_id;
            let next_child = state.sessions.iter().find(|s| s.id == id).unwrap();
            if parent != current.0
                || next_child.parent_session_id != Some(current.0)
                || next_child.project_id != expected_project
            {
                bail!("Escalation no longer follows direct children in the same project");
            }
            current = (id, request_id);
        }
        Ok(chain)
    }

    pub(super) fn validate_decision_authority(
        &self,
        state: &mut PersistedState,
        request: &DecisionRequest,
    ) -> anyhow::Result<()> {
        let mut source = request.clone();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(source.id) {
                bail!("Decision authority contains a cycle");
            }
            let mut top = source.clone();
            while let Some(id) = top.upstream_request_id {
                if !seen.insert(id) {
                    bail!("Decision escalation contains a cycle");
                }
                let upper = state
                    .sessions
                    .iter()
                    .flat_map(|s| &s.decision_requests)
                    .find(|r| r.id == id)
                    .ok_or_else(|| anyhow!("Escalated authority unavailable"))?
                    .clone();
                let lower_session = state
                    .sessions
                    .iter()
                    .find(|s| s.id == top.child_session_id)
                    .ok_or_else(|| anyhow!("Child unavailable"))?
                    .clone();
                let upper_session = state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == upper.child_session_id)
                    .ok_or_else(|| anyhow!("Manager unavailable"))?;
                self.task_store.hydrate(upper_session)?;
                if top.parent_session_id != upper.child_session_id
                    || lower_session.parent_session_id != Some(upper.child_session_id)
                    || lower_session.project_id != upper_session.project_id
                    || upper.user_answer != source.user_answer
                    || upper.authority_message_id != source.authority_message_id
                    || matches!(
                        upper.state,
                        DecisionState::Invalidated | DecisionState::Failed
                    )
                    || top.instruction_message_id
                        != manager_instruction(upper_session).map(|m| m.id)
                    || manager_instruction_pending(upper_session)
                {
                    bail!("Escalated authority no longer matches the request chain");
                }
                top = upper;
            }
            let child_project = state
                .sessions
                .iter()
                .find(|s| {
                    s.id == top.child_session_id
                        && s.parent_session_id == Some(top.parent_session_id)
                })
                .map(|s| s.project_id);
            let parent = state
                .sessions
                .iter_mut()
                .find(|s| s.id == top.parent_session_id)
                .ok_or_else(|| anyhow!("Manager unavailable"))?;
            self.task_store.hydrate(parent)?;
            if child_project != Some(parent.project_id)
                || parent.cancellation_requested_turn_id.is_some()
                || manager_instruction_pending(parent)
                || top.instruction_message_id != manager_instruction(parent).map(|m| m.id)
            {
                bail!("Decision authority changed before delivery");
            }
            if let Some(answer) = &source.user_answer {
                if parent.parent_session_id.is_none()
                    && source.authority_message_id.is_some_and(|id| {
                        parent.messages.iter().any(|m| {
                            m.id == id && m.role == MessageRole::User && m.content == *answer
                        })
                    })
                {
                    return Ok(());
                }
                bail!("Saved user answer authority is unavailable");
            }
            if parent.parent_session_id.is_none() {
                if source.authority_message_id.is_some()
                    && manager_instruction(parent).map(|m| m.id) == source.authority_message_id
                {
                    return Ok(());
                }
                bail!("Decision must cite the current user instruction");
            }
            // A nested manager can reuse received authority, never its model-authored assignment.
            source = parent
                .decision_requests
                .iter()
                .find(|r| {
                    r.state == DecisionState::Resolved
                        && r.authority_message_id.is_some()
                        && r.authority_message_id == source.authority_message_id
                        && parent.input_deliveries.iter().any(|d| {
                            d.id == r.id && d.state == crate::model::InputDeliveryState::Received
                        })
                })
                .ok_or_else(|| {
                    anyhow!("Nested manager has no received user authority for this decision")
                })?
                .clone();
        }
    }

    pub(super) fn refresh_escalations(&self, state: &mut PersistedState) -> anyhow::Result<()> {
        // Follow each changed terminal outcome upward; a linked question never
        // appears received merely because an intermediate manager ended a turn.
        let mut terminals = state
            .sessions
            .iter()
            .flat_map(|s| &s.decision_requests)
            .filter(|r| {
                matches!(
                    r.state,
                    DecisionState::Resolved | DecisionState::Failed | DecisionState::Invalidated
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        // Relay the leaf's confirmed receipt without invoking intermediate runtimes.
        let receipts = state.sessions.iter().flat_map(|s| &s.decision_requests)
            .filter_map(|r| r.native.as_ref().and_then(|n| n.outcome.as_ref()).map(|o| (r.upstream_request_id, o.clone()))).collect::<Vec<_>>();
        for (mut upstream, outcome) in receipts {
            let mut seen = HashSet::new();
            while let Some(id) = upstream {
                if !seen.insert(id) { break; }
                let Some(session) = state.sessions.iter_mut().find(|s|s.decision_requests.iter().any(|r|r.id==id)) else { break };
                let request = session.decision_requests.iter_mut().find(|r|r.id==id).unwrap();
                upstream = request.upstream_request_id;
                if let Some(native) = &mut request.native {
                    if native.outcome.as_ref() != Some(&outcome) {
                        native.outcome = Some(outcome.clone());
                        request.reason = outcome.reason.clone();
                        let session_id=session.id; state.mark_session_dirty(session_id); self.save_steward_wait(state,session_id)?;
                    }
                }
            }
        }
        while let Some(terminal) = terminals.pop() {
            if terminal.state == DecisionState::Invalidated {
                let resumed_turn = state
                    .sessions
                    .iter()
                    .find(|s| s.id == terminal.child_session_id)
                    .and_then(|s| s.input_deliveries.iter().find(|d| d.id == terminal.id))
                    .map(|d| d.turn_id);
                if let Some(parent) = state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == terminal.parent_session_id)
                {
                    if let Some(wait) = &mut parent.steward_wait {
                        let old_len = wait.targets.len();
                        wait.targets.retain(|target| {
                            target.session_id != terminal.child_session_id
                                || (target.turn_id != terminal.turn_id
                                    && Some(target.turn_id) != resumed_turn)
                        });
                        if wait.targets.len() != old_len {
                            if wait.targets.is_empty() {
                                parent.steward_wait = None;
                            }
                            let parent_id = parent.id;
                            state.mark_session_dirty(parent_id);
                            self.save_steward_wait(state, parent_id)?;
                        }
                    }
                }
            }
            if matches!(
                terminal.state,
                DecisionState::Invalidated | DecisionState::Failed
            ) {
                let mut changed = Vec::new();
                for session in &mut state.sessions {
                    for request in &mut session.decision_requests {
                        if request.upstream_request_id == Some(terminal.id)
                            && request.state != DecisionState::Invalidated
                            && request.state != terminal.state
                        {
                            request.state = terminal.state.clone();
                            request.reason = terminal.reason.clone();
                            terminals.push(request.clone());
                            changed.push(session.id);
                        }
                    }
                }
                for id in changed {
                    state.mark_session_dirty(id);
                    self.save_steward_wait(state, id)?;
                }
            }
            let mut upstream = terminal.upstream_request_id;
            let mut seen = HashSet::new();
            while let Some(id) = upstream {
                if !seen.insert(id) {
                    break;
                }
                let Some((index, request_index)) =
                    state.sessions.iter().enumerate().find_map(|(i, s)| {
                        s.decision_requests
                            .iter()
                            .position(|r| r.id == id)
                            .map(|r| (i, r))
                    })
                else {
                    break;
                };
                let request = &mut state.sessions[index].decision_requests[request_index];
                upstream = request.upstream_request_id;
                if request.state == terminal.state
                    || request.state == DecisionState::Invalidated
                    || (request.state == DecisionState::Failed
                        && terminal.state == DecisionState::Resolved)
                {
                    continue;
                }
                request.state = terminal.state.clone();
                request.reason = terminal.reason.clone();
                let session_id = state.sessions[index].id;
                state.mark_session_dirty(session_id);
                self.save_steward_wait(state, session_id)?;
            }
        }
        Ok(())
    }
}

fn ensure_result_wait(
    state: &mut PersistedState,
    parent_id: Uuid,
    child_id: Uuid,
    turn_id: Uuid,
) -> anyhow::Result<()> {
    let parent = state
        .sessions
        .iter_mut()
        .find(|s| s.id == parent_id)
        .ok_or_else(|| anyhow!("Manager unavailable"))?;
    let parent_turn_id = parent
        .turns
        .last()
        .ok_or_else(|| anyhow!("Manager turn unavailable"))?
        .id;
    let wait = parent.steward_wait.get_or_insert_with(|| StewardWait {
        id: Uuid::new_v4(),
        parent_turn_id,
        targets: Vec::new(),
    });
    wait.parent_turn_id = parent_turn_id;
    if !wait.targets.iter().any(|t| t.session_id == child_id) {
        wait.targets.push(StewardWaitTarget {
            session_id: child_id,
            turn_id,
        });
    }
    Ok(())
}
