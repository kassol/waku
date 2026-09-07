//! Decision records and explicit user answers. Daemon I/O stays off the UI thread.
use super::*;
use waku_protocol::model::{DecisionRequest, DecisionState, StewardDecisionOperation};

gpui::actions!(waku_decisions, [CloseDecisions, SendDecisionAnswer]);
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", CloseDecisions, Some("Decisions")),
        gpui::KeyBinding::new(
            "secondary-enter",
            SendDecisionAnswer,
            Some("Decisions > TextInput"),
        ),
    ]);
}

pub(super) struct DecisionDialog {
    parent_id: Uuid,
    visible: bool,
    input: Entity<TextInput>,
    selected: Option<Uuid>,
    drafts: HashMap<Uuid, String>,
    attempts: HashMap<Uuid, String>,
    previous_focus: FocusHandle,
    next_focus: FocusHandle,
    answer_focus: FocusHandle,
    title: String,
    requests: Arc<Vec<DecisionRequest>>,
    titles: Arc<HashMap<Uuid, String>>,
    request_id: Uuid,
    pending: bool,
    error: Option<String>,
    rows: ListState,
    history_focus: FocusHandle,
    refresh_focus: FocusHandle,
    close_focus: FocusHandle,
}

impl DecisionDialog {
    fn save_draft(&mut self, cx: &App) {
        if let Some(id) = self.selected {
            if !self.attempts.contains_key(&id)
                && self
                    .requests
                    .iter()
                    .any(|request| request.id == id && request.state == DecisionState::WaitingUser)
            {
                self.drafts
                    .insert(id, self.input.read(cx).content().to_owned());
            }
        }
    }

    fn sync_input(&mut self, cx: &mut Context<Waku>) {
        let request = self
            .selected
            .and_then(|id| self.requests.iter().find(|r| r.id == id));
        let editable = !self.pending
            && request.is_some_and(|r| {
                r.state == DecisionState::WaitingUser && !self.attempts.contains_key(&r.id)
            });
        let text = request
            .map(|r| {
                r.user_answer
                    .as_ref()
                    .or_else(|| self.attempts.get(&r.id))
                    .or_else(|| self.drafts.get(&r.id))
                    .cloned()
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.input.update(cx, |input, cx| {
            input.set_content(text, cx);
            input.set_read_only(!editable);
        });
    }
}

impl Waku {
    pub(super) fn render_decision_entry(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.selected_session()?;
        if session.decision_requests.is_empty()
            && !self
                .state
                .sessions
                .iter()
                .any(|child| child.parent_session_id == Some(session.id))
        {
            return None;
        }
        let theme = Theme::current(cx);
        let focus = self.transcript_control_focus("open-decisions", cx);
        Some(
            div()
                .id("open-decisions")
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .px(px(7.0))
                .py(px(4.0))
                .rounded(px(4.0))
                .text_color(theme.text_secondary)
                .focus_visible(|s| s.border_1().border_color(theme.accent))
                .hover(|s| s.bg(theme.overlay).text_color(theme.text))
                .child(tr!("decisions.open"))
                .on_click(cx.listener(|this, _, window, cx| this.open_decisions(window, cx)))
                .into_any_element(),
        )
    }

    fn open_decisions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let project_id = session.project_id;
        let mut parent_id = session.id;
        let mut visited = HashSet::new();
        while visited.insert(parent_id) {
            let Some(parent) = self
                .state
                .sessions
                .iter()
                .find(|s| s.id == parent_id && s.project_id == project_id)
                .and_then(|s| s.parent_session_id)
            else {
                break;
            };
            if !self
                .state
                .sessions
                .iter()
                .any(|s| s.id == parent && s.project_id == project_id)
            {
                break;
            }
            parent_id = parent;
        }
        let title = self
            .state
            .sessions
            .iter()
            .find(|s| s.id == parent_id)
            .map(|s| s.display_title().to_owned())
            .unwrap_or_else(|| parent_id.to_string());
        if let Some(dialog) = self
            .decision_dialog
            .as_mut()
            .filter(|d| d.parent_id == parent_id)
        {
            dialog.visible = true;
            window.focus(&dialog.history_focus, cx);
            self.refresh_decisions(window, cx);
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .multi_line()
                .read_only(true)
                .placeholder(tr!("decisions.answer_placeholder"))
        });
        let history_focus = cx.focus_handle().tab_stop(true);
        window.focus(&history_focus, cx);
        self.decision_dialog = Some(DecisionDialog {
            parent_id,
            visible: true,
            input,
            selected: None,
            drafts: HashMap::new(),
            attempts: HashMap::new(),
            previous_focus: cx.focus_handle().tab_stop(true),
            next_focus: cx.focus_handle().tab_stop(true),
            answer_focus: cx.focus_handle().tab_stop(true),
            title,
            requests: Arc::new(Vec::new()),
            titles: Arc::new(HashMap::new()),
            request_id: Uuid::nil(),
            pending: false,
            error: None,
            rows: ListState::new(0, ListAlignment::Top, px(256.0)),
            history_focus,
            refresh_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
        });
        self.refresh_decisions(window, cx);
    }

    fn refresh_decisions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.decision_dialog.as_ref() {
            window.focus(&dialog.history_focus, cx);
        }
        self.request_decisions(None, cx);
    }

    fn select_decision(&mut self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self
            .decision_dialog
            .as_mut()
            .filter(|d| !d.pending && !d.requests.is_empty())
        else {
            return;
        };
        dialog.save_draft(cx);
        let current = dialog
            .requests
            .iter()
            .position(|r| Some(r.id) == dialog.selected)
            .unwrap_or(0);
        let index = if next {
            (current + 1) % dialog.requests.len()
        } else {
            (current + dialog.requests.len() - 1) % dialog.requests.len()
        };
        dialog.selected = Some(dialog.requests[index].id);
        dialog.rows.scroll_to_reveal_item(index);
        dialog.sync_input(cx);
        window.focus(&dialog.input.read(cx).focus(), cx);
        cx.notify();
    }

    fn send_decision_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.decision_dialog.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let Some(request) = dialog
            .selected
            .and_then(|id| dialog.requests.iter().find(|r| r.id == id))
            .filter(|r| r.state == DecisionState::WaitingUser)
        else {
            return;
        };
        let id = request.id;
        let answer = dialog
            .attempts
            .get(&id)
            .cloned()
            .unwrap_or_else(|| dialog.input.read(cx).content().trim().to_owned());
        if answer.is_empty() {
            return;
        }
        dialog.save_draft(cx);
        dialog.attempts.insert(id, answer.clone());
        window.focus(&dialog.input.read(cx).focus(), cx);
        self.request_decisions(Some((id, answer)), cx);
    }

    fn request_decisions(&mut self, answer: Option<(Uuid, String)>, cx: &mut Context<Self>) {
        let Some(dialog) = self.decision_dialog.as_mut().filter(|d| !d.pending) else {
            return;
        };
        dialog.save_draft(cx);
        let parent_id = dialog.parent_id;
        let request_id = Uuid::new_v4();
        let command = if let Some((id, text)) = &answer {
            let Some(request) = dialog.requests.iter().find(|r| r.id == *id) else {
                return;
            };
            waku_client::Command::AnswerDecision {
                child_session_id: request.child_session_id,
                request_id: *id,
                answer: text.clone(),
            }
        } else {
            waku_client::Command::StewardDecision {
                operation: StewardDecisionOperation::List { session_id: None },
            }
        };
        dialog.request_id = request_id;
        dialog.pending = true;
        dialog.error = None;
        dialog.sync_input(cx);
        let daemon = self.daemon.client();
        let supervisor = self.daemon.clone();
        let refresh_history = answer.is_some();
        let task = cx.background_executor().spawn(async move {
            let result = daemon.request(parent_id, Uuid::nil(), command);
            let history = if refresh_history
                && matches!(
                    &result,
                    Ok(waku_client::ResponsePayload::StewardDecisions { .. })
                ) {
                waku_client::persistence::hydrate_session(&supervisor, parent_id)
            } else {
                Ok(None)
            };
            (result, history)
        });
        cx.spawn(async move |this, cx| {
            let (result, history) = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .decision_dialog
                    .as_mut()
                    .filter(|d| d.parent_id == parent_id && d.request_id == request_id)
                else {
                    return;
                };
                dialog.pending = false;
                let mut history_changed = false;
                match result {
                    Ok(waku_client::ResponsePayload::StewardDecisions { requests }) => {
                        match history {
                            Ok(Some(hydrated)) => {
                                if let Some(root) = this.state.session_mut(parent_id) {
                                    history_changed =
                                        merge_decision_answer_messages(root, &hydrated, &requests);
                                }
                            }
                            Err(error) => dialog.error = Some(error.to_string()),
                            Ok(None) => {}
                        }
                        let mut combined = if answer.is_some() {
                            dialog.requests.as_ref().clone()
                        } else {
                            Vec::new()
                        };
                        for request in requests {
                            if request.user_answer.is_some() {
                                dialog.drafts.remove(&request.id);
                                dialog.attempts.remove(&request.id);
                            } else if answer.is_none()
                                && request.state == DecisionState::WaitingUser
                            {
                                // A fresh successful query confirms that no answer was saved.
                                dialog.attempts.remove(&request.id);
                            }
                            if let Some(existing) = combined.iter_mut().find(|r| r.id == request.id)
                            {
                                *existing = request;
                            } else {
                                combined.push(request);
                            }
                        }
                        dialog.titles = Arc::new(
                            combined
                                .iter()
                                .map(|request| {
                                    let id = request.child_session_id;
                                    let title = this
                                        .state
                                        .sessions
                                        .iter()
                                        .find(|s| s.id == id)
                                        .map(|s| s.display_title().to_owned())
                                        .unwrap_or_else(|| id.to_string());
                                    (id, title)
                                })
                                .collect(),
                        );
                        if !combined.iter().any(|r| Some(r.id) == dialog.selected) {
                            dialog.selected = combined
                                .iter()
                                .find(|r| r.state == DecisionState::WaitingUser)
                                .or_else(|| combined.first())
                                .map(|r| r.id);
                        }
                        dialog.rows.reset(combined.len());
                        dialog.requests = Arc::new(combined);
                        if let Some(index) = dialog
                            .requests
                            .iter()
                            .position(|r| Some(r.id) == dialog.selected)
                        {
                            dialog.rows.scroll_to_reveal_item(index);
                        }
                    }
                    Ok(_) => dialog.error = Some(tr!("decisions.invalid_response")),
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                dialog.sync_input(cx);
                if history_changed && this.state.selected_session == Some(parent_id) {
                    this.reset_visible_state();
                    this.reset_transcript_rows(this.transcript_row_count());
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_decisions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.decision_dialog.as_mut() {
            dialog.save_draft(cx);
            dialog.visible = false;
        }
        window.focus(&self.composer_focus(cx), cx);
        cx.notify();
    }

    pub(super) fn render_decisions(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.decision_dialog.as_ref().filter(|d| d.visible)?;
        let theme = Theme::current(cx);
        let requests = dialog.requests.clone();
        let titles = dialog.titles.clone();
        let selected = dialog.selected;
        let history = list(dialog.rows.clone(), move |index, _, _| {
            let request = &requests[index];
            let state = match request.state {
                DecisionState::WaitingManager => tr!("decisions.waiting_manager"),
                DecisionState::WaitingUser => tr!("decisions.waiting_user"),
                DecisionState::PendingReceipt => tr!("decisions.pending_receipt"),
                DecisionState::Resolved => tr!("decisions.resolved"),
                DecisionState::Failed => tr!("decisions.failed"),
                DecisionState::Invalidated => tr!("decisions.invalidated"),
            };
            div()
                .when(selected == Some(request.id), |row| row.bg(theme.overlay))
                .px(px(16.0))
                .py(px(12.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .text_size(sp(14.0))
                .line_height(sp(21.0))
                .text_color(theme.text)
                .child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(format!("{} · {state}", titles[&request.child_session_id])),
                )
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(request.question.clone()),
                )
                .child(tr!("decisions.context", value = request.context.clone()))
                .child(tr!(
                    "decisions.recommendation",
                    value = request.recommendation.clone()
                ))
                .child(tr!(
                    "decisions.blocked_work",
                    value = request.blocked_work.clone()
                ))
                .when_some(request.escalation.as_ref(), |row, escalation| {
                    row.child(tr!(
                        "decisions.escalation_reason",
                        value = escalation.reason.clone()
                    ))
                    .children(escalation.options.iter().map(|option| {
                        div().child(tr!(
                            "decisions.option_impact",
                            option = option.label.clone(),
                            impact = option.impact.clone()
                        ))
                    }))
                    .child(tr!(
                        "decisions.escalation_impact",
                        value = escalation.impact.clone()
                    ))
                })
                .when_some(request.user_answer.clone(), |row, value| {
                    row.child(tr!("decisions.saved_answer", value = value))
                })
                .when_some(request.decision.clone(), |row, value| {
                    row.child(tr!("decisions.decision", value = value))
                })
                .when_some(request.reason.clone(), |row, value| {
                    row.child(tr!("decisions.reason", value = value))
                })
                .into_any_element()
        })
        .size_full();
        let history = super::consultation::consultation_history(
            dialog.rows.clone(),
            &dialog.history_focus,
            dialog.requests.len(),
            theme,
            cx,
        )
        .id("decisions-history")
        .h(px(220.0))
        .child(history);
        let selected_request = dialog
            .selected
            .and_then(|id| dialog.requests.iter().find(|r| r.id == id));
        let can_answer = !dialog.pending
            && selected_request.is_some_and(|r| r.state == DecisionState::WaitingUser);
        let can_select = !dialog.pending && dialog.requests.len() > 1;
        let mut focus_order = vec![dialog.history_focus.clone()];
        if can_select {
            focus_order.extend([dialog.previous_focus.clone(), dialog.next_focus.clone()]);
        }
        if selected_request.is_some() {
            focus_order.push(dialog.input.read(cx).focus());
        }
        if can_answer {
            focus_order.push(dialog.answer_focus.clone());
        }
        if !dialog.pending {
            focus_order.push(dialog.refresh_focus.clone());
        }
        focus_order.push(dialog.close_focus.clone());
        let card =
            decision_card(focus_order)
                .on_action(cx.listener(|this, _: &SendDecisionAnswer, window, cx| {
                    this.send_decision_answer(window, cx)
                }))
                .on_action(cx.listener(|this, _: &CloseDecisions, window, cx| {
                    this.close_decisions(window, cx)
                }))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .w_full()
                .max_w(px(780.0))
                .rounded(px(18.0))
                .bg(theme.composer)
                .shadow_xl()
                .overflow_hidden()
                .child(
                    div()
                        .p(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(5.0))
                        .text_size(sp(14.0))
                        .text_color(theme.text)
                        .child(tr!("decisions.open"))
                        .child(dialog.title.clone()),
                )
                .child(history)
                .when_some(selected_request, |card, request| {
                    card.child(
                        div()
                            .px(px(16.0))
                            .py(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                decision_answer_control(
                                    "decisions-previous",
                                    tr!("decisions.previous"),
                                    &dialog.previous_focus,
                                    can_select,
                                    theme,
                                )
                                .when(can_select, |b| {
                                    b.on_click(cx.listener(|this, _, window, cx| {
                                        this.select_decision(false, window, cx)
                                    }))
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(13.0))
                                    .text_color(theme.text)
                                    .child(tr!(
                                        "decisions.answering",
                                        question = request.question.clone()
                                    )),
                            )
                            .child(
                                decision_answer_control(
                                    "decisions-next",
                                    tr!("decisions.next"),
                                    &dialog.next_focus,
                                    can_select,
                                    theme,
                                )
                                .when(can_select, |b| {
                                    b.on_click(cx.listener(|this, _, window, cx| {
                                        this.select_decision(true, window, cx)
                                    }))
                                }),
                            ),
                    )
                    .child(
                        div()
                            .h(px(76.0))
                            .px(px(16.0))
                            .py(px(6.0))
                            .text_size(sp(14.0))
                            .text_color(theme.text)
                            .child(dialog.input.clone()),
                    )
                    .child(
                        div()
                            .px(px(16.0))
                            .pb(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(12.0))
                                    .text_color(theme.text_secondary)
                                    .child(if dialog.attempts.contains_key(&request.id) {
                                        tr!("decisions.answer_unconfirmed")
                                    } else if request.user_answer.is_some() {
                                        tr!("decisions.answer_saved")
                                    } else if request.state == DecisionState::WaitingUser {
                                        tr!("decisions.answer_hint")
                                    } else {
                                        tr!("decisions.answer_read_only")
                                    }),
                            )
                            .child(
                                decision_answer_control(
                                    "decisions-answer",
                                    if dialog.attempts.contains_key(&request.id) {
                                        tr!("decisions.retry_answer")
                                    } else {
                                        tr!("decisions.send_answer")
                                    },
                                    &dialog.answer_focus,
                                    can_answer,
                                    theme,
                                )
                                .when(can_answer, |b| {
                                    b.on_click(cx.listener(|this, _, window, cx| {
                                        this.send_decision_answer(window, cx)
                                    }))
                                }),
                            ),
                    )
                })
                .when_some(dialog.error.clone(), |card, error| {
                    card.child(
                        div()
                            .px(px(16.0))
                            .text_size(sp(12.5))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    div()
                        .p(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .text_size(sp(12.5))
                                .text_color(theme.text_secondary)
                                .child(if dialog.pending {
                                    tr!("decisions.loading")
                                } else if dialog.error.is_some() {
                                    tr!("decisions.refresh_failed")
                                } else if dialog.requests.is_empty() {
                                    tr!("decisions.empty")
                                } else {
                                    tr!("decisions.snapshot")
                                }),
                        )
                        .child(
                            div()
                                .id("decisions-refresh")
                                .when(!dialog.pending, |b| {
                                    b.track_focus(&dialog.refresh_focus)
                                        .tab_index(0)
                                        .tab_stop(true)
                                })
                                .px(px(10.0))
                                .py(px(7.0))
                                .rounded(px(6.0))
                                .text_size(sp(13.0))
                                .text_color(theme.text)
                                .focus_visible(|s| s.border_1().border_color(theme.accent))
                                .child(tr!("consultation.refresh"))
                                .when(!dialog.pending, |b| {
                                    b.on_click(cx.listener(|this, _, window, cx| {
                                        this.refresh_decisions(window, cx)
                                    }))
                                }),
                        )
                        .child(
                            div()
                                .id("decisions-close")
                                .track_focus(&dialog.close_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .px(px(10.0))
                                .py(px(7.0))
                                .rounded(px(6.0))
                                .text_size(sp(13.0))
                                .text_color(theme.text)
                                .focus_visible(|s| s.border_1().border_color(theme.accent))
                                .hover(|s| s.bg(theme.overlay))
                                .child(tr!("consultation.close"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_decisions(window, cx)
                                })),
                        ),
                );
        let layer = div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.25))
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_decisions(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }
}

// Hydration can lag streaming. Import only the saved answer identities and leave
// every existing message and runtime field untouched.
fn merge_decision_answer_messages(
    local: &mut AgentSession,
    hydrated: &AgentSession,
    requests: &[DecisionRequest],
) -> bool {
    if local.id != hydrated.id {
        return false;
    }
    let mut changed = false;
    for request in requests {
        let Some(answer) = &request.user_answer else {
            continue;
        };
        let Some(id) = request.authority_message_id else {
            continue;
        };
        if local.messages.iter().any(|message| message.id == id) {
            continue;
        }
        if let Some(index) = hydrated.messages.iter().position(|message| {
            message.id == id && message.role == MessageRole::User && message.content == *answer
        }) {
            let next = hydrated.messages[index + 1..].iter().find_map(|message| {
                local
                    .messages
                    .iter()
                    .position(|existing| existing.id == message.id)
            });
            let previous = hydrated.messages[..index].iter().rev().find_map(|message| {
                local
                    .messages
                    .iter()
                    .position(|existing| existing.id == message.id)
            });
            let position = next
                .or_else(|| previous.map(|index| index + 1))
                .unwrap_or(local.messages.len());
            local
                .messages
                .insert(position, hydrated.messages[index].clone());
            changed = true;
        }
    }
    changed
}

fn decision_answer_control(
    id: &'static str,
    label: String,
    focus: &FocusHandle,
    enabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(10.0))
        .py(px(7.0))
        .rounded(px(6.0))
        .text_size(sp(13.0))
        .text_color(if enabled {
            theme.text
        } else {
            theme.text_ghost
        })
        .when(enabled, |b| {
            b.track_focus(focus)
                .tab_index(0)
                .tab_stop(true)
                .focus_visible(|s| s.border_1().border_color(theme.accent))
                .hover(|s| s.bg(theme.overlay))
        })
        .child(label)
}

fn decision_card(focus_order: Vec<FocusHandle>) -> Stateful<Div> {
    super::consultation::consultation_card(focus_order, "decisions-card").key_context("Decisions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_answer_hydration_preserves_newer_stream_and_message_order() {
        let mut local = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        local.push_message(MessageRole::User, "Original task");
        let mut hydrated = local.clone();
        let answer_id = hydrated.push_message(MessageRole::User, "Approved");
        local.push_message(MessageRole::Assistant, "Newer streaming reply");
        let newer = local.messages.last().unwrap().clone();
        let request: DecisionRequest = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "parent_session_id":local.id,
            "child_session_id":Uuid::new_v4(), "turn_id":Uuid::new_v4(),
            "question":"Proceed?", "context":"Scope", "recommendation":"Proceed",
            "blocked_work":"Output", "state":"pendingReceipt", "notified":false,
            "authority_message_id":answer_id, "user_answer":"Approved"
        }))
        .unwrap();
        assert!(merge_decision_answer_messages(
            &mut local,
            &hydrated,
            &[request.clone()]
        ));
        assert_eq!(local.messages[1].id, answer_id);
        assert_eq!(
            serde_json::to_value(&local.messages[2]).unwrap(),
            serde_json::to_value(&newer).unwrap()
        );
        assert!(!merge_decision_answer_messages(
            &mut local,
            &hydrated,
            &[request]
        ));
        assert_eq!(local.messages.len(), 3);
    }

    struct AnswerEditorView {
        input: Entity<TextInput>,
        answer_focus: FocusHandle,
        close_focus: FocusHandle,
        enabled: bool,
        submitted: Vec<String>,
    }

    impl AnswerEditorView {
        fn submit(&mut self, cx: &mut Context<Self>) {
            if self.enabled {
                self.submitted
                    .push(self.input.read(cx).content().to_owned());
            }
        }
    }

    impl Render for AnswerEditorView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut order = vec![self.input.read(cx).focus()];
            if self.enabled {
                order.push(self.answer_focus.clone());
            }
            order.push(self.close_focus.clone());
            decision_card(order)
                .size_full()
                .on_action(cx.listener(|this, _: &SendDecisionAnswer, _, cx| this.submit(cx)))
                .child(div().h(px(60.0)).child(self.input.clone()))
                .child(
                    decision_answer_control(
                        "answer",
                        "Send answer".into(),
                        &self.answer_focus,
                        self.enabled,
                        Theme::dark(),
                    )
                    .when(self.enabled, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    }),
                )
                .child(
                    div()
                        .id("close")
                        .track_focus(&self.close_focus)
                        .size(px(30.0)),
                )
        }
    }

    #[gpui::test]
    fn decision_answer_editor_routes_keyboard_and_disables_pending_answers(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::input::init);
        cx.update(init);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                let mut input = TextInput::new(window, cx).multi_line();
                input.set_content("Use JSON", cx);
                input
            });
            AnswerEditorView {
                input,
                answer_focus: cx.focus_handle().tab_stop(true),
                close_focus: cx.focus_handle().tab_stop(true),
                enabled: true,
                submitted: Vec::new(),
            }
        });
        let (input, answer, close) = cx.read_entity(&view, |view, _| {
            (
                view.input.clone(),
                view.answer_focus.clone(),
                view.close_focus.clone(),
            )
        });
        let input_focus = cx.read_entity(&input, |input, _| input.focus());
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_keystrokes("secondary-enter");
        assert_eq!(
            cx.read_entity(&view, |view, _| view.submitted.clone()),
            vec!["Use JSON"]
        );
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| answer.is_focused(window)));
        cx.simulate_keystrokes("enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert_eq!(cx.read_entity(&view, |view, _| view.submitted.len()), 2);
        cx.update_entity(&view, |view, cx| {
            view.enabled = false;
            view.input.update(cx, |input, _| input.set_read_only(true));
            cx.notify();
        });
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.simulate_keystrokes("secondary-enter");
        cx.simulate_keystrokes("backspace");
        assert_eq!(
            cx.read_entity(&input, |input, _| input.content().to_owned()),
            "Use JSON"
        );
        assert_eq!(cx.read_entity(&view, |view, _| view.submitted.len()), 2);
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| close.is_focused(window)));
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| input_focus.is_focused(window)));
    }

    struct DecisionView {
        controls: Vec<FocusHandle>,
        refreshed: usize,
        closed: bool,
    }

    impl Render for DecisionView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            decision_card(self.controls.clone())
                .on_action(cx.listener(|this, _: &CloseDecisions, _, _| this.closed = true))
                .children(self.controls.iter().enumerate().map(|(index, focus)| {
                    div()
                        .id(index)
                        .track_focus(focus)
                        .size(px(20.0))
                        .on_click(cx.listener(move |this, _, _, _| {
                            if index == 1 {
                                this.refreshed += 1;
                            }
                        }))
                }))
        }
    }

    #[gpui::test]
    fn decision_dialog_traps_focus_refreshes_once_and_closes_with_escape(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(init);
        let (view, cx) = cx.add_window_view(|_, cx| DecisionView {
            controls: (0..3).map(|_| cx.focus_handle().tab_stop(true)).collect(),
            refreshed: 0,
            closed: false,
        });
        let controls = cx.read_entity(&view, |view, _| view.controls.clone());
        cx.update(|window, cx| window.focus(&controls[0], cx));
        for index in [1, 2, 0, 1] {
            cx.simulate_keystrokes("tab");
            cx.run_until_parked();
            assert!(cx.update(|window, _| controls[index].is_focused(window)));
        }
        for (key, count) in [("enter", 1), ("space", 2)] {
            cx.simulate_keystrokes(key);
            cx.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse(key).unwrap(),
            });
            cx.run_until_parked();
            assert_eq!(cx.read_entity(&view, |view, _| view.refreshed), count);
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.read_entity(&view, |view, _| view.closed));
    }
}
