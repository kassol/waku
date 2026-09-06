//! Provider event projection shared by the daemon and desktop client.
//! This module performs no I/O; presentation and runtime side effects stay with callers.

use crate::model::*;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamPhase {
    Text,
    Reasoning,
    Activity,
}

/// Transient ordering state for one runtime; retain it between events.
#[derive(Default)]
pub struct HistoryReducer {
    pub stream_phase: Option<StreamPhase>,
    pub last_driver_error: Option<String>,
}

#[derive(Default)]
pub struct HistoryEffects {
    pub invalidated_activity_diff: Option<Uuid>,
    pub finished_turn: Option<(Uuid, usize)>,
}

/// Apply a completed rewind without replacing user choices made while it ran.
pub fn apply_rewound_history(current: &mut AgentSession, rewound: AgentSession) {
    current.messages = rewound.messages;
    current.transcript_blocks = rewound.transcript_blocks;
    current.turns = rewound.turns;
    current.provider_cursor = rewound.provider_cursor;
    current.runtime_event_cursor = rewound.runtime_event_cursor;
    current.history_saved_cursor = rewound.history_saved_cursor;
    current.history_save_error = rewound.history_save_error;
    current.last_driver_error = rewound.last_driver_error;
    current.cancellation_requested_turn_id = rewound.cancellation_requested_turn_id;
    current.pending_permission = rewound.pending_permission;
    current.pending_user_input = rewound.pending_user_input;
    current.status = rewound.status;
    current.steward_wait = rewound.steward_wait;
    // Delivery acknowledgments cannot be rewound or re-sent with conversation history.
    current.updated_at = current.updated_at.max(rewound.updated_at);
}

pub fn history_snapshot_is_stale(current: &AgentSession, snapshot: &AgentSession) -> bool {
    matches!((current.runtime_event_cursor, snapshot.runtime_event_cursor), (Some(current), Some(saved))
        if current.runtime_id == saved.runtime_id && current.epoch == saved.epoch && current.sequence > saved.sequence)
}

impl HistoryReducer {
    /// Apply one event after the transport has ordered and deduplicated it.
    pub fn apply(&mut self, session: &mut AgentSession, event: DriverEvent) -> HistoryEffects {
        let mut effects = HistoryEffects::default();
        match event {
            DriverEvent::InputDeliveryChanged(delivery) => {
                if !session.input_deliveries.iter().any(|entry| entry.id == delivery.id) {
                    session.input_deliveries.push(delivery.clone());
                }
                effects.invalidated_activity_diff = self.update_activity(session, input_delivery_activity(&delivery));
            }
            DriverEvent::InputDeliveryOutcome(outcome) => {
                let mut changed = None;
                if let Some(delivery) = session.input_deliveries.iter_mut().find(|d| d.id == outcome.id) {
                    if matches!(delivery.state, InputDeliveryState::Accepted | InputDeliveryState::Uncertain) {
                        delivery.state = outcome.state;
                        delivery.confirmation = outcome.confirmation;
                        delivery.reason = outcome.reason;
                        changed = Some(delivery.clone());
                        if delivery.state == InputDeliveryState::Received && delivery.mode == InputDeliveryMode::Steer {
                            let turn_id = delivery.turn_id;
                            let prompt = delivery.prompt.clone();
                            session.steward_wait = None;
                            session.messages.push(Message::new_for_turn(MessageRole::User, prompt, turn_id));
                        }
                    }
                }
                if let Some(delivery) = changed {
                    effects.invalidated_activity_diff = self.update_activity(session, input_delivery_activity(&delivery));
                }
            }
            DriverEvent::StewardWaitChanged(wait) => {
                if wait.as_ref().is_none_or(|wait| {
                    session
                        .turns
                        .last()
                        .is_some_and(|turn| turn.id == wait.parent_turn_id)
                }) {
                    session.steward_wait = wait;
                }
            }
            DriverEvent::HistorySnapshot(mut snapshot) => {
                if history_snapshot_is_stale(session, &snapshot) {
                    return effects;
                }
                // A replay snapshot owns history; user choices and locally
                // submitted turns may have changed while it was in flight.
                let saved_turn_count = snapshot.turns.last().map_or(0, |turn| turn.turn_count);
                let local_turns = session
                    .turns
                    .iter()
                    .filter(|turn| turn.turn_count > saved_turn_count)
                    .cloned()
                    .collect::<Vec<_>>();
                if !local_turns.is_empty() {
                    let local_ids = local_turns
                        .iter()
                        .map(|turn| turn.id)
                        .collect::<std::collections::HashSet<_>>();
                    snapshot.messages.extend(
                        session
                            .messages
                            .iter()
                            .filter(|message| {
                                message.turn_id.is_some_and(|id| local_ids.contains(&id))
                            })
                            .cloned(),
                    );
                    snapshot.transcript_blocks.extend(
                        session
                            .transcript_blocks
                            .iter()
                            .filter(|block| block.turn_id.is_some_and(|id| local_ids.contains(&id)))
                            .cloned(),
                    );
                    snapshot.turns.extend(local_turns);
                    snapshot.status = session.status;
                    snapshot.steward_wait = session.steward_wait.clone();
                    snapshot.last_driver_error = session.last_driver_error.clone();
                }
                session.parent_session_id = snapshot.parent_session_id;
                session.auto_title = snapshot.auto_title.take();
                session.available_commands = std::mem::take(&mut snapshot.available_commands);
                session.thread_goal = snapshot.thread_goal.take();
                session.context_usage = snapshot.context_usage;
                session.last_reply_at = session.last_reply_at.max(snapshot.last_reply_at);
                session.detail_loaded = true;
                session.input_deliveries = snapshot.input_deliveries.clone();
                apply_rewound_history(session, *snapshot);
                self.last_driver_error = session.last_driver_error.clone();
                self.stream_phase = if session
                    .messages
                    .last()
                    .is_some_and(|message| message.streaming)
                {
                    Some(StreamPhase::Text)
                } else {
                    session
                        .transcript_blocks
                        .last()
                        .filter(|block| block.turn_id == session.active_turn_id())
                        .map(|block| {
                            if block.activities.last().is_some_and(|activity| {
                                activity.reasoning.is_some() && !activity.complete
                            }) {
                                StreamPhase::Reasoning
                            } else {
                                StreamPhase::Activity
                            }
                        })
                };
            }
            DriverEvent::PromptSubmitted {
                message,
                turn_id,
                message_id,
            } => {
                session.adopt_submitted_prompt(&message, turn_id, message_id);
                self.last_driver_error = None;
                session.last_driver_error = None;
            }
            DriverEvent::CancelRequested => {
                session.steward_wait = None;
                session.cancellation_requested_turn_id = session.active_turn_id();
            }
            DriverEvent::TurnInterrupted => {
                session.steward_wait = None;
                session.cancellation_requested_turn_id = None;
                session.pending_permission = None;
                session.pending_user_input = None;
                if session.active_turn_id().is_some() {
                    finish_streaming_assistant(session);
                    complete_turn_blocks(session);
                    self.stream_phase = None;
                    self.last_driver_error = None;
                    session.last_driver_error = None;
                    if !turn_has_assistant_message(session) {
                        session.push_message(MessageRole::Assistant, tr!("session.stopped"));
                    }
                    session.status = SessionStatus::Idle;
                    effects.finished_turn = session.finish_active_turn(TurnStatus::Interrupted);
                }
            }
            DriverEvent::InteractionResponded { request_id } => {
                if session
                    .pending_permission
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    session.pending_permission = None;
                }
                if session
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    session.pending_user_input = None;
                }
                if session.active_turn_id().is_some()
                    && session.status == SessionStatus::Waiting
                    && session.pending_permission.is_none()
                    && session.pending_user_input.is_none()
                {
                    session.status = SessionStatus::Working;
                }
            }
            DriverEvent::TurnStarted => {
                self.last_driver_error = None;
                session.last_driver_error = None;
                if session.active_turn_id().is_some() {
                    session.mark_active_turn_provider_started();
                    session.status = SessionStatus::Working;
                } else if matches!(session.provider, ProviderKind::Codex | ProviderKind::Claude) {
                    session.begin_provider_turn();
                    session.mark_active_turn_provider_started();
                    session.status = SessionStatus::Working;
                }
            }
            DriverEvent::TextDelta(delta) => {
                if session_accepts_turn_output(session) {
                    if self.stream_phase == Some(StreamPhase::Reasoning) {
                        self.complete_reasoning_activity(session);
                    }
                    append_text_delta(session, self.stream_phase == Some(StreamPhase::Text), delta);
                    self.stream_phase = Some(StreamPhase::Text);
                }
            }
            DriverEvent::ReasoningDelta(delta) => {
                if session_accepts_turn_output(session) {
                    self.append_reasoning_delta(session, delta);
                }
            }
            DriverEvent::Activity {
                id,
                kind,
                title,
                detail,
                complete,
            } => {
                if session_accepts_turn_output(session) {
                    effects.invalidated_activity_diff = self.update_activity(
                        session,
                        ActivityItem::new(id, kind, title, detail, complete),
                    );
                }
            }
            DriverEvent::RichActivity(item) => {
                if session_accepts_turn_output(session) {
                    effects.invalidated_activity_diff = self.update_activity(session, item);
                }
            }
            DriverEvent::TurnParked => {
                session.pending_permission = None;
                session.pending_user_input = None;
                if session.active_turn_id().is_some() {
                    finish_streaming_assistant(session);
                    complete_turn_blocks(session);
                    self.stream_phase = None;
                    session.status = SessionStatus::Background;
                    session.updated_at = unix_time();
                }
            }
            DriverEvent::TurnFinished { success, summary } => {
                if !success {
                    session.steward_wait = None;
                }
                if session.cancellation_requested_turn_id.is_some()
                    && session.cancellation_requested_turn_id == session.active_turn_id()
                {
                    return self.apply(session, DriverEvent::TurnInterrupted);
                }
                session.cancellation_requested_turn_id = None;
                session.pending_permission = None;
                session.pending_user_input = None;
                session.last_driver_error = if success {
                    None
                } else {
                    self.last_driver_error
                        .take()
                        .or_else(|| session.last_driver_error.clone())
                        .or_else(|| summary.clone())
                        .or_else(|| Some(tr!("session.stopped_before_response")))
                };
                self.last_driver_error = None;
                if session.active_turn_id().is_some() {
                    finish_streaming_assistant(session);
                    complete_turn_blocks(session);
                    self.stream_phase = None;
                    let needs_fallback = !turn_has_assistant_message(session);
                    session.status = if success {
                        SessionStatus::Idle
                    } else {
                        SessionStatus::Failed
                    };
                    if needs_fallback {
                        session.push_message(
                            MessageRole::Assistant,
                            summary.unwrap_or_else(|| {
                                if success {
                                    tr!("session.turn_completed")
                                } else {
                                    tr!("session.stopped_before_response")
                                }
                            }),
                        );
                    }
                    effects.finished_turn = session.finish_active_turn(if success {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    });
                }
            }
            DriverEvent::Error(error) => {
                let error = compact_driver_error(&error);
                self.last_driver_error = Some(error.clone());
                session.last_driver_error = Some(error.clone());
                if session.active_turn_is_unconfirmed_pursuit() {
                    if let Some(turn_id) = session.active_turn_id() {
                        session.unwind_unstarted_turn(turn_id);
                    }
                    if session.status.is_busy() {
                        session.status = SessionStatus::Idle;
                    }
                }
                let has_active_turn = session.active_turn_id().is_some();
                let should_append = has_active_turn
                    && !turn_has_assistant_message(session)
                    && session.status != SessionStatus::Working;
                if has_active_turn {
                    if session.status != SessionStatus::Working {
                        session.status = SessionStatus::Failed;
                    }
                    if should_append {
                        session.push_message(MessageRole::Assistant, error);
                    }
                }
            }
            DriverEvent::ProcessExited => {
                for delivery in &mut session.input_deliveries {
                    if delivery.state == InputDeliveryState::Accepted {
                        delivery.state = InputDeliveryState::Uncertain;
                        delivery.reason = Some("Provider exited before acknowledging input; do not resend automatically".into());
                    }
                }

                if session.cancellation_requested_turn_id.is_some()
                    && session.cancellation_requested_turn_id == session.active_turn_id()
                {
                    return self.apply(session, DriverEvent::TurnInterrupted);
                }
                session.pending_permission = None;
                session.pending_user_input = None;
                finish_streaming_assistant(session);
                complete_turn_blocks(session);
                self.stream_phase = None;
                let needs_fallback = !turn_has_assistant_message(session);
                let failure_message = self
                    .last_driver_error
                    .take()
                    .or_else(|| session.last_driver_error.clone())
                    .unwrap_or_else(|| tr!("session.codex_exited_before_response"));
                if session.status.is_busy() || session.active_turn_id().is_some() {
                    session.steward_wait = None;
                    session.last_driver_error = Some(failure_message.clone());
                    session.status = SessionStatus::Failed;
                    session.updated_at = unix_time();
                    if needs_fallback {
                        session.push_message(MessageRole::Assistant, failure_message);
                    }
                    effects.finished_turn = session.finish_active_turn(TurnStatus::Failed);
                }
            }
            DriverEvent::RuntimeEventCursorAdvanced(cursor) => {
                session.runtime_event_cursor = Some(cursor);
            }
            DriverEvent::Connected { provider_cursor } => {
                self.last_driver_error = None;
                if let Some(ProviderResumeCursor::Claude {
                    resume_at: Some(message_id),
                    ..
                }) = &provider_cursor
                {
                    session.mark_active_turn_provider_resume_at(message_id.clone());
                }
                session.provider_cursor = provider_cursor;
                if session.status == SessionStatus::Connecting {
                    session.status = SessionStatus::Working;
                }
            }
            DriverEvent::AgentPresetSelected(agent_preset) => session.agent_preset = agent_preset,
            DriverEvent::AutoTitleUpdated(title) => {
                session.set_auto_title(title);
            }
            DriverEvent::AvailableCommands(names) => session.available_commands = names,
            DriverEvent::Permission {
                request_id,
                title,
                detail,
                options,
            } => {
                if session_accepts_turn_output(session) {
                    session.pending_permission = Some(PendingPermission {
                        request_id,
                        title,
                        detail,
                        options,
                    });
                    session.status = SessionStatus::Waiting;
                }
            }
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                if session_accepts_turn_output(session) && !questions.is_empty() {
                    session.pending_user_input = Some(UserInputRequest {
                        request_id,
                        questions,
                    });
                    session.status = SessionStatus::Waiting;
                }
            }
            DriverEvent::ComputerUseUpdated(_) => {
                session_accepts_turn_output(session);
            }
            DriverEvent::SteerAccepted { message } => {
                session.steward_wait = None;
                session.push_user_message_with_presentation(message, None, Vec::new());
                session.updated_at = unix_time();
            }
            DriverEvent::GoalUpdated(goal) => {
                if let Some(goal) = &goal
                    && session.messages.is_empty()
                {
                    session.set_title_from_prompt(&goal.objective);
                }
                session.thread_goal = goal;
            }
            DriverEvent::UsageUpdated {
                context_tokens,
                context_window,
            } => {
                let usage = session.context_usage.get_or_insert(ContextUsage::default());
                if let Some(tokens) = context_tokens {
                    usage.tokens = tokens;
                }
                if let Some(window) = context_window {
                    usage.window = Some(window);
                }
            }
            DriverEvent::HistoryPersistence { cursor, error } => {
                let same_runtime = |other: RuntimeEventCursor| {
                    other.runtime_id == cursor.runtime_id && other.epoch == cursor.epoch
                };
                let belongs_to_current = session.runtime_event_cursor.is_none_or(same_runtime);
                let not_older = session
                    .history_saved_cursor
                    .is_none_or(|saved| !same_runtime(saved) || cursor.sequence >= saved.sequence);
                if belongs_to_current && not_older {
                    if let Some(error) = error {
                        session.history_save_error = Some(error);
                    } else {
                        session.history_saved_cursor = Some(cursor);
                        if session
                            .runtime_event_cursor
                            .is_none_or(|current| cursor.sequence >= current.sequence)
                        {
                            session.history_save_error = None;
                        }
                    }
                }
            }
            DriverEvent::BackgroundWork(_)
            | DriverEvent::SteerRejected { .. }
            | DriverEvent::PlanUsageUpdated(_) => {}
        }
        effects
    }

    fn complete_reasoning_activity(&mut self, session: &mut AgentSession) {
        let reasoning = session
            .transcript_blocks
            .iter_mut()
            .rev()
            .flat_map(|block| block.activities.iter_mut().rev())
            .find(|activity| activity.reasoning.is_some() && !activity.complete);
        if let Some(reasoning) = reasoning {
            reasoning.complete = true;
            session.updated_at = unix_time();
        }
    }

    fn append_reasoning_delta(&mut self, session: &mut AgentSession, delta: String) {
        let previous_phase = self.stream_phase;
        let continuing = previous_phase == Some(StreamPhase::Reasoning);
        if !continuing && delta.trim().is_empty() {
            return;
        }
        let now = unix_time_millis();
        if !continuing {
            finish_streaming_assistant(session);
        }
        if continuing
            && let Some(reasoning) = session
                .transcript_blocks
                .last_mut()
                .and_then(|block| block.activities.last_mut())
                .and_then(|activity| activity.reasoning.as_mut())
        {
            reasoning.content.push_str(&delta);
            reasoning.finished_at_ms = now;
        } else {
            push_transcript_activity(
                session,
                ActivityItem::from_reasoning(
                    ReasoningBlock {
                        content: delta,
                        started_at_ms: now,
                        finished_at_ms: now,
                    },
                    false,
                ),
                matches!(
                    previous_phase,
                    Some(StreamPhase::Reasoning | StreamPhase::Activity)
                ),
            );
        }
        session.updated_at = unix_time();
        self.stream_phase = Some(StreamPhase::Reasoning);
    }

    fn update_activity(&mut self, session: &mut AgentSession, item: ActivityItem) -> Option<Uuid> {
        let previous_phase = self.stream_phase;
        if previous_phase == Some(StreamPhase::Text) {
            finish_streaming_assistant(session);
        }
        if previous_phase == Some(StreamPhase::Reasoning) {
            self.complete_reasoning_activity(session);
        }

        let continuing_work = matches!(
            previous_phase,
            Some(StreamPhase::Reasoning | StreamPhase::Activity)
        );
        for block in session.transcript_blocks.iter_mut().rev() {
            let matching = block.activities.iter_mut().rev().find(|activity| {
                item.source_id
                    .as_ref()
                    .is_some_and(|id| activity.source_id.as_ref() == Some(id))
                    || (item.source_id.is_none()
                        && activity.title == item.title
                        && !activity.complete)
            });
            if let Some(activity) = matching {
                let has_arguments = item.arguments.is_some();
                let replaces_changes = !item.file_changes.is_empty();
                let activity_id = activity.id;
                activity.kind = item.kind;
                activity.title = item.title;
                activity.complete = item.complete;
                activity.failed = item.failed;
                if item.detail.is_some() {
                    activity.detail = item.detail;
                }
                if item.arguments.is_some() {
                    activity.arguments = item.arguments;
                }
                if item.output.is_some() {
                    activity.output = item.output;
                }
                if !item.image_urls.is_empty() {
                    activity.image_urls = item.image_urls;
                }
                if !item.file_changes.is_empty() {
                    activity.file_changes = item.file_changes;
                }
                if item.display_target.is_some()
                    && (activity.display_target.is_none() || has_arguments)
                {
                    activity.display_target = item.display_target;
                }
                if item.display_description.is_some()
                    && (activity.display_description.is_none() || has_arguments)
                {
                    activity.display_description = item.display_description;
                }
                if item.reasoning.is_some() {
                    activity.reasoning = item.reasoning;
                }
                session.updated_at = unix_time();
                self.stream_phase = Some(StreamPhase::Activity);
                return replaces_changes.then_some(activity_id);
            }
        }

        push_transcript_activity(session, item, continuing_work);
        session.updated_at = unix_time();
        self.stream_phase = Some(StreamPhase::Activity);
        None
    }
}

fn input_delivery_activity(delivery: &InputDelivery) -> ActivityItem {
    let state = match delivery.state {
        InputDeliveryState::Accepted => tr!("session.input_accepted"),
        InputDeliveryState::Received => match delivery.confirmation {
            Some(InputConfirmation::Transport) => tr!("session.input_received_transport"),
            _ => tr!("session.input_received_provider"),
        },
        InputDeliveryState::Failed => tr!("session.input_failed"),
        InputDeliveryState::Uncertain => tr!("session.input_uncertain"),
        InputDeliveryState::Unsupported => tr!("session.input_unsupported"),
    };
    ActivityItem::new(Some(format!("input-{}", delivery.id)), ActivityKind::Tool, state,
        Some(delivery.prompt.clone()), true)
        .with_output(Some(format!("{}\n\nDelivery: {}\nTurn: {}\n{}", delivery.prompt, delivery.id, delivery.turn_id,
            delivery.reason.as_deref().unwrap_or(""))))
        .with_failed(delivery.state == InputDeliveryState::Failed)
}

pub fn finish_streaming_assistant(session: &mut AgentSession) {
    for message in &mut session.messages {
        if message.role == MessageRole::Assistant && message.streaming {
            message.streaming = false;
        }
    }
}

pub fn complete_turn_blocks(session: &mut AgentSession) {
    for block in &mut session.transcript_blocks {
        for activity in &mut block.activities {
            activity.complete = true;
        }
    }
}

pub fn turn_has_assistant_message(session: &AgentSession) -> bool {
    let Some(turn_id) = session.active_turn_id() else {
        return false;
    };
    session
        .messages
        .iter()
        .any(|message| message.role == MessageRole::Assistant && message.turn_id == Some(turn_id))
}

pub fn append_text_delta(session: &mut AgentSession, continuing: bool, delta: String) {
    if !continuing {
        finish_streaming_assistant(session);
    }
    let existing = continuing.then(|| {
        session
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.role == MessageRole::Assistant && message.streaming)
    });
    if let Some(Some(message)) = existing {
        message.content.push_str(&delta);
    } else {
        let mut message = session
            .active_turn_id()
            .map(|turn_id| Message::new_for_turn(MessageRole::Assistant, delta.clone(), turn_id))
            .unwrap_or_else(|| Message::new(MessageRole::Assistant, delta));
        message.streaming = true;
        session.messages.push(message);
    }
    session.updated_at = unix_time();
}

pub fn push_transcript_activity(
    session: &mut AgentSession,
    item: ActivityItem,
    continuing_work: bool,
) {
    let after_message = session.messages.len();
    let turn_id = session.active_turn_id();
    if continuing_work
        && let Some(block) = session.transcript_blocks.last_mut()
        && block.after_message == after_message
        && block.turn_id == turn_id
    {
        block.activities.push(item);
    } else {
        session.transcript_blocks.push(TranscriptBlock {
            after_message,
            turn_id,
            activities: vec![item],
        });
    }
}

pub fn session_accepts_turn_output(session: &mut AgentSession) -> bool {
    if session.active_turn_id().is_none() || !session.status.is_busy() {
        return false;
    }
    session.mark_active_turn_provider_started();
    if session.status == SessionStatus::Connecting {
        session.status = SessionStatus::Working;
    }
    true
}

pub fn compact_driver_error(error: &str) -> String {
    const MAX_LINES: usize = 6;
    const MAX_CHARS: usize = 800;

    let lines = error.lines().collect::<Vec<_>>();
    let mut compact = lines
        .iter()
        .take(MAX_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_LINES {
        compact.push_str("\n…");
    }
    if compact.chars().count() > MAX_CHARS {
        compact = compact.chars().take(MAX_CHARS - 1).collect();
        compact.push('…');
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steward_wait_survives_completion_and_stale_snapshot_until_new_input() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let parent_turn_id = session.begin_turn("delegate");
        let wait = StewardWait {
            id: Uuid::new_v4(),
            parent_turn_id,
            targets: vec![StewardWaitTarget {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
            }],
        };
        let cursor = RuntimeEventCursor {
            runtime_id: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            sequence: 1,
        };
        session.runtime_event_cursor = Some(cursor);
        let old = session.clone();
        let mut reducer = HistoryReducer::default();
        let wire = crate::event_to_wire(DriverEvent::StewardWaitChanged(Some(wait.clone()))).unwrap();
        reducer.apply(&mut session, crate::event_from_wire(wire).unwrap());
        assert!(!session.is_waiting_for_children());
        reducer.apply(
            &mut session,
            DriverEvent::TurnFinished {
                success: true,
                summary: None,
            },
        );
        session.runtime_event_cursor.as_mut().unwrap().sequence = 2;
        reducer.apply(&mut session, DriverEvent::HistorySnapshot(Box::new(old)));
        assert_eq!(session.steward_wait, Some(wait.clone()));
        assert!(session.is_waiting_for_children());
        let saved = session.clone();
        session.begin_turn("new user input");
        reducer.apply(&mut session, DriverEvent::HistorySnapshot(Box::new(saved)));
        reducer.apply(
            &mut session,
            DriverEvent::StewardWaitChanged(Some(wait.clone())),
        );
        assert!(session.steward_wait.is_none());
        let mut restored = session.clone();
        restored.steward_wait = Some(wait.clone());
        reducer.apply(&mut restored, DriverEvent::CancelRequested);
        assert!(restored.steward_wait.is_none());
        restored.steward_wait = Some(wait);
        reducer.apply(
            &mut restored,
            DriverEvent::PromptSubmitted {
                message: "new remote input".into(),
                turn_id: Uuid::new_v4(),
                message_id: Uuid::new_v4(),
            },
        );
        assert!(restored.steward_wait.is_none());
    }

    #[test]
    fn cancellation_waits_for_provider_and_preserves_current_turn() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let turn = session.begin_turn("task");
        let mut reducer = HistoryReducer::default();
        reducer.apply(&mut session, DriverEvent::TurnStarted);
        reducer.apply(&mut session, DriverEvent::TextDelta("partial".into()));
        reducer.apply(&mut session, DriverEvent::CancelRequested);
        reducer.apply(&mut session, DriverEvent::CancelRequested);
        assert_eq!(session.active_turn_id(), Some(turn));
        assert_eq!(session.turns.last().unwrap().status, TurnStatus::Running);
        reducer.apply(
            &mut session,
            DriverEvent::TurnFinished {
                success: true,
                summary: None,
            },
        );
        assert_eq!(
            session.turns.last().unwrap().status,
            TurnStatus::Interrupted
        );
        assert_eq!(session.messages.last().unwrap().content, "partial");
        session.begin_turn("next");
        reducer.apply(
            &mut session,
            DriverEvent::TurnFinished {
                success: true,
                summary: None,
            },
        );
        assert_eq!(session.turns.last().unwrap().status, TurnStatus::Completed);
    }

    #[test]
    fn snapshot_preserves_pending_provider_error_until_exit() {
        let mut current = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        current.begin_turn("task");
        let mut reducer = HistoryReducer::default();
        reducer.apply(&mut current, DriverEvent::TurnStarted);
        reducer.apply(
            &mut current,
            DriverEvent::Error("provider lost connection".into()),
        );
        let snapshot = serde_json::from_str(&serde_json::to_string(&current).unwrap()).unwrap();
        let mut restored = current.clone();
        let mut resumed = HistoryReducer::default();
        resumed.apply(
            &mut restored,
            DriverEvent::HistorySnapshot(Box::new(snapshot)),
        );
        resumed.apply(&mut restored, DriverEvent::ProcessExited);
        reducer.apply(&mut current, DriverEvent::ProcessExited);
        assert_eq!(
            restored.messages.last().unwrap().content,
            "provider lost connection"
        );
        assert_eq!(
            restored.messages.last().unwrap().content,
            current.messages.last().unwrap().content
        );
    }

    #[test]
    fn snapshot_restores_history_and_continues_the_saved_stream() {
        let mut saved = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        saved.begin_turn("task");
        let mut reducer = HistoryReducer::default();
        reducer.apply(&mut saved, DriverEvent::TurnStarted);
        reducer.apply(&mut saved, DriverEvent::TextDelta("preserved".into()));
        saved.parent_session_id = Some(Uuid::new_v4());
        let mut current = AgentSession::new(saved.project_id, ProviderKind::Codex);
        current.begin_turn("stale");
        reducer.apply(
            &mut current,
            DriverEvent::HistorySnapshot(Box::new(saved.clone())),
        );
        reducer.apply(&mut current, DriverEvent::TextDelta(" tail".into()));
        assert_eq!(current.messages.len(), saved.messages.len());
        assert_eq!(current.messages.last().unwrap().content, "preserved tail");
        assert_eq!(current.parent_session_id, saved.parent_session_id);
    }

    #[test]
    fn snapshot_does_not_rewind_a_later_applied_event() {
        let mut current = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        current.begin_turn("task");
        let mut reducer = HistoryReducer::default();
        reducer.apply(&mut current, DriverEvent::TurnStarted);
        reducer.apply(&mut current, DriverEvent::TextDelta("saved".into()));
        let cursor = RuntimeEventCursor {
            runtime_id: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            sequence: 8,
        };
        current.runtime_event_cursor = Some(cursor);
        let snapshot = current.clone();
        reducer.apply(&mut current, DriverEvent::TextDelta(" later".into()));
        current.runtime_event_cursor = Some(RuntimeEventCursor {
            sequence: 9,
            ..cursor
        });
        reducer.apply(
            &mut current,
            DriverEvent::HistorySnapshot(Box::new(snapshot)),
        );
        assert_eq!(current.messages.last().unwrap().content, "saved later");
        assert_eq!(current.runtime_event_cursor.unwrap().sequence, 9);
    }

    #[test]
    fn snapshot_keeps_newer_user_choices_and_local_turn() {
        let mut current = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        current.begin_turn("first task");
        current.push_message(MessageRole::Assistant, "partial");
        current.finish_active_turn(TurnStatus::Completed);
        let mut snapshot = current.clone();
        snapshot.messages.last_mut().unwrap().content = "complete saved answer".into();
        snapshot.available_commands = vec![];
        snapshot.last_driver_error = Some("previous failure".into());
        snapshot.context_usage = Some(crate::model::ContextUsage {
            tokens: 321,
            window: Some(1000),
        });
        let new_project = Uuid::new_v4();
        current.project_id = new_project;
        current.set_title("new title");
        current.model = Some("new model".into());
        current.runtime_mode = RuntimeMode::FullAccess;
        current
            .queued_messages
            .push(QueuedMessage::new("queued follow-up"));
        let newer_turn = current.begin_turn("new local task");
        current.status = SessionStatus::Connecting;
        HistoryReducer::default().apply(
            &mut current,
            DriverEvent::HistorySnapshot(Box::new(snapshot)),
        );
        assert_eq!(current.title, "new title");
        assert_eq!(current.project_id, new_project);
        assert_eq!(current.model.as_deref(), Some("new model"));
        assert_eq!(current.runtime_mode, RuntimeMode::FullAccess);
        assert_eq!(current.queued_messages[0].content, "queued follow-up");
        assert_eq!(current.active_turn_id(), Some(newer_turn));
        assert_eq!(current.messages[1].content, "complete saved answer");
        assert_eq!(current.messages[2].content, "new local task");
        assert_eq!(current.context_usage.unwrap().tokens, 321);
        assert_eq!(current.last_driver_error, None);
        assert_eq!(current.status, SessionStatus::Connecting);
    }

    #[test]
    fn rewind_keeps_user_changes_made_after_its_snapshot() {
        let mut current = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        current.begin_turn("keep this turn");
        current.push_message(MessageRole::Assistant, "retained reply");
        current.finish_active_turn(TurnStatus::Completed);
        current.begin_turn("replace this turn");
        current.finish_active_turn(TurnStatus::Completed);
        let mut rewound = current.clone();
        rewound.truncate_after_turn(1);
        rewound.status = SessionStatus::Idle;
        rewound.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "rewound-thread".into(),
        });
        let cursor = RuntimeEventCursor {
            runtime_id: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            sequence: 8,
        };
        rewound.runtime_event_cursor = Some(cursor);
        rewound.history_saved_cursor = Some(cursor);
        current.set_title("renamed during rewind");
        current.model = Some("new-model".into());
        current.reasoning_effort = Some("high".into());
        current.service_tier = Some("priority".into());
        current.runtime_mode = RuntimeMode::Ask;
        current
            .queued_messages
            .push(QueuedMessage::new("new follow-up"));
        current.pending_permission = Some(PendingPermission {
            request_id: "old-approval".into(),
            title: "Approve".into(),
            detail: String::new(),
            options: Vec::new(),
        });
        current.pending_user_input = Some(UserInputRequest {
            request_id: "old-question".into(),
            questions: Vec::new(),
        });
        current.updated_at = rewound.updated_at + 1;
        let latest_update = current.updated_at;

        apply_rewound_history(&mut current, rewound);

        assert_eq!(current.title, "renamed during rewind");
        assert_eq!(current.model.as_deref(), Some("new-model"));
        assert_eq!(current.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(current.service_tier.as_deref(), Some("priority"));
        assert_eq!(current.runtime_mode, RuntimeMode::Ask);
        assert_eq!(current.queued_messages[0].content, "new follow-up");
        assert_eq!(current.updated_at, latest_update);
        assert_eq!(current.turns.len(), 1);
        assert_eq!(current.messages.len(), 2);
        assert_eq!(current.messages[1].content, "retained reply");
        assert_eq!(current.provider_native_id(), Some("rewound-thread"));
        assert_eq!(current.status, SessionStatus::Idle);
        assert_eq!(current.history_saved_cursor, Some(cursor));
        assert!(current.pending_permission.is_none());
        assert!(current.pending_user_input.is_none());
    }
    use crate::model::{AgentSession, DriverEvent, MessageRole, ProviderKind};
    use uuid::Uuid;

    #[test]
    fn waiting_interactions_survive_hydration_and_clear_only_after_matching_response() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut history = HistoryReducer::default();
        history.apply(&mut session, DriverEvent::TurnStarted);
        history.apply(
            &mut session,
            DriverEvent::Permission {
                request_id: "approval".into(),
                title: "Run checks".into(),
                detail: "cargo test".into(),
                options: vec![PermissionOption {
                    id: "allow".into(),
                    label: "Allow".into(),
                    allow: true,
                }],
            },
        );
        let serialized = serde_json::to_value(&session).unwrap();
        assert_eq!(serialized["pending_permission"]["requestId"], "approval");
        let mut restored: AgentSession = serde_json::from_value(serialized).unwrap();
        history.apply(
            &mut restored,
            DriverEvent::InteractionResponded {
                request_id: "stale".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(&restored).unwrap()["pending_permission"]["requestId"],
            "approval"
        );
        history.apply(
            &mut restored,
            DriverEvent::InteractionResponded {
                request_id: "approval".into(),
            },
        );
        assert!(
            serde_json::to_value(&restored)
                .unwrap()
                .get("pending_permission")
                .is_none()
        );
        history.apply(
            &mut restored,
            DriverEvent::UserInputRequested {
                request_id: "question".into(),
                questions: vec![UserInputQuestion {
                    id: "q1".into(),
                    header: "Scope".into(),
                    question: "Which files?".into(),
                    options: vec![],
                    multi_select: false,
                }],
            },
        );
        assert_eq!(
            serde_json::to_value(&restored).unwrap()["pending_user_input"]["requestId"],
            "question"
        );
        history.apply(
            &mut restored,
            DriverEvent::TurnFinished {
                success: true,
                summary: None,
            },
        );
        assert!(
            serde_json::to_value(&restored)
                .unwrap()
                .get("pending_user_input")
                .is_none()
        );
    }

    #[test]
    fn user_actions_preserve_cancel_and_respond_history() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut history = HistoryReducer::default();
        history.apply(
            &mut session,
            DriverEvent::PromptSubmitted {
                message: "inspect".into(),
                turn_id: Uuid::new_v4(),
                message_id: Uuid::new_v4(),
            },
        );
        history.apply(&mut session, DriverEvent::TurnStarted);
        session.status = SessionStatus::Waiting;
        let response = crate::driver_wire::event_from_wire(crate::protocol::WireDriverEvent::new(
            "interactionResponded",
            serde_json::json!({"request_id": "approval"}),
        ))
        .unwrap();
        history.apply(&mut session, response);
        assert_eq!(session.status, SessionStatus::Working);
        let cancel = crate::driver_wire::event_from_wire(crate::protocol::WireDriverEvent::new(
            "cancelRequested",
            serde_json::Value::Null,
        ))
        .unwrap();
        history.apply(&mut session, cancel);
        assert_eq!(session.turns[0].status, TurnStatus::Running);
        assert_eq!(session.status, SessionStatus::Working);
        history.apply(
            &mut session,
            DriverEvent::TurnFinished {
                success: false,
                summary: None,
            },
        );
        history.apply(&mut session, DriverEvent::ProcessExited);
        assert_eq!(session.turns[0].status, TurnStatus::Interrupted);
        assert_eq!(session.status, SessionStatus::Idle);
    }

    #[test]
    fn submitted_prompt_echo_and_text_form_one_turn() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut history = HistoryReducer::default();
        let turn_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        for event in [
            DriverEvent::PromptSubmitted {
                message: "Inspect the repository".into(),
                turn_id,
                message_id,
            },
            DriverEvent::PromptSubmitted {
                message: "Inspect the repository".into(),
                turn_id,
                message_id,
            },
            DriverEvent::TurnStarted,
            DriverEvent::TextDelta("Found ".into()),
            DriverEvent::TextDelta("the entry point".into()),
        ] {
            history.apply(&mut session, event);
        }
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].id, message_id);
        assert_eq!(session.messages[1].role, MessageRole::Assistant);
        assert_eq!(session.messages[1].content, "Found the entry point");
        assert_eq!(session.messages[1].turn_id, Some(turn_id));
        assert!(session.turns[0].provider_turn_started);
    }
    #[test]
    fn interleaved_reasoning_tools_and_text_preserve_order_and_tool_details() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let mut history = HistoryReducer::default();
        let mut tool = ActivityItem::new(
            Some("read-1".into()),
            ActivityKind::Command,
            "Read",
            Some("started".into()),
            false,
        )
        .with_arguments(Some(r#"{"command":"cat README.md"}"#.into()));
        tool.display_target = Some("README.md".into());
        let tool_id = tool.id;
        let mut result = ActivityItem::new(
            Some("read-1".into()),
            ActivityKind::Command,
            "Read completed",
            None,
            true,
        )
        .with_output(Some("repository instructions".into()));
        result.display_target = Some("fallback".into());
        for event in [
            DriverEvent::TurnStarted,
            DriverEvent::ReasoningDelta(" ".into()),
            DriverEvent::ReasoningDelta("Inspect".into()),
            DriverEvent::ReasoningDelta(" first".into()),
            DriverEvent::TextDelta("Opening files".into()),
            DriverEvent::RichActivity(tool),
            DriverEvent::TextDelta("Read file".into()),
            DriverEvent::RichActivity(result),
            DriverEvent::TextDelta("Done".into()),
        ] {
            history.apply(&mut session, event);
        }
        assert_eq!(
            session
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["Opening files", "Read file", "Done"]
        );
        assert_eq!(session.transcript_blocks.len(), 2);
        let reasoning = &session.transcript_blocks[0];
        assert_eq!(reasoning.after_message, 0);
        assert_eq!(
            reasoning.activities[0].reasoning.as_ref().unwrap().content,
            "Inspect first"
        );
        assert!(reasoning.activities[0].complete);
        let block = &session.transcript_blocks[1];
        assert_eq!(block.after_message, 1);
        assert_eq!(block.activities.len(), 1);
        let activity = &block.activities[0];
        assert_eq!(activity.id, tool_id);
        assert_eq!(activity.title, "Read completed");
        assert_eq!(activity.detail.as_deref(), Some("started"));
        assert_eq!(activity.arguments.as_deref(), Some("cat README.md"));
        assert_eq!(activity.output.as_deref(), Some("repository instructions"));
        assert_eq!(activity.display_target.as_deref(), Some("README.md"));
        assert!(activity.complete);
        assert!(!session.messages[0].streaming);
        assert!(!session.messages[1].streaming);
    }

    #[test]
    fn parked_turn_resumes_and_settles_once_with_exit_fallback() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let mut history = HistoryReducer::default();
        history.apply(&mut session, DriverEvent::TurnStarted);
        let first_turn = session.active_turn_id();
        history.apply(
            &mut session,
            DriverEvent::TextDelta("Waiting for checks".into()),
        );
        history.apply(&mut session, DriverEvent::TurnParked);
        assert_eq!(session.status, SessionStatus::Background);
        assert_eq!(session.active_turn_id(), first_turn);
        assert!(!session.messages[0].streaming);
        history.apply(&mut session, DriverEvent::TurnStarted);
        history.apply(&mut session, DriverEvent::TextDelta("Checks passed".into()));
        let settled = history.apply(
            &mut session,
            DriverEvent::TurnFinished {
                success: true,
                summary: Some("must not replace output".into()),
            },
        );
        assert_eq!(settled.finished_turn.map(|(id, _)| id), first_turn);
        assert_eq!(session.turns[0].status, TurnStatus::Completed);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "Checks passed");
        assert!(
            history
                .apply(
                    &mut session,
                    DriverEvent::TurnFinished {
                        success: true,
                        summary: None
                    }
                )
                .finished_turn
                .is_none()
        );
        history.apply(&mut session, DriverEvent::TextDelta("late output".into()));
        assert_eq!(session.messages.len(), 2);
        history.apply(&mut session, DriverEvent::TurnStarted);
        history.apply(
            &mut session,
            DriverEvent::Error("provider disconnected".into()),
        );
        let exited = history.apply(&mut session, DriverEvent::ProcessExited);
        assert!(exited.finished_turn.is_some());
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(session.turns[1].status, TurnStatus::Failed);
        assert_eq!(session.messages[2].content, "provider disconnected");
        assert_eq!(session.messages[2].turn_id, Some(session.turns[1].id));
        history.apply(&mut session, DriverEvent::ProcessExited);
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn provider_metadata_and_live_steer_survive_projection() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let mut history = HistoryReducer::default();
        history.apply(&mut session, DriverEvent::TurnStarted);
        let turn_id = session.active_turn_id();
        for event in [
            DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Claude {
                    session_id: "native-session".into(),
                    resume_at: Some("native-message".into()),
                }),
            },
            DriverEvent::AutoTitleUpdated(Some("Inspection".into())),
            DriverEvent::AgentPresetSelected(Some("reviewer".into())),
            DriverEvent::AvailableCommands(vec![ReportedCommand {
                name: "review".into(),
                description: "Review changes".into(),
            }]),
            DriverEvent::UsageUpdated {
                context_tokens: Some(120),
                context_window: None,
            },
            DriverEvent::UsageUpdated {
                context_tokens: None,
                context_window: Some(200_000),
            },
            DriverEvent::SteerAccepted {
                message: "Also inspect tests".into(),
            },
            DriverEvent::Permission {
                request_id: "permit-1".into(),
                title: "Run tests".into(),
                detail: "cargo test".into(),
                options: vec![],
            },
        ] {
            history.apply(&mut session, event);
        }
        assert_eq!(
            session.turns[0].provider_resume_at.as_deref(),
            Some("native-message")
        );
        assert_eq!(session.auto_title.as_deref(), Some("Inspection"));
        assert_eq!(session.agent_preset.as_deref(), Some("reviewer"));
        assert_eq!(session.available_commands[0].name, "review");
        assert_eq!(session.context_usage.as_ref().unwrap().tokens, 120);
        assert_eq!(
            session.context_usage.as_ref().unwrap().window,
            Some(200_000)
        );
        assert_eq!(session.messages[0].content, "Also inspect tests");
        assert_eq!(session.messages[0].turn_id, turn_id);
        assert_eq!(session.status, SessionStatus::Waiting);
    }

    #[test]
    fn saving_acknowledgements_do_not_regress_or_clear_a_newer_failure() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut history = HistoryReducer::default();
        let current = RuntimeEventCursor {
            runtime_id: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            sequence: 10,
        };
        history.apply(
            &mut session,
            DriverEvent::RuntimeEventCursorAdvanced(current),
        );
        history.apply(
            &mut session,
            DriverEvent::HistoryPersistence {
                cursor: RuntimeEventCursor {
                    sequence: 8,
                    ..current
                },
                error: None,
            },
        );
        assert_eq!(session.history_saved_cursor.unwrap().sequence, 8);
        history.apply(
            &mut session,
            DriverEvent::HistoryPersistence {
                cursor: current,
                error: Some("disk full".into()),
            },
        );
        history.apply(
            &mut session,
            DriverEvent::HistoryPersistence {
                cursor: RuntimeEventCursor {
                    sequence: 4,
                    ..current
                },
                error: None,
            },
        );
        assert_eq!(session.history_saved_cursor.unwrap().sequence, 8);
        assert_eq!(session.history_save_error.as_deref(), Some("disk full"));
        history.apply(
            &mut session,
            DriverEvent::HistoryPersistence {
                cursor: RuntimeEventCursor {
                    runtime_id: Uuid::new_v4(),
                    sequence: 99,
                    ..current
                },
                error: None,
            },
        );
        assert_eq!(session.history_saved_cursor.unwrap().sequence, 8);
        assert_eq!(session.history_save_error.as_deref(), Some("disk full"));
        history.apply(
            &mut session,
            DriverEvent::HistoryPersistence {
                cursor: current,
                error: None,
            },
        );
        assert_eq!(session.history_saved_cursor, Some(current));
        assert_eq!(session.history_save_error, None);
    }
}
