//! Read-only decision records. Queries never start a provider runtime.
use super::*;
use waku_protocol::model::{DecisionRequest, DecisionState, StewardDecisionOperation};

gpui::actions!(waku_decisions, [CloseDecisions]);
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new(
        "escape",
        CloseDecisions,
        Some("Decisions"),
    )]);
}

pub(super) struct DecisionDialog {
    parent_id: Uuid,
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
        let parent_id = if self
            .state
            .sessions
            .iter()
            .any(|child| child.parent_session_id == Some(session.id))
        {
            session.id
        } else {
            session.parent_session_id.unwrap_or(session.id)
        };
        let title = self
            .state
            .sessions
            .iter()
            .find(|s| s.id == parent_id)
            .map(|s| s.display_title().to_owned())
            .unwrap_or_else(|| parent_id.to_string());
        let history_focus = cx.focus_handle().tab_stop(true);
        window.focus(&history_focus, cx);
        self.decision_dialog = Some(DecisionDialog {
            parent_id,
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
        self.refresh_decisions(cx);
    }

    fn refresh_decisions(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.decision_dialog.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let parent_id = dialog.parent_id;
        let request_id = Uuid::new_v4();
        dialog.request_id = request_id;
        dialog.pending = true;
        dialog.error = None;
        let daemon = self.daemon.client();
        let task = cx.background_executor().spawn(async move {
            daemon.request(
                parent_id,
                Uuid::nil(),
                waku_client::Command::StewardDecision {
                    operation: StewardDecisionOperation::List { session_id: None },
                },
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .decision_dialog
                    .as_mut()
                    .filter(|d| d.parent_id == parent_id && d.request_id == request_id)
                else {
                    return;
                };
                dialog.pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::StewardDecisions { requests }) => {
                        dialog.titles = Arc::new(
                            requests
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
                        dialog.rows.reset(requests.len());
                        dialog.requests = Arc::new(requests);
                    }
                    Ok(_) => dialog.error = Some(tr!("decisions.invalid_response")),
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_decisions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.decision_dialog = None;
        window.focus(&self.composer_focus(cx), cx);
        cx.notify();
    }

    pub(super) fn render_decisions(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.decision_dialog.as_ref()?;
        let theme = Theme::current(cx);
        let requests = dialog.requests.clone();
        let titles = dialog.titles.clone();
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
        .child(history);
        let card = decision_card(vec![
            dialog.history_focus.clone(),
            dialog.refresh_focus.clone(),
            dialog.close_focus.clone(),
        ])
        .on_action(
            cx.listener(|this, _: &CloseDecisions, window, cx| this.close_decisions(window, cx)),
        )
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
                        .track_focus(&dialog.refresh_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .px(px(10.0))
                        .py(px(7.0))
                        .rounded(px(6.0))
                        .text_size(sp(13.0))
                        .text_color(theme.text)
                        .focus_visible(|s| s.border_1().border_color(theme.accent))
                        .child(tr!("consultation.refresh"))
                        .when(!dialog.pending, |b| {
                            b.on_click(cx.listener(|this, _, _, cx| this.refresh_decisions(cx)))
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
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_decisions(window, cx)),
                        ),
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

fn decision_card(focus_order: Vec<FocusHandle>) -> Stateful<Div> {
    super::consultation::consultation_card(focus_order, "decisions-card").key_context("Decisions")
}

#[cfg(test)]
mod tests {
    use super::*;

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
