use super::*;
#[cfg(test)]
pub(super) use history::push_transcript_activity;
pub(super) use history::{compact_driver_error, session_accepts_turn_output};
use waku_protocol::history;

impl Waku {
    fn apply_history_event(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        event: DriverEvent,
    ) -> history::HistoryEffects {
        let outcome = match &event {
            DriverEvent::TurnFinished { success: true, .. } => {
                Some(crate::analytics::TurnOutcome::Completed)
            }
            DriverEvent::TurnFinished { success: false, .. } => {
                Some(crate::analytics::TurnOutcome::Failed)
            }
            DriverEvent::ProcessExited => Some(crate::analytics::TurnOutcome::ProcessExited),
            _ => None,
        };
        let analytics_event =
            outcome.and_then(|outcome| self.active_turn_finished_event(session_id, outcome));
        let mut reducer = history::HistoryReducer {
            stream_phase: runtime.stream_phase,
            last_driver_error: runtime.last_driver_error.take(),
        };
        let effects = self
            .state
            .session_mut(session_id)
            .map(|session| reducer.apply(session, event))
            .unwrap_or_default();
        runtime.stream_phase = reducer.stream_phase;
        runtime.last_driver_error = reducer.last_driver_error;
        if let Some(activity_id) = effects.invalidated_activity_diff {
            self.activity_diffs.borrow_mut().remove(&activity_id);
        }
        if effects.finished_turn.is_some()
            && let Some(event) = analytics_event
        {
            self.analytics.track(event);
        }
        effects
    }

    pub(super) fn finish_streaming_assistant(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            history::finish_streaming_assistant(session);
        }
    }

    pub(super) fn complete_turn_blocks(&mut self, session_id: Uuid) {
        if let Some(session) = self.state.session_mut(session_id) {
            history::complete_turn_blocks(session);
        }
    }

    pub(super) fn turn_has_assistant_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(history::turn_has_assistant_message)
    }

    /// Whether the running turn was prompted — a provider-started wake has no
    /// user message of its own.
    pub(super) fn active_turn_has_user_message(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                let turn_id = session.active_turn_id()?;
                Some(session.messages.iter().any(|message| {
                    message.turn_id == Some(turn_id) && message.role == MessageRole::User
                }))
            })
            .unwrap_or(false)
    }

    pub(super) fn accepts_turn_output(&mut self, session_id: Uuid) -> bool {
        // The turn begins at submission accept, before its prompt has reached
        // any provider. While preparation is still running, a reused runtime
        // could only be draining leftovers of a settled turn — output landing
        // in the new turn then would attribute stale text to it.
        if self.submission_preparations.contains(&session_id) {
            return false;
        }
        self.state
            .session_mut(session_id)
            .is_some_and(session_accepts_turn_output)
    }

    /// Returns whether the runtime should remain attached after this event.
    ///
    /// `allow_queue_drain` is false when the caller is flushing buffered
    /// events for a turn the user just stopped: a settling event must not
    /// start queued follow-ups then, because the user asked to stop, not to
    /// continue.
    pub(super) fn handle_driver_event(
        &mut self,
        session_id: Uuid,
        runtime: &mut SessionRuntime,
        event: DriverEvent,
        allow_queue_drain: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        runtime.last_active_at = Instant::now();
        match event {
            DriverEvent::HistorySnapshot(snapshot) => {
                runtime.pending_permission = snapshot.pending_permission.clone();
                if runtime
                    .pending_user_input
                    .as_ref()
                    .map(|request| &request.request_id)
                    != snapshot
                        .pending_user_input
                        .as_ref()
                        .map(|request| &request.request_id)
                {
                    runtime.pending_user_input =
                        snapshot.pending_user_input.clone().map(|request| {
                            PendingUserInput::new(request.request_id, request.questions)
                        });
                    if self.state.selected_session == Some(session_id) {
                        self.user_input_answer
                            .update(cx, |input, cx| input.clear(cx));
                    }
                }
                // The same message IDs can now hold a much longer saved prefix.
                if let Some(session) = self.state.session_mut(session_id) {
                    let mut markdown = self.message_markdown.borrow_mut();
                    for message in session.messages.iter().chain(snapshot.messages.iter()) {
                        markdown.remove(&message.id);
                    }
                }
                self.activity_diffs.borrow_mut().clear();
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::HistorySnapshot(snapshot),
                );
                if self.state.selected_session == Some(session_id) {
                    self.reset_visible_state();
                    self.reset_transcript_rows(self.transcript_row_count());
                }
            }
            event @ (DriverEvent::RuntimeEventCursorAdvanced(_)
            | DriverEvent::HistoryPersistence { .. }
            | DriverEvent::AgentPresetSelected(_)
            | DriverEvent::AutoTitleUpdated(_)
            | DriverEvent::PromptSubmitted { .. }
            | DriverEvent::TurnStarted) => {
                self.apply_history_event(session_id, runtime, event);
            }
            DriverEvent::CancelRequested => {
                runtime.pending_permission = None;
                runtime.pending_user_input = None;
                self.apply_history_event(session_id, runtime, DriverEvent::CancelRequested);
            }
            DriverEvent::InteractionResponded { request_id } => {
                if runtime
                    .pending_permission
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    runtime.pending_permission = None;
                }
                if runtime
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    runtime.pending_user_input = None;
                }
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::InteractionResponded { request_id },
                );
            }
            DriverEvent::Connected { provider_cursor } => {
                runtime.last_background_refresh_at = Instant::now();
                runtime.driver.refresh_background_work();
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::Connected { provider_cursor },
                );
            }
            DriverEvent::AvailableCommands(names) => {
                self.composer_sources_stale |= self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| session.available_commands != names);
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::AvailableCommands(names),
                );
            }
            DriverEvent::TurnParked => {
                // The reply ended while detached work the provider will wake
                // the session for is still running. The turn stays open for
                // that wake; only its streaming state settles, and the session
                // shows the wait instead of a finish. A prompted turn announces
                // the wait once; a wake that parks again stays quiet.
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                self.settle_foreground_work(session_id, BackgroundWorkStatus::Completed);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                let announce = cx.active_window().is_none()
                    && !runtime.park_announced
                    && self.active_turn_has_user_message(session_id);
                let task_notification = announce
                    .then(|| {
                        self.state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .map(|session| {
                                if session.display_title() == AgentSession::DEFAULT_TITLE {
                                    tr!("session.new_task")
                                } else {
                                    session.display_title().to_owned()
                                }
                            })
                    })
                    .flatten();
                self.apply_history_event(session_id, runtime, DriverEvent::TurnParked);
                runtime.park_announced = true;
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some(title) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &tr!("session.turn_waiting_background"),
                        cx,
                    );
                }
            }
            DriverEvent::TextDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.apply_history_event(session_id, runtime, DriverEvent::TextDelta(delta));
                }
            }
            DriverEvent::ReasoningDelta(delta) => {
                if self.accepts_turn_output(session_id) {
                    self.apply_history_event(
                        session_id,
                        runtime,
                        DriverEvent::ReasoningDelta(delta),
                    );
                }
            }
            DriverEvent::Activity {
                id,
                kind,
                title,
                detail,
                complete,
            } => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch = should_refresh_branch_after_activity(kind, complete)
                        && self.state.selected_session == Some(session_id);
                    let item = ActivityItem::new(id, kind, title, detail, complete);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.apply_history_event(session_id, runtime, DriverEvent::RichActivity(item));
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::RichActivity(item) => {
                if self.accepts_turn_output(session_id) {
                    let refresh_branch =
                        should_refresh_branch_after_activity(item.kind, item.complete)
                            && self.state.selected_session == Some(session_id);
                    self.observe_foreground_command_activity(session_id, &item);
                    self.apply_history_event(session_id, runtime, DriverEvent::RichActivity(item));
                    if refresh_branch {
                        self.refresh_selected_branch_snapshot(cx);
                    }
                }
            }
            DriverEvent::BackgroundWork(event) => {
                // Background work is session state, not turn output. It must
                // survive a settled or rewound turn and therefore bypasses
                // `accepts_turn_output` deliberately.
                self.handle_background_work_event(session_id, event);
            }
            DriverEvent::Permission {
                request_id,
                title,
                detail,
                options,
            } => {
                if self.accepts_turn_output(session_id) {
                    self.apply_history_event(
                        session_id,
                        runtime,
                        DriverEvent::Permission {
                            request_id: request_id.clone(),
                            title: title.clone(),
                            detail: detail.clone(),
                            options: options.clone(),
                        },
                    );
                    runtime.pending_permission = Some(PendingPermission {
                        request_id,
                        title,
                        detail,
                        options,
                    });
                }
            }
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                if self.accepts_turn_output(session_id) && !questions.is_empty() {
                    self.apply_history_event(
                        session_id,
                        runtime,
                        DriverEvent::UserInputRequested {
                            request_id: request_id.clone(),
                            questions: questions.clone(),
                        },
                    );
                    runtime.pending_user_input = Some(PendingUserInput::new(request_id, questions));
                    if self.state.selected_session == Some(session_id) {
                        self.user_input_answer
                            .update(cx, |input, cx| input.clear(cx));
                    }
                }
            }
            DriverEvent::ComputerUseUpdated(state) => {
                if self.accepts_turn_output(session_id) {
                    Self::upsert_computer_use_preview(runtime, state);
                }
            }
            DriverEvent::SteerAccepted { message } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    // Providers normally echo the exact transport text, but a
                    // normalized echo still acknowledges the oldest pending
                    // steer. Preserve its attachment presentation metadata.
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message.clone()));
                // The provider folded the message into the live turn. Append
                // it to the same turn so the transcript mirrors the provider
                // conversation (no new turn boundary).
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::SteerAccepted { message },
                );
                if let Some(message) = self
                    .state
                    .session_mut(session_id)
                    .and_then(|session| session.messages.last_mut())
                {
                    message.display_content = submission.display_content;
                    message.attachments = submission.attachments;
                }
            }
            DriverEvent::SteerRejected { message, reason } => {
                let submission = runtime
                    .pending_steers
                    .iter()
                    .position(|submission| submission.prompt == message)
                    .and_then(|index| runtime.pending_steers.remove(index))
                    .or_else(|| runtime.pending_steers.pop_front())
                    .unwrap_or_else(|| ComposerSubmission::plain(message));
                let (busy, settled_cleanly) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| {
                        let settled_cleanly = session
                            .turns
                            .last()
                            .is_some_and(|turn| turn.status == TurnStatus::Completed);
                        (session.is_busy(), settled_cleanly)
                    })
                    .unwrap_or((false, false));
                if busy {
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                    if self.state.selected_session == Some(session_id) {
                        self.show_toast(tr!(
                            "session.steer_rejected",
                            error = compact_driver_error(&reason)
                        ));
                    }
                } else if settled_cleanly {
                    // The turn settled before the steer arrived; run the
                    // message as a fresh turn instead of losing it. Submission
                    // is deferred through the queue-drain pass because this
                    // session's runtime is detached from the map while its
                    // events are handled — an inline submit would spawn a
                    // second driver process only to have it clobbered when the
                    // drain re-inserts the detached runtime.
                    if let Some(session) = self.state.session_mut(session_id) {
                        session
                            .queued_messages
                            .insert(0, submission.into_queued_message());
                    }
                    if allow_queue_drain {
                        self.pending_queue_drains.push(session_id);
                    }
                } else {
                    // The user stopped the turn (or the provider died) before
                    // the steer landed. Keep the message visible and
                    // user-controlled instead of auto-running it.
                    self.enqueue_follow_up_submission(session_id, submission, cx);
                }
            }
            DriverEvent::PlanUsageUpdated(usage) => {
                if let Some(provider) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider)
                {
                    self.plan_usage.insert(provider, usage);
                }
            }
            DriverEvent::GoalUpdated(goal) => {
                // Conversation meta like usage: it applies regardless of turn
                // state, and `None` means the provider cleared the goal.
                if goal.is_some() {
                    self.goal_observed_at.insert(session_id, Instant::now());
                } else {
                    self.goal_observed_at.remove(&session_id);
                }
                self.apply_history_event(session_id, runtime, DriverEvent::GoalUpdated(goal));
            }
            event @ DriverEvent::UsageUpdated { .. } => {
                self.apply_history_event(session_id, runtime, event);
            }
            DriverEvent::TurnFinished { success, summary } => {
                self.settle_foreground_work(
                    session_id,
                    if success {
                        BackgroundWorkStatus::Completed
                    } else {
                        BackgroundWorkStatus::Failed
                    },
                );
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                runtime.last_driver_error = None;
                // A settled turn moved the account's rate-limit needles; ask
                // that provider's plan meter to refresh once its backoff
                // allows.
                if let Some(provider) = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider)
                    .filter(|provider| usage_meter::PLAN_USAGE_PROVIDERS.contains(provider))
                {
                    self.plan_usage_stale.insert(provider);
                }
                if self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(AgentSession::active_turn_id)
                    .is_none()
                {
                    return true;
                }
                let task_notification = cx.active_window().is_none().then(|| {
                    self.state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| {
                            let title = if session.display_title() == AgentSession::DEFAULT_TITLE {
                                tr!("session.new_task")
                            } else {
                                session.display_title().to_owned()
                            };
                            let body = if success {
                                tr!("session.turn_completed")
                            } else {
                                tr!("session.stopped")
                            };
                            (title, body)
                        })
                });
                self.apply_history_event(
                    session_id,
                    runtime,
                    DriverEvent::TurnFinished { success, summary },
                );
                runtime.park_announced = false;
                runtime.pending_permission = None;
                runtime.pending_user_input = None;
                runtime.pending_computer_approval = None;
                runtime.driver.cancel_computer_use();
                // The agent may have edited files or switched branches, so the
                // cached view of the workspace is no longer trustworthy. This
                // handler has no `Context`, so the drain loop acts on the flag.
                if self.state.selected_session == Some(session_id) {
                    self.workspace_queries_stale = true;
                }
                runtime.computer_use_previews.clear();
                runtime.driver.refresh_background_work();
                self.capture_latest_turn_checkpoint_for(session_id);
                if allow_queue_drain && success {
                    // Start the next queued follow-up once the runtime has
                    // been re-inserted so the same process is reused.
                    self.pending_queue_drains.push(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                if let Some(Some((title, body))) = task_notification {
                    crate::platform::show_task_notification(
                        &task_notification_tag(session_id),
                        &title,
                        &body,
                        cx,
                    );
                }
            }
            DriverEvent::Error(error) => {
                let error = compact_driver_error(&error);
                if self.state.selected_session == Some(session_id) {
                    self.show_toast(error.clone());
                }
                self.apply_history_event(session_id, runtime, DriverEvent::Error(error));
            }
            DriverEvent::ProcessExited => {
                self.mark_background_work_lost(session_id);
                let previous_kinds = self.snapshot_selected_transcript_rows(session_id);
                let effects =
                    self.apply_history_event(session_id, runtime, DriverEvent::ProcessExited);
                runtime.pending_permission = None;
                runtime.pending_user_input = None;
                runtime.pending_computer_approval = None;
                runtime.driver.cancel_computer_use();
                runtime.computer_use_previews.clear();
                if effects.finished_turn.is_some() {
                    self.capture_latest_turn_checkpoint_for(session_id);
                }
                if let Some(previous_kinds) = previous_kinds.as_deref() {
                    self.splice_active_transcript_rows_after_visibility_change(previous_kinds);
                }
                return false;
            }
        }
        true
    }

    fn upsert_computer_use_preview(runtime: &mut SessionRuntime, state: ComputerUseState) {
        if !state.visible {
            return;
        }
        let Some(window_id) = state.target.as_ref().map(|target| target.window_id) else {
            return;
        };
        let mut preview = ComputerUsePreview {
            target: state.target,
            phase: state.phase,
            visible: state.visible,
            screenshot: state.image_url.as_deref().and_then(|image_url| {
                crate::computer_use::decode_preview_image_url(image_url).ok()
            }),
        };
        if let Some(index) = runtime.computer_use_previews.iter().position(|preview| {
            preview
                .target
                .as_ref()
                .is_some_and(|target| target.window_id == window_id)
        }) {
            let previous = runtime.computer_use_previews.remove(index);
            if preview.screenshot.is_none() {
                preview.screenshot = previous.screenshot;
            }
        }
        runtime.computer_use_previews.push(preview);
    }
}

/// A completed edit or shell command is the earliest provider-neutral point at
/// which its filesystem effects are stable enough to re-read. The actual Git
/// work remains behind the branch cache's background fetch.
pub(super) fn should_refresh_branch_after_activity(
    kind: crate::model::ActivityKind,
    complete: bool,
) -> bool {
    complete
        && matches!(
            kind,
            crate::model::ActivityKind::Command | crate::model::ActivityKind::FileChange
        )
}

pub(super) fn stream_delta_kind(event: &DriverEvent) -> Option<StreamDeltaKind> {
    match event {
        DriverEvent::TextDelta(_) => Some(StreamDeltaKind::Text),
        DriverEvent::ReasoningDelta(_) => Some(StreamDeltaKind::Reasoning),
        _ => None,
    }
}

pub(super) fn stream_delta_text(event: &DriverEvent, kind: StreamDeltaKind) -> Option<&str> {
    match (kind, event) {
        (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
        | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => Some(text),
        _ => None,
    }
}

/// Coalesce every adjacent delta of one kind while retaining provider order.
/// Runtime cursors are acknowledgements rather than visible boundaries, so the
/// newest cursor follows the combined delta. The full text enters layout in
/// this pass; Markdown's paint-only veil provides the progressive dissolve.
pub(super) fn pop_stream_batch(
    events: &mut VecDeque<DriverEvent>,
    kind: StreamDeltaKind,
) -> Option<DriverEvent> {
    let mut chunk = String::new();
    let mut latest_cursor = None;
    loop {
        match events.front() {
            Some(DriverEvent::RuntimeEventCursorAdvanced(_)) => {
                latest_cursor = events.pop_front();
            }
            Some(event) if stream_delta_text(event, kind).is_some() => {
                let event = events.pop_front()?;
                match (kind, event) {
                    (StreamDeltaKind::Text, DriverEvent::TextDelta(text))
                    | (StreamDeltaKind::Reasoning, DriverEvent::ReasoningDelta(text)) => {
                        chunk.push_str(&text);
                    }
                    _ => unreachable!("the stream kind was checked before removing the event"),
                }
            }
            _ => break,
        }
    }
    if let Some(cursor) = latest_cursor {
        events.push_front(cursor);
    }
    match kind {
        StreamDeltaKind::Text => Some(DriverEvent::TextDelta(chunk)),
        StreamDeltaKind::Reasoning => Some(DriverEvent::ReasoningDelta(chunk)),
    }
}

#[cfg(test)]
pub(super) fn append_text_delta_to_session(
    sessions: &mut [AgentSession],
    session_id: Uuid,
    continuing: bool,
    delta: String,
) {
    if let Some(session) = sessions.iter_mut().find(|session| session.id == session_id) {
        history::append_text_delta(session, continuing, delta);
    }
}
