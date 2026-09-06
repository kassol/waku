//! Independent discussion UI. Daemon I/O runs off the rendering thread.
use super::*;
use gpui::{KeyBinding, actions};
use waku_client::consultation::Consultation;

actions!(waku_consultation, [SendConsultation, CloseConsultation]);
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-enter", SendConsultation, Some("Consultation")),
        KeyBinding::new("escape", CloseConsultation, Some("Consultation")),
    ]);
}

pub(super) struct ConsultationDialog {
    source_id: Uuid,
    title: String,
    visible: bool,
    input: Entity<TextInput>,
    record: Option<Arc<Consultation>>,
    pending: bool,
    request_id: Uuid,
    execution_attempt: Option<(String, Uuid)>,
    error: Option<String>,
    rows: ListState,
    history_focus: FocusHandle,
    send_focus: FocusHandle,
    close_focus: FocusHandle,
    execute_focus: FocusHandle,
    refresh_focus: FocusHandle,
    retry_focus: FocusHandle,
}

impl Waku {
    pub(super) fn open_consultation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.selected_session() else {
            return;
        };
        let source_id = source.id;
        let title = source.display_title().to_owned();
        if let Some(dialog) = self
            .consultation
            .as_mut()
            .filter(|d| d.source_id == source_id)
        {
            dialog.visible = true;
            window.focus(&dialog.input.read(cx).focus(), cx);
            if !dialog.pending {
                dialog.pending = true;
                self.request_consultation(
                    waku_client::Command::LoadConsultation {
                        source_session_id: source_id,
                    },
                    source_id,
                    cx,
                );
            }
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .multi_line()
                .placeholder(tr!("consultation.placeholder"))
        });
        window.focus(&input.read(cx).focus(), cx);
        self.consultation = Some(ConsultationDialog {
            source_id,
            title,
            visible: true,
            input,
            record: None,
            pending: true,
            request_id: Uuid::nil(),
            execution_attempt: None,
            error: None,
            rows: ListState::new(0, ListAlignment::Bottom, px(256.0)),
            history_focus: cx.focus_handle(),
            send_focus: cx.focus_handle(),
            close_focus: cx.focus_handle(),
            execute_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            retry_focus: cx.focus_handle(),
        });
        self.request_consultation(
            waku_client::Command::LoadConsultation {
                source_session_id: source_id,
            },
            source_id,
            cx,
        );
    }

    fn request_consultation(
        &mut self,
        command: waku_client::Command,
        source_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let request_id = Uuid::new_v4();
        let Some(dialog) = self
            .consultation
            .as_mut()
            .filter(|d| d.source_id == source_id)
        else {
            return;
        };
        dialog.request_id = request_id;
        let submitted = match &command {
            waku_client::Command::Consult { question, .. } => Some(question.clone()),
            waku_client::Command::ExecuteConsultation { instruction, .. } => {
                Some(instruction.clone())
            }
            _ => None,
        };
        let daemon = self.daemon.client();
        let task = cx
            .background_executor()
            .spawn(async move { daemon.request(Uuid::nil(), Uuid::nil(), command) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .consultation
                    .as_mut()
                    .filter(|d| d.source_id == source_id && d.request_id == request_id)
                else {
                    return;
                };
                dialog.pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Consultation { consultation }) => {
                        dialog.rows.reset(
                            consultation
                                .as_ref()
                                .map_or(0, |c| c.exchanges.len() + c.instructions.len()),
                        );
                        dialog.record = consultation.map(Arc::new);
                        dialog.error = None;
                        if submitted
                            .as_ref()
                            .is_some_and(|q| dialog.input.read(cx).content().trim() == q)
                        {
                            dialog.execution_attempt = None;
                            dialog
                                .input
                                .update(cx, |input, cx| input.set_content(String::new(), cx));
                        }
                    }
                    Ok(_) => dialog.error = Some(tr!("consultation.invalid_response")),
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn send_consultation(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.consultation.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let question = dialog.input.read(cx).content().trim().to_owned();
        if question.is_empty() {
            return;
        }
        let source_id = dialog.source_id;
        dialog.execution_attempt = None;
        dialog.pending = true;
        dialog.error = None;
        self.request_consultation(
            waku_client::Command::Consult {
                source_session_id: source_id,
                question,
            },
            source_id,
            cx,
        );
    }

    fn execute_consultation(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(dialog) = self.consultation.as_mut().filter(|d| !d.pending) else {
            return;
        };
        let (instruction, delivery_id) = if retry {
            let Some(last) = dialog.record.as_ref().and_then(|r| r.instructions.last()) else {
                return;
            };
            (last.instruction.clone(), last.delivery_id)
        } else {
            let text = dialog.input.read(cx).content().trim().to_owned();
            if text.is_empty() {
                return;
            }
            match &dialog.execution_attempt {
                Some((previous, id)) if previous == &text => (text, *id),
                _ => (text, Uuid::new_v4()),
            }
        };
        dialog.execution_attempt = Some((instruction.clone(), delivery_id));
        dialog.pending = true;
        dialog.error = None;
        let source_id = dialog.source_id;
        self.request_consultation(
            waku_client::Command::ExecuteConsultation {
                source_session_id: source_id,
                delivery_id,
                instruction,
            },
            source_id,
            cx,
        );
    }

    fn refresh_consultation(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.consultation.as_mut().filter(|d| !d.pending) else {
            return;
        };
        dialog.pending = true;
        let source_id = dialog.source_id;
        self.request_consultation(
            waku_client::Command::LoadConsultation {
                source_session_id: source_id,
            },
            source_id,
            cx,
        );
    }

    fn close_consultation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.consultation.as_mut() {
            dialog.visible = false;
        }
        window.focus(&self.composer_focus(cx), cx);
        cx.notify();
    }

    pub(super) fn render_consultation(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.consultation.as_ref().filter(|d| d.visible)?;
        let theme = Theme::current(cx);
        let record = dialog.record.clone();
        let rows = dialog.rows.clone();
        let count = record
            .as_ref()
            .map_or(0, |r| r.exchanges.len() + r.instructions.len());
        let history = list(rows.clone(), move |index, _, cx| {
            let theme = Theme::current(cx);
            let Some(record) = record.as_ref() else {
                return div().into_any_element();
            };
            if index >= record.exchanges.len() {
                let Some(instruction) = record.instructions.get(index - record.exchanges.len())
                else {
                    return div().into_any_element();
                };
                let (status, reason) = match instruction.delivery.as_ref() {
                    Some(delivery) => {
                        use waku_client::model::{InputConfirmation, InputDeliveryState};
                        let status = match delivery.state {
                            InputDeliveryState::Accepted => tr!("session.input_accepted"),
                            InputDeliveryState::Queued => tr!("session.input_queued"),
                            InputDeliveryState::Failed => tr!("session.input_failed"),
                            InputDeliveryState::Uncertain => tr!("session.input_uncertain"),
                            InputDeliveryState::Unsupported => tr!("session.input_unsupported"),
                            InputDeliveryState::Received => {
                                if delivery.confirmation == Some(InputConfirmation::Provider) {
                                    tr!("session.input_received_provider")
                                } else {
                                    tr!("session.input_received_transport")
                                }
                            }
                        };
                        (
                            status,
                            instruction
                                .error
                                .clone()
                                .or_else(|| delivery.reason.clone()),
                        )
                    }
                    None => (
                        if instruction.error.is_some() {
                            tr!("session.input_failed")
                        } else {
                            tr!("consultation.instruction_saved")
                        },
                        instruction.error.clone(),
                    ),
                };
                return div()
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
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("consultation.execution_record")),
                    )
                    .child(instruction.instruction.clone())
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .text_color(theme.text_secondary)
                            .child(status),
                    )
                    .child(
                        div()
                            .text_size(sp(12.0))
                            .text_color(theme.text_secondary)
                            .child(format!(
                                "{} · {}",
                                DateTime::<Utc>::from_timestamp(instruction.context_at as i64, 0)
                                    .map(|t| t
                                        .with_timezone(&Local)
                                        .format("%Y-%m-%d %H:%M:%S")
                                        .to_string())
                                    .unwrap_or_default(),
                                instruction.delivery_id
                            )),
                    )
                    .when(!instruction.pending_targets.is_empty(), |row| {
                        row.child(
                            div()
                                .text_size(sp(12.0))
                                .text_color(theme.text_secondary)
                                .child(tr!(
                                    "consultation.pending_targets",
                                    count = instruction.pending_targets.len()
                                )),
                        )
                    })
                    .when_some(reason, |row, reason| {
                        row.child(div().text_size(sp(12.0)).child(reason))
                    })
                    .into_any_element();
            }
            let exchange = &record.exchanges[index];
            let time = DateTime::<Utc>::from_timestamp(exchange.context_at as i64, 0)
                .map(|t| {
                    t.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
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
                        .font_weight(FontWeight::MEDIUM)
                        .child(exchange.question.clone()),
                )
                .child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("consultation.context_time", time = time)),
                )
                .child(
                    exchange
                        .answer
                        .clone()
                        .or_else(|| exchange.error.clone())
                        .unwrap_or_else(|| tr!("consultation.interrupted")),
                )
                .into_any_element()
        })
        .size_full();
        let history = consultation_history(rows.clone(), &dialog.history_focus, count, theme, cx)
            .child(history);
        let can_send = !dialog.pending;
        let card = div()
            .id("consultation-card")
            .key_context("Consultation")
            .tab_group()
            .tab_stop(false)
            .on_action(cx.listener(|this, _: &SendConsultation, _, cx| this.send_consultation(cx)))
            .on_action(cx.listener(|this, _: &CloseConsultation, window, cx| {
                this.close_consultation(window, cx)
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
                    .text_color(theme.text)
                    .text_size(sp(14.0))
                    .child(tr!("consultation.title"))
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(dialog.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(tr!("consultation.read_only")),
                    ),
            )
            .child(history)
            .child(
                div()
                    .h(px(90.0))
                    .px(px(16.0))
                    .py(px(8.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(dialog.input.clone()),
            )
            .when_some(dialog.error.clone(), |card, error| {
                card.child(
                    div()
                        .px(px(16.0))
                        .text_color(theme.danger)
                        .text_size(sp(12.5))
                        .child(error),
                )
            })
            .child(
                div()
                    .p(px(12.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(if dialog.pending {
                                tr!("consultation.answering")
                            } else if dialog.error.is_some() {
                                tr!("consultation.not_saved")
                            } else {
                                tr!("consultation.saved")
                            }),
                    )
                    .child(
                        div()
                            .id("consultation-refresh")
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
                            .when(can_send, |b| {
                                b.on_click(
                                    cx.listener(|this, _, _, cx| this.refresh_consultation(cx)),
                                )
                            }),
                    )
                    .when(
                        dialog
                            .record
                            .as_ref()
                            .is_some_and(|r| !r.instructions.is_empty()),
                        |footer| {
                            footer.child(
                                div()
                                    .id("consultation-retry")
                                    .track_focus(&dialog.retry_focus)
                                    .tab_index(0)
                                    .tab_stop(true)
                                    .px(px(10.0))
                                    .py(px(7.0))
                                    .rounded(px(6.0))
                                    .text_size(sp(13.0))
                                    .text_color(theme.text)
                                    .focus_visible(|s| s.border_1().border_color(theme.accent))
                                    .child(tr!("consultation.retry"))
                                    .when(can_send, |b| {
                                        b.on_click(cx.listener(|this, _, _, cx| {
                                            this.execute_consultation(true, cx)
                                        }))
                                    }),
                            )
                        },
                    )
                    .child(
                        div()
                            .id("consultation-execute")
                            .track_focus(&dialog.execute_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(6.0))
                            .text_size(sp(13.0))
                            .text_color(theme.text)
                            .focus_visible(|s| s.border_1().border_color(theme.accent))
                            .child(tr!("consultation.execute"))
                            .when(can_send, |b| {
                                b.on_click(cx.listener(|this, _, _, cx| {
                                    this.execute_consultation(false, cx)
                                }))
                            }),
                    )
                    .child(
                        div()
                            .id("consultation-close")
                            .track_focus(&dialog.close_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(6.0))
                            .text_color(theme.text)
                            .text_size(sp(13.0))
                            .focus_visible(|s| s.border_1().border_color(theme.accent))
                            .hover(|s| s.bg(theme.overlay))
                            .child(tr!("consultation.close"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_consultation(window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id("consultation-send")
                            .track_focus(&dialog.send_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .px(px(12.0))
                            .py(px(7.0))
                            .rounded(px(6.0))
                            .text_size(sp(13.0))
                            .text_color(if can_send {
                                theme.text
                            } else {
                                theme.text_ghost
                            })
                            .focus_visible(|s| s.border_1().border_color(theme.accent))
                            .child(tr!("consultation.send"))
                            .when(can_send, |button| {
                                button.hover(|s| s.bg(theme.overlay)).on_click(
                                    cx.listener(|this, _, _, cx| this.send_consultation(cx)),
                                )
                            }),
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
                cx.listener(|this, _, window, cx| this.close_consultation(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }
}

fn consultation_history<T: 'static>(
    scroll_rows: ListState,
    focus: &FocusHandle,
    count: usize,
    theme: Theme,
    cx: &mut Context<T>,
) -> Stateful<Div> {
    div()
        .id("consultation-history")
        .track_focus(focus)
        .tab_index(0)
        .tab_stop(true)
        .h(px(300.0))
        .focus_visible(|s| s.border_1().border_color(theme.accent))
        .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
            if count == 0 {
                return;
            }
            match event.keystroke.key.as_str() {
                "end" => scroll_rows.scroll_to_end(),
                "home" => scroll_rows.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.0) }),
                key => {
                    let delta = match key {
                        "up" => -40.0, "down" => 40.0,
                        "pageup" => -300.0, "pagedown" => 300.0,
                        _ => return,
                    };
                    // Bottom-aligned lists keep an end anchor; move from the visible pixel offset.
                    let max = scroll_rows.max_offset_for_scrollbar().y;
                    let current = (-scroll_rows.scroll_px_offset_for_scrollbar().y).clamp(px(0.0), max);
                    let next = (current + px(delta)).clamp(px(0.0), max);
                    scroll_rows.set_offset_from_scrollbar(point(px(0.0), -next));
                }
            }
            cx.notify();
            cx.stop_propagation();
        }))
}

#[cfg(test)]
mod history_keyboard_tests {
    use super::*;

    struct HistoryView {
        rows: ListState,
        focus: FocusHandle,
        renders: usize,
    }

    impl Render for HistoryView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            consultation_history(self.rows.clone(), &self.focus, 1, Theme::dark(), cx)
                .w(px(400.0))
                .child(
                    list(self.rows.clone(), |_, _, _| {
                        div()
                            .h(px(900.0))
                            .w_full()
                            .child("A long, completed consultation answer.\n".repeat(40))
                            .into_any_element()
                    })
                    .size_full(),
                )
        }
    }

    #[gpui::test]
    fn static_long_answer_scrolls_and_repaints_by_keyboard(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| HistoryView {
            rows: ListState::new(1, ListAlignment::Bottom, px(256.0)),
            focus: cx.focus_handle(),
            renders: 0,
        });
        let (rows, focus) =
            cx.read_entity(&view, |view, _| (view.rows.clone(), view.focus.clone()));
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(rows.logical_scroll_top().offset_in_item, px(0.0));
        let end = f32::from(rows.max_offset_for_scrollbar().y);
        assert!(end > 500.0, "the answer must exceed the viewport");
        for (key, expected) in [
            ("down", 40.0),
            ("end", end),
            ("up", end - 40.0),
            ("pageup", end - 340.0),
            ("pagedown", end - 40.0),
            ("home", 0.0),
        ] {
            let renders = cx.read_entity(&view, |view, _| view.renders);
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            assert_eq!(
                (-rows.scroll_px_offset_for_scrollbar().y).min(rows.max_offset_for_scrollbar().y),
                px(expected),
                "{key}"
            );
            assert!(
                cx.read_entity(&view, |view, _| view.renders) > renders,
                "{key} must repaint the owner without provider events"
            );
            if key == "end" {
                assert_eq!(rows.is_scrolled_to_end(), Some(true));
            } else {
                let bounds = rows.bounds_for_item(0).expect("answer remains rendered");
                assert_eq!(bounds.size.height, px(900.0));
            }
        }
    }
}
