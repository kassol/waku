//! Explicit continuation is separate from opening archived history.
use super::*;
use waku_protocol::model::{ChildContinuation, InputDeliveryState, StewardLifecycleOperation};

gpui::actions!(waku_continuation, [CloseContinuation, SubmitContinuation]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", CloseContinuation, Some("Continuation")),
        gpui::KeyBinding::new(
            "secondary-enter",
            SubmitContinuation,
            Some("Continuation > TextInput"),
        ),
    ]);
}

pub(super) struct ContinuationDialog {
    source: Uuid,
    completion: Uuid,
    manager: Uuid,
    title: String,
    visible: bool,
    input: Entity<TextInput>,
    attempt: Option<(Uuid, String)>,
    result: Option<ChildContinuation>,
    pending: bool,
    retryable: bool,
    generation: Uuid,
    error: Option<String>,
    send_focus: FocusHandle,
    query_focus: FocusHandle,
    source_focus: FocusHandle,
    result_focus: FocusHandle,
    close_focus: FocusHandle,
    return_focus: FocusHandle,
}

impl Waku {
    pub(super) fn render_continue_child_entry(
        &self,
        source: Uuid,
        completion: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.transcript_control_focus(format!("continue-child-{completion}"), cx);
        super::decisions::decision_answer_control(
            "continue-child",
            tr!("continuation.open"),
            &focus,
            true,
            Theme::current(cx),
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_continuation(source, completion, window, cx)
        }))
        .into_any_element()
    }

    fn open_continuation(
        &mut self,
        source: Uuid,
        completion: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(child) = self.state.sessions.iter().find(|s| s.id == source) else {
            return;
        };
        let Some(saved) = child.completions.iter().find(|c| c.id == completion) else {
            return;
        };
        let manager = saved.manager_session_id;
        let title = child.display_title().to_owned();
        let previous = child
            .continuations
            .iter()
            .rev()
            .find(|c| c.source_completion_id == completion)
            .cloned();
        let return_focus =
            self.transcript_control_focus(format!("continue-child-{completion}"), cx);
        if let Some(dialog) = self
            .continuation_dialog
            .as_mut()
            .filter(|d| d.source == source && d.completion == completion)
        {
            dialog.visible = true;
            dialog.return_focus = return_focus;
            window.focus(&dialog.input.read(cx).focus(), cx);
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .multi_line()
                .placeholder(tr!("continuation.instruction"))
        });
        if let Some(record) = &previous {
            input.update(cx, |input, cx| {
                input.set_content(record.instruction.clone(), cx);
                input.set_read_only(true);
            });
        }
        window.focus(&input.read(cx).focus(), cx);
        self.continuation_dialog = Some(ContinuationDialog {
            source,
            completion,
            manager,
            title,
            visible: true,
            input,
            attempt: previous.as_ref().map(|r| (r.id, r.instruction.clone())),
            result: previous,
            pending: false,
            retryable: false,
            generation: Uuid::nil(),
            error: None,
            send_focus: cx.focus_handle().tab_stop(true),
            query_focus: cx.focus_handle().tab_stop(true),
            source_focus: cx.focus_handle().tab_stop(true),
            result_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            return_focus,
        });
        cx.notify();
    }

    fn request_continuation(&mut self, query: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.continuation_dialog.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let failed = dialog
            .result
            .as_ref()
            .is_some_and(|r| r.state == InputDeliveryState::Failed);
        if !query && failed {
            dialog.attempt = None;
            dialog.result = None;
        }
        if !query && dialog.attempt.is_some() && !dialog.retryable {
            return;
        }
        if !query && dialog.attempt.is_none() {
            let instruction = dialog.input.read(cx).content().trim().to_owned();
            if instruction.is_empty() {
                return;
            }
            dialog.attempt = Some((Uuid::new_v4(), instruction));
        }
        let Some((operation, instruction)) = dialog.attempt.clone() else {
            return;
        };
        let source = dialog.source;
        let manager = dialog.manager;
        let generation = Uuid::new_v4();
        let command = if query {
            waku_client::Command::StewardLifecycle {
                operation: StewardLifecycleOperation::Status {
                    session_id: source,
                    operation_id: operation,
                },
            }
        } else {
            waku_client::Command::ContinueChild {
                child_session_id: source,
                completion_id: dialog.completion,
                operation_id: operation,
                instruction,
            }
        };
        dialog.pending = true;
        dialog.retryable = false;
        dialog.error = None;
        dialog.generation = generation;
        dialog
            .input
            .update(cx, |input, _| input.set_read_only(true));
        window.focus(&dialog.input.read(cx).focus(), cx);
        let daemon = self.daemon.client();
        let supervisor = self.daemon.clone();
        let task = cx.background_executor().spawn(async move {
            let result = daemon.request(manager, Uuid::nil(), command);
            let snapshot = super::runtime::load_remote_task_state(&daemon);
            let history = if matches!(
                &result,
                Ok(waku_client::ResponsePayload::LifecycleContinued { .. })
            ) {
                waku_client::persistence::hydrate_session(&supervisor, manager)
                    .ok()
                    .flatten()
            } else {
                None
            };
            (result, snapshot, history)
        });
        cx.spawn(async move |this, cx| {
            let (result, snapshot, history) = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .continuation_dialog
                    .as_mut()
                    .filter(|d| d.source == source && d.generation == generation)
                else {
                    return;
                };
                dialog.pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::LifecycleContinued {
                        session,
                        continuation,
                    }) => {
                        dialog.result = this
                            .state
                            .sessions
                            .iter_mut()
                            .find(|session| session.id == source)
                            .and_then(|local| {
                                merge_continuation_response(local, &session, operation)
                            })
                            .or(Some(continuation));
                    }
                    Ok(_) => dialog.error = Some(tr!("decisions.invalid_response")),
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                if let Ok(snapshot) = snapshot {
                    if query && dialog.result.is_none() {
                        dialog.retryable = snapshot
                            .sessions
                            .iter()
                            .find(|s| s.id == source)
                            .is_some_and(|s| !s.continuations.iter().any(|r| r.id == operation));
                    }
                    this.apply_remote_task_state(snapshot, cx);
                }
                let authority = this
                    .continuation_dialog
                    .as_ref()
                    .and_then(|d| d.result.as_ref())
                    .map(|r| r.authority_message_id);
                if let (Some(hydrated), Some(authority)) = (history, authority) {
                    if let Some(parent) = this.state.session_mut(manager) {
                        if super::decisions::merge_saved_message(
                            parent,
                            &hydrated,
                            authority,
                            MessageRole::User,
                            None,
                        ) && this.state.selected_session == Some(manager)
                        {
                            this.reset_visible_state();
                            this.reset_transcript_rows(this.transcript_row_count());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn sync_continuation_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.continuation_dialog.as_mut() else {
            return;
        };
        let Some((operation, _)) = dialog.attempt.as_ref() else {
            return;
        };
        if let Some(record) = self
            .state
            .sessions
            .iter()
            .find(|s| s.id == dialog.source)
            .and_then(|s| s.continuations.iter().find(|r| r.id == *operation))
        {
            dialog.result = Some(record.clone());
            dialog.retryable = false;
            cx.notify();
        }
    }

    fn close_continuation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.continuation_dialog.as_mut() {
            dialog.visible = false;
            window.focus(&dialog.return_focus, cx);
        }
        cx.notify();
    }

    pub(super) fn render_continuation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.continuation_dialog.as_ref().filter(|d| d.visible)?;
        let theme = Theme::current(cx);
        let failed = dialog
            .result
            .as_ref()
            .is_some_and(|r| r.state == InputDeliveryState::Failed);
        let submit = !dialog.pending && (dialog.attempt.is_none() || dialog.retryable || failed);
        let query = !dialog.pending && dialog.attempt.is_some();
        let mut focus = vec![dialog.input.read(cx).focus()];
        if submit {
            focus.push(dialog.send_focus.clone());
        }
        if query {
            focus.push(dialog.query_focus.clone());
        }
        focus.push(dialog.source_focus.clone());
        let target = dialog.result.as_ref().and_then(|r| r.result_session_id);
        if target.is_some() {
            focus.push(dialog.result_focus.clone());
        }
        focus.push(dialog.close_focus.clone());
        let source = dialog.source;
        let status = if dialog.pending {
            tr!("continuation.checking")
        } else if let Some(record) = &dialog.result {
            continuation_state_label(&record.state)
        } else if dialog.attempt.is_some() {
            tr!("continuation.unconfirmed")
        } else {
            tr!("continuation.explanation")
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.canvas.opacity(0.65))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    super::decisions::decision_card(focus)
                        .key_context("Continuation")
                        .w(px(640.0))
                        .p(px(20.0))
                        .rounded(px(14.0))
                        .bg(theme.composer)
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .on_action(cx.listener(|this, _: &CloseContinuation, window, cx| {
                            this.close_continuation(window, cx)
                        }))
                        .on_action(cx.listener(|this, _: &SubmitContinuation, window, cx| {
                            this.request_continuation(false, window, cx)
                        }))
                        .child(tr!("continuation.title", title = dialog.title.clone()))
                        .child(div().h(px(120.0)).child(dialog.input.clone()))
                        .child(div().text_color(theme.text_secondary).child(status))
                        .when_some(
                            dialog
                                .result
                                .as_ref()
                                .and_then(|r| r.reason.clone())
                                .or_else(|| dialog.error.clone()),
                            |card, reason| card.child(div().text_color(theme.danger).child(reason)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap(px(8.0))
                                .when(submit, |row| {
                                    row.child(
                                        super::decisions::decision_answer_control(
                                            "continuation-submit",
                                            if failed {
                                                tr!("continuation.retry")
                                            } else {
                                                tr!("continuation.submit")
                                            },
                                            &dialog.send_focus,
                                            true,
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.request_continuation(false, window, cx)
                                            }),
                                        ),
                                    )
                                })
                                .when(query, |row| {
                                    row.child(
                                        super::decisions::decision_answer_control(
                                            "continuation-query",
                                            tr!("continuation.query"),
                                            &dialog.query_focus,
                                            true,
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.request_continuation(true, window, cx)
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    super::decisions::decision_answer_control(
                                        "continuation-source",
                                        tr!("task_workspace.open_source_history"),
                                        &dialog.source_focus,
                                        true,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.close_continuation(window, cx);
                                            this.select_session(source, cx);
                                        },
                                    )),
                                )
                                .when_some(target, |row, target| {
                                    row.child(
                                        super::decisions::decision_answer_control(
                                            "continuation-result",
                                            tr!("continuation.result"),
                                            &dialog.result_focus,
                                            true,
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.close_continuation(window, cx);
                                                this.select_session(target, cx);
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    super::decisions::decision_answer_control(
                                        "continuation-close",
                                        tr!("consultation.close"),
                                        &dialog.close_focus,
                                        true,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.close_continuation(window, cx),
                                    )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

fn merge_continuation_response(
    local: &mut AgentSession,
    incoming: &AgentSession,
    operation: Uuid,
) -> Option<ChildContinuation> {
    local.apply_lifecycle_metadata(incoming);
    local
        .continuations
        .iter()
        .find(|record| record.id == operation)
        .cloned()
}

fn continuation_state_label(state: &InputDeliveryState) -> String {
    match state {
        InputDeliveryState::Accepted => tr!("session.input_accepted"),
        InputDeliveryState::Queued => tr!("continuation.queued"),
        InputDeliveryState::Received => tr!("continuation.received"),
        InputDeliveryState::Uncertain => tr!("session.input_uncertain"),
        InputDeliveryState::Failed => tr!("session.input_failed"),
        InputDeliveryState::Unsupported => tr!("session.input_unsupported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_response_preserves_newer_receipt_without_catalog_refresh() {
        let mut local = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let operation = Uuid::new_v4();
        local.lifecycle_revision = 2;
        local.continuations.push(ChildContinuation {
            id: operation,
            source_completion_id: Uuid::new_v4(),
            manager_session_id: Uuid::new_v4(),
            authority_message_id: Uuid::new_v4(),
            instruction: "Continue verification".into(),
            result_session_id: Some(local.id),
            state: InputDeliveryState::Received,
            reason: None,
            created_at: 1,
        });
        let mut response = local.list_projection();
        response.lifecycle_revision = 1;
        response.continuations[0].state = InputDeliveryState::Accepted;
        let visible = merge_continuation_response(&mut local, &response, operation).unwrap();
        assert_eq!(visible.state, InputDeliveryState::Received);
        assert_eq!(local.lifecycle_revision, 2);
    }

    struct ContinuationEditor {
        input: Entity<TextInput>,
        close: FocusHandle,
        submitted: usize,
        pending: bool,
        closed: bool,
    }

    impl Render for ContinuationEditor {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            super::super::decisions::decision_card(vec![
                self.input.read(cx).focus(),
                self.close.clone(),
            ])
            .key_context("Continuation")
            .size_full()
            .on_action(cx.listener(|this, _: &SubmitContinuation, _, cx| {
                if !this.pending {
                    this.submitted += 1;
                    this.pending = true;
                    this.input.update(cx, |input, _| input.set_read_only(true));
                }
            }))
            .on_action(cx.listener(|this, _: &CloseContinuation, _, _| this.closed = true))
            .child(div().h(px(80.0)).child(self.input.clone()))
            .child(div().id("close").track_focus(&self.close).size(px(30.0)))
        }
    }

    #[gpui::test]
    fn continuation_editor_preserves_pending_instruction_and_traps_keyboard(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::input::init);
        cx.update(init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                let mut input = TextInput::new(window, cx).multi_line();
                input.set_content("Continue verification", cx);
                input
            });
            ContinuationEditor {
                input,
                close: cx.focus_handle().tab_stop(true),
                submitted: 0,
                pending: false,
                closed: false,
            }
        });
        let (input, close) = cx.read_entity(&view, |v, _| (v.input.clone(), v.close.clone()));
        let focus = cx.read_entity(&input, |input, _| input.focus());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("secondary-enter");
        cx.simulate_keystrokes("secondary-enter");
        cx.simulate_keystrokes("backspace");
        assert_eq!(cx.read_entity(&view, |v, _| v.submitted), 1);
        assert_eq!(
            cx.read_entity(&input, |v, _| v.content().to_owned()),
            "Continue verification"
        );
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| close.is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| focus.is_focused(window)));
        cx.simulate_keystrokes("escape");
        assert!(cx.read_entity(&view, |v, _| v.closed));
    }
}
