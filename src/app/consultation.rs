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
    error: Option<String>,
    rows: ListState,
    history_focus: FocusHandle,
    send_focus: FocusHandle,
    close_focus: FocusHandle,
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
            error: None,
            rows: ListState::new(0, ListAlignment::Bottom, px(256.0)),
            history_focus: cx.focus_handle(),
            send_focus: cx.focus_handle(),
            close_focus: cx.focus_handle(),
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
        let submitted = match &command {
            waku_client::Command::Consult { question, .. } => Some(question.clone()),
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
                    .filter(|d| d.source_id == source_id)
                else {
                    return;
                };
                dialog.pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Consultation { consultation }) => {
                        dialog
                            .rows
                            .reset(consultation.as_ref().map_or(0, |c| c.exchanges.len()));
                        dialog.record = consultation.map(Arc::new);
                        dialog.error = None;
                        if submitted
                            .as_ref()
                            .is_some_and(|q| dialog.input.read(cx).content().trim() == q)
                        {
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
        let scroll_rows = rows.clone();
        let count = record.as_ref().map_or(0, |r| r.exchanges.len());
        let history = list(rows, move |index, _, cx| {
            let theme = Theme::current(cx);
            let Some(exchange) = record.as_ref().and_then(|r| r.exchanges.get(index)) else {
                return div().into_any_element();
            };
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
        let history = div()
            .id("consultation-history")
            .track_focus(&dialog.history_focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(300.0))
            .focus_visible(|s| s.border_1().border_color(theme.accent))
            .on_key_down(move |event, _, cx| {
                if count == 0 { return; }
                let offset = scroll_rows.logical_scroll_top();
                let next = match event.keystroke.key.as_str() {
                    "up" => offset.item_ix.saturating_sub(1),
                    "down" => (offset.item_ix + 1).min(count.saturating_sub(1)),
                    "home" => 0,
                    "end" => count.saturating_sub(1),
                    _ => return,
                };
                scroll_rows.scroll_to(ListOffset {
                    item_ix: next,
                    offset_in_item: px(0.0),
                });
                cx.stop_propagation();
            })
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
            .max_w(px(620.0))
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
