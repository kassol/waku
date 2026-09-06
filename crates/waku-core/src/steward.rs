//! Read-only queries over daemon-owned, hydrated child sessions.
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

    fn child_summary(&self, session: &AgentSession) -> ChildSessionSummary {
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
