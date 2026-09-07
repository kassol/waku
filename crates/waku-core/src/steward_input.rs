//! Durable input submissions. Save an identity before crossing the provider boundary.
use super::*;
use crate::model::{InputDelivery, InputDeliveryMode, InputDeliveryOutcome, InputDeliveryState};

impl WakuBackend {
    pub(super) fn steward_input_status(
        &self,
        caller: Uuid,
        target: Uuid,
        id: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        events.ensure_steward_active()?;
        let mut state = self.task_state.lock();
        let (session, _) = self.authorized_child(&mut state, caller, target, events)?;
        validate_child_options(
            &self.task_store,
            &mut state,
            target,
            session.provider,
            session.runtime_mode,
        )?;
        let delivery = session
            .input_deliveries
            .iter()
            .find(|d| d.id == id && d.caller_session_id == caller)
            .cloned()
            .ok_or_else(|| anyhow!("input delivery is unavailable"))?;
        drop(state);
        events.ensure_steward_active()?;
        Ok(ResponsePayload::ChildInputStatus { delivery })
    }

    // Caller must authorize this target before entering. The explicit user
    // entry can reuse this path with caller == target without expanding MCP scope.
    pub(crate) fn deliver_authorized_input(
        &self,
        caller: Uuid,
        target: Uuid,
        prompt: String,
        display_content: Option<String>,
        id: Option<Uuid>,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        if prompt.trim().is_empty() {
            bail!("prompt must not be empty");
        }
        self.ensure_accepting_work()?;
        events.ensure_steward_active()?;
        let _operation = events.reserve_input_target(target, caller == target)?;
        self.ensure_accepting_work()?;
        let exited = self
            .forwarders
            .lock()
            .get(&target)
            .is_some_and(|(_, f)| f.is_finished());
        if exited {
            self.close_runtime(target, None)?;
        }
        let id = id.unwrap_or_else(Uuid::new_v4);
        let driver = self.sessions.lock().get(&target).cloned();
        let (session, project_path, delivery, message_id) = {
            let mut state = self.task_state.lock();
            // Revalidate after acquiring the target operation, including retries.
            let (session, project_path) = if caller != target {
                self.authorized_child(&mut state, caller, target, events)?
            } else {
                let session = state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == target)
                    .ok_or_else(|| anyhow!("session is unavailable"))?;
                self.task_store.hydrate(session)?;
                let session = session.clone();
                let path = state
                    .projects
                    .iter()
                    .find(|p| p.id == session.project_id)
                    .ok_or_else(|| anyhow!("project is unavailable"))?
                    .path
                    .clone();
                (session, path)
            };
            validate_child_options(
                &self.task_store,
                &mut state,
                target,
                session.provider,
                session.runtime_mode,
            )?;
            if state
                .sessions
                .iter()
                .flat_map(|session| &session.input_deliveries)
                .any(|existing| {
                    existing.id == id
                        && (existing.caller_session_id != caller
                            || existing.target_session_id != target
                            || existing.prompt != prompt)
                })
            {
                bail!("delivery_id is already bound to different input");
            }
            if let Some(existing) = session.input_deliveries.iter().find(|d| d.id == id) {
                if existing.caller_session_id != caller
                    || existing.target_session_id != target
                    || existing.prompt != prompt
                {
                    bail!("delivery_id is already bound to different input");
                }
                return Ok(ResponsePayload::ChildPromptAccepted {
                    turn_id: existing.turn_id,
                    delivery: Some(existing.clone()),
                });
            }
            // Recheck decision authority while the same target lock and state transaction
            // that save its delivery are held. A cancellation or new user instruction
            // between scheduling and submission must not restart obsolete work.
            if let Some(request) = session.decision_requests.iter().find(|r| r.id == id) {
                if request.parent_session_id != caller
                    || super::steward_decision::decision_projection(&session, request).state != crate::model::DecisionState::PendingReceipt
                    || prompt != super::steward_decision::decision_prompt(request)
                { bail!("Decision request is not eligible for delivery"); }
                let parent = state.sessions.iter_mut().find(|s| s.id == caller).ok_or_else(|| anyhow!("Manager unavailable"))?;
                self.task_store.hydrate(parent)?;
                if parent.cancellation_requested_turn_id.is_some()
                    || super::steward_decision::manager_instruction_pending(parent)
                    || request.authority_message_id.is_none()
                    || super::steward_decision::manager_instruction(parent).map(|m| m.id) != request.authority_message_id
                { bail!("Decision authority changed before delivery"); }
            }
            if session.cancellation_requested_turn_id.is_some() {
                bail!("session cancellation has not settled");
            }
            if session.pending_permission.is_some() || session.pending_user_input.is_some() {
                bail!(
                    "session is waiting for the user; ordinary input cannot answer approvals or questions"
                );
            }
            let busy = session.active_turn_id().is_some() || session.status.is_busy();
            let supported = driver
                .as_ref()
                .is_some_and(|(_, driver)| driver.supports_steer());
            let session = state.sessions.iter_mut().find(|s| s.id == target).unwrap();
            let (turn_id, message_id) = if busy {
                (
                    session
                        .active_turn_id()
                        .ok_or_else(|| anyhow!("session has no active turn to receive input"))?,
                    None,
                )
            } else {
                let turn_id = session.begin_turn_with_presentation(prompt.clone(), display_content.clone(), Vec::new());
                session.status = SessionStatus::Connecting;
                session.last_driver_error = None;
                (turn_id, Some(session.messages.last().unwrap().id))
            };
            let delivery = InputDelivery {
                id,
                caller_session_id: caller,
                target_session_id: target,
                prompt: prompt.clone(),
                display_content,
                turn_id,
                mode: if busy && supported {
                    InputDeliveryMode::Steer
                } else {
                    InputDeliveryMode::Prompt
                },
                state: if busy && driver.is_none() {
                    InputDeliveryState::Failed
                } else if busy && !supported {
                    InputDeliveryState::Queued
                } else {
                    InputDeliveryState::Accepted
                },
                confirmation: None,
                reason: if busy && driver.is_none() {
                    Some("Runtime is unavailable; steering capability is unknown".into())
                } else {
                    (busy && !supported)
                        .then(|| "Provider does not support steering; input is queued".into())
                },
                created_at: crate::model::unix_time(),
            };
            session.input_deliveries.push(delivery.clone());
            waku_protocol::history::HistoryReducer::default()
                .apply(session, DriverEvent::InputDeliveryChanged(delivery.clone()));
            let saved_session = session.clone();
            state.mark_session_dirty(target);
            if let Err(error) = self.task_store.save(&mut state) {
                self.saving_failed.store(true, Ordering::Release);
                self.failed_sessions.lock().insert(target);
                state
                    .sessions
                    .iter_mut()
                    .find(|s| s.id == target)
                    .unwrap()
                    .history_save_error = Some(error.to_string());
                drop(state);
                events.stop_failed_work();
                return Err(error.into());
            }
            (saved_session, project_path, delivery, message_id)
        };
        if let Some(message_id) = message_id {
            self.send_saved_steward_turn(
                session,
                project_path,
                prompt,
                delivery.turn_id,
                message_id,
                Some(id),
                events,
            )?;
        } else if let Some((runtime, driver)) = driver {
            let sink = events.child_sink(target, runtime);
            sink.send(event_to_wire(DriverEvent::InputDeliveryChanged(
                delivery.clone(),
            ))?)?;
            if delivery.state == InputDeliveryState::Accepted {
                sink.send(event_to_wire(DriverEvent::InputDeliveryOutcome(
                    InputDeliveryOutcome {
                        id,
                        state: InputDeliveryState::Uncertain,
                        confirmation: None,
                        reason: Some(
                            "Awaiting provider confirmation; do not resend automatically".into(),
                        ),
                    },
                ))?)?;
                if let Err(error) = driver.deliver_input(prompt, id, true) {
                    sink.send(event_to_wire(DriverEvent::InputDeliveryOutcome(
                        InputDeliveryOutcome {
                            id,
                            state: InputDeliveryState::Failed,
                            confirmation: None,
                            reason: Some(error.to_string()),
                        },
                    ))?)?;
                }
            }
        }
        let state = self.task_state.lock();
        let delivery = state
            .sessions
            .iter()
            .find(|s| s.id == target)
            .and_then(|s| s.input_deliveries.iter().find(|d| d.id == id))
            .cloned()
            .unwrap_or(delivery);
        drop(state);
        drop(_operation);
        if delivery.state == InputDeliveryState::Queued {
            events.input_state_changed();
        }
        Ok(ResponsePayload::ChildPromptAccepted {
            turn_id: delivery.turn_id,
            delivery: Some(delivery),
        })
    }
}

impl WakuBackend {
    pub(super) fn resume_queued_inputs(&self, events: &EventSink) {
        if self.ensure_accepting_work().is_err() {
            return;
        }
        let _gate = self.work_gate.read();
        if self.ensure_accepting_work().is_err() {
            return;
        }
        let targets = self
            .task_state
            .lock()
            .sessions
            .iter()
            .filter(|session| {
                session
                    .input_deliveries
                    .iter()
                    .any(|d| d.state == InputDeliveryState::Queued)
            })
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for target in targets {
            if self.ensure_accepting_work().is_err() {
                break;
            }
            let Ok(_operation) = events.reserve_steward_target(target) else {
                continue;
            };
            if let Err(error) = self.resume_input_queue(target, events) {
                eprintln!("could not resume input queue {target}: {error:#}");
                if self.saving_failed.load(Ordering::Acquire) {
                    events.stop_failed_work();
                }
            }
        }
    }

    fn resume_input_queue(&self, target: Uuid, events: &EventSink) -> anyhow::Result<()> {
        if self
            .forwarders
            .lock()
            .get(&target)
            .is_some_and(|(_, f)| f.is_finished())
        {
            self.close_runtime(target, None)?;
        }
        let (session, project_path, delivery, message_id) = {
            let mut state = self.task_state.lock();
            let Some(session) = state.sessions.iter_mut().find(|s| s.id == target) else {
                return Ok(());
            };
            self.task_store.hydrate(session)?;
            let Some(index) = session
                .input_deliveries
                .iter()
                .position(|d| d.state == InputDeliveryState::Queued)
            else {
                return Ok(());
            };
            if session.active_turn_id().is_some()
                || session.status.is_busy()
                || session.pending_permission.is_some()
                || session.pending_user_input.is_some()
                || session.cancellation_requested_turn_id.is_some()
                || !session.queued_messages.is_empty()
            {
                return Ok(());
            }
            let latest = session.turns.last().map(|turn| turn.id);
            // A submitted prompt with no receipt cannot be overtaken or re-sent.
            if session.input_deliveries[..index].iter().any(|d| {
                Some(d.turn_id) == latest
                    && d.mode == InputDeliveryMode::Prompt
                    && matches!(
                        d.state,
                        InputDeliveryState::Accepted | InputDeliveryState::Uncertain
                    )
            }) {
                return Ok(());
            }
            let queued = session.input_deliveries[index].clone();
            let provider = session.provider;
            let mode = session.runtime_mode;
            let validation = (|| -> anyhow::Result<PathBuf> {
                if latest != Some(queued.turn_id) {
                    bail!("Target turn changed after input was queued");
                }
                let path = if queued.caller_session_id != target {
                    self.authorized_child(&mut state, queued.caller_session_id, target, events)?
                        .1
                } else {
                    let project_id = state
                        .sessions
                        .iter()
                        .find(|s| s.id == target)
                        .unwrap()
                        .project_id;
                    state
                        .projects
                        .iter()
                        .find(|p| p.id == project_id)
                        .ok_or_else(|| anyhow!("project is unavailable"))?
                        .path
                        .clone()
                };
                validate_child_options(&self.task_store, &mut state, target, provider, mode)?;
                Ok(path)
            })();
            let project_path = match validation {
                Ok(path) => path,
                Err(error) => {
                    let session = state.sessions.iter_mut().find(|s| s.id == target).unwrap();
                    session.input_deliveries[index].state = InputDeliveryState::Failed;
                    session.input_deliveries[index].reason = Some(error.to_string());
                    let delivery = session.input_deliveries[index].clone();
                    waku_protocol::history::HistoryReducer::default()
                        .apply(session, DriverEvent::InputDeliveryChanged(delivery));
                    state.mark_session_dirty(target);
                    self.save_steward_wait(&mut state, target)?;
                    drop(state);
                    events.input_state_changed();
                    return Ok(());
                }
            };
            self.ensure_accepting_work()?;
            let session = state.sessions.iter_mut().find(|s| s.id == target).unwrap();
            let turn_id = session.begin_turn_with_presentation(queued.prompt.clone(), queued.display_content.clone(), Vec::new());
            session.status = SessionStatus::Connecting;
            session.last_driver_error = None;
            let message_id = session.messages.last().unwrap().id;
            // Later queued inputs follow this saved submission, never a separate
            // user turn that happens to finish before the worker runs.
            for following in &mut session.input_deliveries[index + 1..] {
                if following.state == InputDeliveryState::Queued
                    && following.turn_id == queued.turn_id
                {
                    following.turn_id = turn_id;
                }
            }
            let delivery = &mut session.input_deliveries[index];
            delivery.turn_id = turn_id;
            delivery.mode = InputDeliveryMode::Prompt;
            delivery.state = InputDeliveryState::Accepted;
            delivery.reason = None;
            let delivery = delivery.clone();
            waku_protocol::history::HistoryReducer::default()
                .apply(session, DriverEvent::InputDeliveryChanged(delivery.clone()));
            let session = session.clone();
            state.mark_session_dirty(target);
            self.save_steward_wait(&mut state, target)?;
            (session, project_path, delivery, message_id)
        };
        events.input_state_changed();
        self.send_saved_steward_turn(
            session,
            project_path,
            delivery.prompt,
            delivery.turn_id,
            message_id,
            Some(delivery.id),
            events,
        )
    }
}

impl WakuBackend {
    pub(super) fn cancel_queued_inputs(
        &self,
        target: Uuid,
        events: &EventSink,
    ) -> anyhow::Result<bool> {
        let mut state = self.task_state.lock();
        let Some(session) = state.sessions.iter_mut().find(|s| s.id == target) else {
            return Ok(false);
        };
        self.task_store.hydrate(session)?;
        let mut changed = Vec::new();
        for delivery in &mut session.input_deliveries {
            if delivery.state == InputDeliveryState::Queued {
                delivery.state = InputDeliveryState::Failed;
                delivery.reason = Some("Queued input was cancelled before submission".into());
                changed.push(delivery.clone());
            }
        }
        if changed.is_empty() {
            return Ok(false);
        }
        for delivery in changed {
            waku_protocol::history::HistoryReducer::default()
                .apply(session, DriverEvent::InputDeliveryChanged(delivery));
        }
        state.mark_session_dirty(target);
        if let Err(error) = self.save_steward_wait(&mut state, target) {
            drop(state);
            events.stop_failed_work();
            return Err(error);
        }
        drop(state);
        events.input_state_changed();
        Ok(true)
    }
}
