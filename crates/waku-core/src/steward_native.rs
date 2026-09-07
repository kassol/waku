use super::steward_decision::{decision_projection, manager_instruction};
use super::*;
use crate::model::{
    DecisionRequest, DecisionState, InputDeliveryOutcome, InputDeliveryState, NativeDecision,
    NativeDecisionRequest, NativeDecisionResponse,
};

impl WakuBackend {
    pub(super) fn capture_native_request(
        &self,
        state: &mut PersistedState,
        index: usize,
        runtime_id: Uuid,
        native: NativeDecisionRequest,
    ) -> anyhow::Result<()> {
        let child = &state.sessions[index];
        if !matches!(child.provider, ProviderKind::Claude | ProviderKind::Codex) {
            return Ok(());
        }
        let Some(turn_id) = child.active_turn_id() else {
            return Ok(());
        };
        if child.decision_requests.iter().any(|r| {
            r.turn_id == turn_id
                && r.native.as_ref().is_some_and(|n| {
                    n.runtime_id == runtime_id && n.request.request_id() == native.request_id()
                })
        }) {
            return Ok(());
        }
        let child_id = child.id;
        let parent_id = child.parent_session_id.unwrap_or(child_id);
        let parent = state
            .sessions
            .iter_mut()
            .find(|s| s.id == parent_id)
            .ok_or_else(|| anyhow!("Native request manager unavailable"))?;
        self.task_store.hydrate(parent)?;
        let instruction_message_id = manager_instruction(parent).map(|m| m.id);
        let instruction = manager_instruction(parent).map(|m| m.content.clone());
        let (question, context) = match &native {
            NativeDecisionRequest::Permission { title, detail, .. } => {
                (title.clone(), detail.clone())
            }
            NativeDecisionRequest::UserInput { questions, .. } => (
                questions
                    .iter()
                    .map(|q| q.question.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                "Provider requires an answer to its original questions".into(),
            ),
        };
        state.sessions[index]
            .decision_requests
            .push(DecisionRequest {
                id: Uuid::new_v4(),
                parent_session_id: parent_id,
                child_session_id: child_id,
                turn_id,
                question,
                context,
                recommendation: "Choose only within existing user authority".into(),
                blocked_work: "Provider is waiting on its original request".into(),
                instruction_message_id,
                instruction,
                state: if child_id == parent_id {
                    DecisionState::WaitingUser
                } else {
                    DecisionState::WaitingManager
                },
                decision: None,
                authority_message_id: None,
                reason: None,
                notified: false,
                escalation: None,
                upstream_request_id: None,
                user_answer: None,
                native: Some(NativeDecision {
                    session_id: child_id,
                    runtime_id,
                    request: native,
                    response: None,
                    outcome: None,
                }),
            });
        Ok(())
    }

    pub(super) fn answer_native(
        &self,
        child_id: Uuid,
        request_id: Uuid,
        response: NativeDecisionResponse,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        let standalone = {
            let mut state = self.task_state.lock();
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .ok_or_else(|| anyhow!("Session unavailable"))?;
            self.task_store.hydrate(child)?;
            if child.parent_session_id.is_none() {
                let request = child
                    .decision_requests
                    .iter()
                    .find(|r| r.id == request_id)
                    .ok_or_else(|| anyhow!("Native request unavailable"))?;
                let native = request
                    .native
                    .as_ref()
                    .ok_or_else(|| anyhow!("Native request required"))?;
                Some((native.runtime_id, native.request.request_id().to_owned()))
            } else {
                None
            }
        };
        if let Some((runtime, wire_id)) = standalone {
            self.respond_native_direct(child_id, runtime, wire_id, response, events)?;
            self.steward_decision(
                child_id,
                crate::model::StewardDecisionOperation::List {
                    session_id: Some(child_id),
                },
                events,
            )
        } else {
            self.answer_decision_with_native(
                child_id,
                request_id,
                String::new(),
                Some(response),
                events,
            )
        }
    }

    pub(super) fn decide_native(
        &self,
        caller: Uuid,
        child_id: Uuid,
        request_id: Uuid,
        response: NativeDecisionResponse,
        authority: Option<Uuid>,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        {
            let _operation = events.reserve_steward_target(child_id)?;
            let mut state = self.task_state.lock();
            let (child, _) = self.authorized_child(&mut state, caller, child_id, events)?;
            let existing = child
                .decision_requests
                .iter()
                .find(|r| r.id == request_id)
                .ok_or_else(|| anyhow!("Native decision unavailable"))?;
            if existing.escalation.is_some() {
                bail!("Escalated requests require the user's answer")
            }
            validate_native_response(existing, &response)?;
            if matches!(
                decision_projection(&child, existing).state,
                DecisionState::Invalidated | DecisionState::Failed
            ) {
                bail!("Native request is no longer active")
            }
            if let Some(saved) = existing.native.as_ref().and_then(|n| n.response.as_ref()) {
                if saved != &response || existing.authority_message_id != authority {
                    bail!("Native request already has a different answer")
                }
                return Ok(ResponsePayload::StewardDecisions {
                    requests: vec![existing.clone()],
                });
            }
            let mut proposed = existing.clone();
            proposed.authority_message_id = authority;
            self.validate_decision_authority(&mut state, &proposed)?;
            proposed.decision = Some(native_response_summary(existing, &response)?);
            proposed.native.as_mut().unwrap().response = Some(response);
            proposed.state = DecisionState::PendingReceipt;
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            *child
                .decision_requests
                .iter_mut()
                .find(|r| r.id == request_id)
                .unwrap() = proposed;
            state.mark_session_dirty(child_id);
            self.save_steward_wait(&mut state, child_id)?;
        }
        self.deliver_native_decision(child_id, request_id, events)?;
        events.input_state_changed();
        self.steward_decision(
            caller,
            crate::model::StewardDecisionOperation::List {
                session_id: Some(child_id),
            },
            events,
        )
    }

    pub(super) fn deliver_native_decision(
        &self,
        child_id: Uuid,
        request_id: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<()> {
        self.ensure_accepting_work()?;
        let _operation = events.reserve_steward_target(child_id)?;
        let (driver, request, response) = {
            let mut state = self.task_state.lock();
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .ok_or_else(|| anyhow!("Child unavailable"))?;
            self.task_store.hydrate(child)?;
            let request = child
                .decision_requests
                .iter()
                .find(|r| r.id == request_id)
                .ok_or_else(|| anyhow!("Native decision unavailable"))?
                .clone();
            let native = request
                .native
                .as_ref()
                .ok_or_else(|| anyhow!("Native request required"))?;
            if request.state != DecisionState::PendingReceipt || native.outcome.is_some() {
                return Ok(());
            }
            validate_native_live(child, &request)?;
            let response = native
                .response
                .clone()
                .ok_or_else(|| anyhow!("Native response missing"))?;
            validate_native_response(&request, &response)?;
            if request.parent_session_id != child_id || request.user_answer.is_none() {
                self.validate_decision_authority(&mut state, &request)?;
            }
            if request.parent_session_id != child_id {
                self.authorized_child(&mut state, request.parent_session_id, child_id, events)?;
            }
            let driver = self
                .sessions
                .lock()
                .get(&child_id)
                .filter(|(runtime, _)| *runtime == native.runtime_id)
                .map(|(_, d)| d.clone())
                .ok_or_else(|| {
                    anyhow!("Original native runtime is unavailable; request cannot be resumed")
                })?;
            self.ensure_accepting_work()?;
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            child
                .decision_requests
                .iter_mut()
                .find(|r| r.id == request_id)
                .unwrap()
                .native
                .as_mut()
                .unwrap()
                .outcome = Some(InputDeliveryOutcome {
                id: request_id,
                state: InputDeliveryState::Accepted,
                confirmation: None,
                reason: None,
            });
            state.mark_session_dirty(child_id);
            self.save_steward_wait(&mut state, child_id)?;
            (driver, request, response)
        };
        let saved = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|s| s.id == child_id)
            .unwrap()
            .decision_requests
            .iter()
            .find(|r| r.id == request_id)
            .unwrap()
            .clone();
        events
            .child_sink(child_id, request.native.as_ref().unwrap().runtime_id)
            .send(event_to_wire(DriverEvent::DecisionRequestChanged(saved))?)?;
        // Driver enqueue is nonblocking. Keep the final authority check and enqueue
        // under the same state lock so a new user direction cannot interleave.
        let mut state = self.task_state.lock();
        let latest = state
            .sessions
            .iter()
            .find(|s| s.id == child_id)
            .unwrap()
            .decision_requests
            .iter()
            .find(|r| r.id == request_id)
            .unwrap()
            .clone();
        let checked = (|| -> anyhow::Result<()> {
            let child = state.sessions.iter().find(|s| s.id == child_id).unwrap();
            validate_native_live(child, &latest)?;
            if latest.parent_session_id != child_id || latest.user_answer.is_none() {
                self.validate_decision_authority(&mut state, &latest)?;
            }
            self.ensure_accepting_work()?;
            driver.respond_tracked(
                request.native.as_ref().unwrap().request.request_id().into(),
                response,
                request_id,
            )
        })();
        if let Err(error) = checked {
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .unwrap();
            let saved = child
                .decision_requests
                .iter_mut()
                .find(|r| r.id == request_id)
                .unwrap();
            saved.state = DecisionState::Failed;
            saved.reason = Some(error.to_string());
            saved.native.as_mut().unwrap().outcome = Some(InputDeliveryOutcome {
                id: request_id,
                state: InputDeliveryState::Failed,
                confirmation: None,
                reason: Some(error.to_string()),
            });
            state.mark_session_dirty(child_id);
            self.save_steward_wait(&mut state, child_id)?;
        }
        Ok(())
    }

    pub(super) fn respond_native_direct(
        &self,
        child_id: Uuid,
        runtime_id: Uuid,
        wire_id: String,
        response: NativeDecisionResponse,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        if events.scoped_project.is_some() {
            bail!("Only the user can answer directly")
        }
        let provider = self
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|s| s.id == child_id)
            .map(|s| s.provider)
            .ok_or_else(|| anyhow!("Session unavailable"))?;
        if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
            let driver = self
                .sessions
                .lock()
                .get(&child_id)
                .filter(|(id, _)| *id == runtime_id)
                .map(|(_, d)| d.clone())
                .ok_or_else(|| anyhow!("Runtime unavailable"))?;
            events.send(event_to_wire(DriverEvent::InteractionResponded {
                request_id: wire_id.clone(),
            })?)?;
            match response {
                NativeDecisionResponse::Permission { option_id } => {
                    driver.respond(wire_id, option_id)
                }
                NativeDecisionResponse::UserInput { answers } => {
                    driver.respond_user_input(wire_id, answers)
                }
            }
            return Ok(ResponsePayload::Ack);
        }
        let request_id = {
            let _operation = events.reserve_steward_target(child_id)?;
            let mut state = self.task_state.lock();
            let child = state
                .sessions
                .iter_mut()
                .find(|s| s.id == child_id)
                .ok_or_else(|| anyhow!("Session unavailable"))?;
            self.task_store.hydrate(child)?;
            if child.parent_session_id.is_some() {
                bail!("Managed child requests must be answered through the main session")
            }
            let request = child
                .decision_requests
                .iter_mut()
                .rev()
                .find(|r| {
                    r.native.as_ref().is_some_and(|n| {
                        n.runtime_id == runtime_id && n.request.request_id() == wire_id
                    })
                })
                .ok_or_else(|| anyhow!("Native request unavailable"))?
                .clone();
            validate_native_response(&request, &response)?;
            if matches!(
                request.state,
                DecisionState::Invalidated | DecisionState::Failed
            ) {
                bail!("Native request is no longer active")
            }
            if request.state != DecisionState::Resolved {
                validate_native_live(child, &request)?;
            }
            let request = child
                .decision_requests
                .iter_mut()
                .find(|r| r.id == request.id)
                .unwrap();
            if let Some(saved) = &request.native.as_ref().unwrap().response {
                if saved != &response {
                    bail!("Native request already has a different answer")
                }
                return Ok(ResponsePayload::Ack);
            }
            if matches!(
                request.state,
                DecisionState::Invalidated | DecisionState::Failed | DecisionState::Resolved
            ) {
                bail!("Native request is no longer active")
            }
            // Direct user interaction is a trusted answer to this exact live provider request.
            request.user_answer = Some(native_response_summary(request, &response)?);
            request.decision = request.user_answer.clone();
            request.native.as_mut().unwrap().response = Some(response);
            request.state = DecisionState::PendingReceipt;
            let id = request.id;
            let proposed = request.clone();
            validate_native_live(child, &proposed)?;
            state.mark_session_dirty(child_id);
            self.save_steward_wait(&mut state, child_id)?;
            id
        };
        self.deliver_native_decision(child_id, request_id, events)?;
        events.input_state_changed();
        Ok(ResponsePayload::Ack)
    }
}

pub(super) fn validate_native_live(
    child: &AgentSession,
    request: &DecisionRequest,
) -> anyhow::Result<()> {
    let native = request
        .native
        .as_ref()
        .ok_or_else(|| anyhow!("Native request required"))?;
    if child.active_turn_id() != Some(request.turn_id)
        || child.cancellation_requested_turn_id.is_some()
    {
        bail!("Original native turn is no longer active")
    }
    let _ = native;
    if matches!(
        request.state,
        DecisionState::Invalidated | DecisionState::Failed | DecisionState::Resolved
    ) {
        bail!("Native request is no longer pending")
    }
    Ok(())
}
pub(super) fn validate_native_response(
    request: &DecisionRequest,
    response: &NativeDecisionResponse,
) -> anyhow::Result<()> {
    let native = request
        .native
        .as_ref()
        .ok_or_else(|| anyhow!("Native request required"))?;
    match (&native.request, response) {
        (
            NativeDecisionRequest::Permission { options, .. },
            NativeDecisionResponse::Permission { option_id },
        ) if options.iter().any(|o| o.id == *option_id) => Ok(()),
        (
            NativeDecisionRequest::UserInput { questions, .. },
            NativeDecisionResponse::UserInput { answers },
        ) if answers.len() == questions.len()
            && questions.iter().all(|q| {
                answers
                    .iter()
                    .filter(|a| {
                        a.question_id == q.id
                            && !a.answers.is_empty()
                            && a.answers.iter().all(|s| !s.trim().is_empty())
                            && (q.multi_select || a.answers.len() == 1)
                    })
                    .count()
                    == 1
            }) =>
        {
            Ok(())
        }
        _ => bail!("Response must match the original native options and every question"),
    }
}

pub(super) fn native_response_summary(
    request: &DecisionRequest,
    response: &NativeDecisionResponse,
) -> anyhow::Result<String> {
    validate_native_response(request, response)?;
    Ok(
        match (&request.native.as_ref().unwrap().request, response) {
            (
                NativeDecisionRequest::Permission { options, .. },
                NativeDecisionResponse::Permission { option_id },
            ) => options
                .iter()
                .find(|o| o.id == *option_id)
                .unwrap()
                .label
                .clone(),
            (
                NativeDecisionRequest::UserInput { questions, .. },
                NativeDecisionResponse::UserInput { answers },
            ) => questions
                .iter()
                .map(|q| {
                    format!(
                        "{}：{}",
                        q.question,
                        answers
                            .iter()
                            .find(|a| a.question_id == q.id)
                            .unwrap()
                            .answers
                            .join("、")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => unreachable!(),
        },
    )
}
