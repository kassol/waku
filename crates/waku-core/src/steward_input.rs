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
        id: Option<Uuid>,
        events: &EventSink,
    ) -> anyhow::Result<ResponsePayload> {
        if prompt.trim().is_empty() {
            bail!("prompt must not be empty");
        }
        self.ensure_accepting_work()?;
        events.ensure_steward_active()?;
        let _operation = events.reserve_steward_target(target)?;
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
                let turn_id = session.begin_turn(prompt.clone());
                session.status = SessionStatus::Connecting;
                session.last_driver_error = None;
                (turn_id, Some(session.messages.last().unwrap().id))
            };
            let delivery = InputDelivery {
                id,
                caller_session_id: caller,
                target_session_id: target,
                prompt: prompt.clone(),
                turn_id,
                mode: if busy {
                    InputDeliveryMode::Steer
                } else {
                    InputDeliveryMode::Prompt
                },
                state: if busy && driver.is_none() {
                    InputDeliveryState::Failed
                } else if busy && !supported {
                    InputDeliveryState::Unsupported
                } else {
                    InputDeliveryState::Accepted
                },
                confirmation: None,
                reason: if busy && driver.is_none() {
                    Some("Runtime is unavailable; steering capability is unknown".into())
                } else {
                    (busy && !supported)
                        .then(|| "Provider does not support steering this runtime".into())
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
        Ok(ResponsePayload::ChildPromptAccepted {
            turn_id: delivery.turn_id,
            delivery: Some(delivery),
        })
    }
}
