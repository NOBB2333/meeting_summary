//! The meeting workspace reserves its height for messages, composer and document.
use super::*;

impl Recorder {
    pub(crate) fn open_ai_settings(
        &mut self,
        page: crate::settings::SettingsPage,
        cx: &mut Context<Self>,
    ) {
        self.settings = true;
        self.settings_page = page;
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
    pub(crate) fn minutes_view(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let ready = self.transcript.complete && !self.transcript.text().is_empty();
        let wide = width >= 860.;
        let short = height < 620.;
        let showing_document = self.ai.artifact_open && (wide || !self.ai.conversation_tab);
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .flex_shrink_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("AI 对话")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        button(
                            "minutes-format-settings",
                            format!("模板 · {}", self.ai.format.style.label()),
                            false,
                        )
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_ai_settings(crate::settings::SettingsPage::Templates, cx)
                        })),
                    )
                    .child(
                        button(
                            "toggle-artifact",
                            if showing_document {
                                if wide { "收起文档" } else { "返回对话" }
                            } else {
                                "查看文档"
                            },
                            showing_document,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !wide {
                                this.ai.artifact_open = true;
                                this.ai.conversation_tab = showing_document;
                            } else {
                                this.ai.artifact_open = !showing_document;
                                this.ai.conversation_tab = true;
                            }
                            cx.notify();
                        })),
                    ),
            );
        let composer = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_shrink_0()
            .pt_2()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(Textarea::new(&self.ai.instruction).disabled(busy).w_full())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                button("conversation-revise-mode", "修改纪要", !self.ai.ask_only)
                                    .ghost()
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ai.ask_only = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button("conversation-ask-mode", "仅提问", self.ai.ask_only)
                                    .ghost()
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ai.ask_only = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.summary_task.is_none(), |d| {
                        let send = primary_button("send-minutes-message", "发送")
                            .disabled(busy || !ready || self.ai.editing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let action = if this.ai.ask_only {
                                    MinutesAction::Ask
                                } else if this.ai.history.latest().is_empty() {
                                    MinutesAction::Generate
                                } else {
                                    MinutesAction::Revise
                                };
                                this.request_minutes(action, cx);
                            }));
                        #[cfg(feature = "ui-smoke")]
                        let send = div().child(send).on_children_prepainted({
                            let bounds = self.ai.send_bounds.clone();
                            move |children, _, _| bounds.set(children.first().copied())
                        });
                        d.child(send)
                    })
                    .when(self.summary_task.is_some(), |d| {
                        d.child(
                            button("cancel-summary", "停止生成", false)
                                .danger()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.ai.cancelled.store(true, Ordering::Relaxed);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(!busy && !self.ai.pending_request.is_empty(), |d| {
                d.child(
                    button("retry-minutes-message", "重试未完成的请求", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let action = if !this.ai.stream_artifact {
                                MinutesAction::Ask
                            } else if this.ai.history.latest().is_empty() {
                                MinutesAction::Generate
                            } else {
                                MinutesAction::Revise
                            };
                            let request = ai_chat::display_request(&this.ai.pending_request);
                            this.request_minutes_with_instruction(action, Some(request), cx);
                        })),
                )
            })
            .when(!short, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(label("Enter 发送 · Shift+Enter 换行").text_xs())
                        .child(
                            button("open-ai-config", "全局设置", false)
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_ai_settings(crate::settings::SettingsPage::Ai, cx)
                                })),
                        ),
                )
            });
        let status = if self.ai_clock.has_run() {
            format!(
                "{} · {}",
                self.ai.status,
                task::duration_text(self.ai_clock.elapsed())
            )
        } else {
            self.ai.status.clone()
        };
        let show_status = !status.is_empty()
            && !status.starts_with("AI 配置已载入")
            && !status.starts_with("默认 DeepSeek");
        let chat = div()
            .w_full()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(self.ai.chat.clone()),
            )
            .when(show_status, |d| {
                d.child(
                    div()
                        .id("chat-status")
                        .max_h(px(if short { 18. } else { 36. }))
                        .overflow_y_scroll()
                        .flex_shrink_0()
                        .child(label(status).text_xs()),
                )
            })
            .child(composer);
        let document = self.minutes_artifact_view(cx);
        let mut body = div().w_full().flex_1().min_h(px(0.)).overflow_hidden();
        if wide && self.ai.artifact_open {
            body = body.child(
                h_resizable("minutes-workspace-split")
                    .child(
                        resizable_panel()
                            .size(px(width * 0.5))
                            .size_range(px(320.)..px(width - 350.))
                            .child(chat),
                    )
                    .child(
                        resizable_panel()
                            .size(px(width * 0.5))
                            .size_range(px(340.)..px(width - 330.))
                            .child(document),
                    ),
            );
        } else if showing_document {
            body = body.child(document.w_full());
        } else {
            body = body.child(chat.w_full());
        }
        div()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .overflow_hidden()
            .child(header)
            .child(body)
    }
}
