//! All conversation turns remain accessible; only visible turns are laid out.
use super::*;
use gpui::component::{scroll::Scrollbar, text::TextView};

#[derive(Clone, PartialEq)]
struct Message {
    user: bool,
    text: String,
    pending: bool,
    artifact: bool,
    version: Option<u64>,
}
pub(super) enum ChatEvent {
    OpenArtifact(u64),
}
pub(super) struct ReplyState {
    pub busy: bool,
    pub reset: bool,
    pub artifact: bool,
}
pub(super) struct ChatView {
    state: ListState,
    rows: Rc<Vec<Message>>,
    running: bool,
    request: String,
}
impl ChatView {
    #[cfg(feature = "ui-smoke")]
    pub(super) fn smoke_state(&self) -> ListState {
        self.state.clone()
    }
    #[cfg(feature = "ui-smoke")]
    pub(super) fn smoke_rows(&self) -> usize {
        self.rows.len()
    }
    #[cfg(feature = "ui-smoke")]
    pub(super) fn smoke_last_text(&self) -> &str {
        self.rows.last().map_or("", |row| row.text.as_str())
    }

    pub fn new() -> Self {
        let state = ListState::new(0, ListAlignment::Top, px(120.));
        state.set_follow_mode(FollowMode::Tail);
        Self {
            state,
            rows: Rc::new(Vec::new()),
            running: false,
            request: String::new(),
        }
    }
    pub fn replace(
        &mut self,
        history: &opencap_asr::MinutesHistory,
        request: &str,
        stream: &str,
        state: ReplyState,
        cx: &mut Context<Self>,
    ) {
        self.update_rows(
            history,
            request,
            stream,
            state.busy,
            state.reset,
            state.artifact,
        );
        cx.notify();
    }
    fn update_rows(
        &mut self,
        history: &opencap_asr::MinutesHistory,
        request: &str,
        stream: &str,
        busy: bool,
        reset: bool,
        artifact_reply: bool,
    ) {
        let new_request = !request.is_empty() && (request != self.request || busy && !self.running);
        self.request = request.into();
        self.running = busy;
        let mut rows = Vec::new();
        for turn in &history.conversation {
            rows.push(Message {
                user: true,
                text: display_request(&turn.request),
                pending: false,
                artifact: false,
                version: None,
            });
            let version = history
                .versions
                .iter()
                .rev()
                .find(|version| {
                    version.markdown == turn.reply && version.instruction == turn.request
                })
                .or_else(|| {
                    history
                        .versions
                        .iter()
                        .find(|version| version.markdown == turn.reply)
                })
                .map(|v| v.number);
            rows.push(Message {
                user: false,
                text: turn.reply.clone(),
                pending: false,
                artifact: version.is_some(),
                version,
            });
        }
        if !request.is_empty() {
            rows.push(Message {
                user: true,
                text: display_request(request),
                pending: false,
                artifact: false,
                version: None,
            });
            rows.push(Message {
                user: false,
                text: if stream.is_empty() {
                    if busy {
                        "正在等待正文…"
                    } else {
                        "本次未完成，可调整要求后重试。"
                    }
                    .into()
                } else {
                    stream.into()
                },
                pending: true,
                artifact: artifact_reply,
                version: None,
            });
        }
        if reset {
            self.state.reset_with_uniform_height(rows.len(), px(100.));
            self.state.set_follow_mode(FollowMode::Tail);
        } else {
            for (i, (old, new)) in self.rows.iter().zip(&rows).enumerate() {
                if old != new {
                    self.state.remeasure_items(i..i + 1);
                }
            }
            if rows.len() != self.rows.len() {
                self.state.splice(
                    self.rows.len().min(rows.len())..self.rows.len(),
                    rows.len().saturating_sub(self.rows.len()),
                );
            }
        }
        self.rows = Rc::new(rows);
        if new_request {
            self.state.set_follow_mode(FollowMode::Tail);
            self.state.scroll_to_end();
        }
    }
}
impl EventEmitter<ChatEvent> for ChatView {}
pub(super) fn display_request(request: &str) -> String {
    if ["标准会议纪要正文：", "阅读摘要：", "行动清单："]
        .iter()
        .any(|prefix| request.starts_with(prefix))
        && let Some((_, text)) = request.split_once("\n用户补充要求：")
    {
        return if text.trim().is_empty() {
            "生成会议纪要".into()
        } else {
            text.trim().into()
        };
    }
    request.into()
}
impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let running = self.running;
        let entity = cx.entity().downgrade();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_shrink_0()
                    .child(label(if self.state.is_following_tail() {
                        "跟随最新回复"
                    } else {
                        "正在查看历史"
                    }))
                    .child(
                        button("chat-latest", "回到最新", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.set_follow_mode(FollowMode::Tail);
                                this.state.scroll_to_end();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .overflow_hidden()
                    .when(rows.is_empty(), |d| {
                        d.child(label(
                            "告诉 AI 你想怎么写。生成后可继续提建议，每轮对话都会保留。",
                        ))
                    })
                    .when(!rows.is_empty(), |d| {
                        d.child(
                            list(self.state.clone(), move |index, _, _| {
                                let row = &rows[index];
                                let mut message = div()
                                    .w_full()
                                    .pb_4()
                                    .pr_3()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(label(if row.user {
                                        "你"
                                    } else if row.pending {
                                        if running {
                                            "AI · 正在生成，完成后保存"
                                        } else {
                                            "AI · 未完成，此预览尚未保存"
                                        }
                                    } else {
                                        "AI"
                                    }));
                                if row.user {
                                    message = message.child(
                                        div().p_3().rounded_lg().bg(rgb(SOFT)).text_sm().child(
                                            TextView::markdown(
                                                ("chat-user", index),
                                                opencap_asr::escape_markdown(&row.text),
                                            )
                                            .selectable(true)
                                            .scrollable(false),
                                        ),
                                    );
                                } else {
                                    message = message.child(
                                        TextView::markdown(
                                            ("chat-message", index),
                                            row.text.clone(),
                                        )
                                        .selectable(true)
                                        .scrollable(false),
                                    );
                                }
                                if row.artifact
                                    && let Some(version) = row.version
                                {
                                    let entity = entity.clone();
                                    message = message.child(
                                        button(
                                            ("chat-artifact", index),
                                            format!("查看文档 · v{version}"),
                                            false,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                let _ = entity.update(cx, |_, cx| {
                                                    cx.emit(ChatEvent::OpenArtifact(version))
                                                });
                                            },
                                        ),
                                    );
                                }
                                message.into_any_element()
                            })
                            .size_full(),
                        )
                    })
                    .child(Scrollbar::vertical(&self.state)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::ChatView;
    use gpui::{FollowMode, ListOffset, px};
    #[test]
    fn full_conversation_and_streaming_keep_the_reading_anchor() {
        let mut history = opencap_asr::MinutesHistory::default();
        for i in 0..250 {
            history.conversation.push(opencap_asr::MinutesTurn {
                request: format!("修改要求 {i}"),
                reply: format!("修订正文 {i}"),
            });
        }
        let mut view = ChatView::new();
        view.update_rows(&history, "下一轮", "当前正文", true, true, false);
        assert_eq!(view.rows.len(), 502);
        assert!(view.rows[0].text.contains("修改要求 0"));
        view.state.set_follow_mode(FollowMode::Normal);
        view.state.scroll_to(ListOffset {
            item_ix: 12,
            offset_in_item: px(18.),
        });
        view.update_rows(&history, "下一轮", "当前正文继续增加", true, false, false);
        let anchor = view.state.logical_scroll_top();
        assert_eq!(anchor.item_ix, 12);
        assert_eq!(anchor.offset_in_item, px(18.));
        history.conversation.push(opencap_asr::MinutesTurn {
            request: "下一轮".into(),
            reply: "完整正文".into(),
        });
        view.update_rows(&history, "", "", false, false, false);
        assert_eq!(view.rows.len(), 502);
        assert_eq!(view.rows.last().unwrap().text, "完整正文");
        assert_eq!(view.state.logical_scroll_top().item_ix, 12);
    }
    #[test]
    fn generated_documents_and_questions_show_full_replies_in_chat() {
        let history = opencap_asr::MinutesHistory {
            versions: vec![opencap_asr::MinutesVersion {
                number: 1,
                created_unix: 0,
                instruction: "初稿".into(),
                markdown: "# 正文\n预算不能增加。".into(),
                elapsed_seconds: None,
            }],
            conversation: vec![opencap_asr::MinutesTurn {
                request: "标准会议纪要正文：模板要求\n用户补充要求：请简短一些".into(),
                reply: "# 正文\n预算不能增加。".into(),
            }],
        };
        let mut view = ChatView::new();
        view.update_rows(&history, "继续修改", "# 新正文", true, true, true);
        assert_eq!(view.rows[0].text, "请简短一些");
        assert_eq!(view.rows[1].version, Some(1));
        assert!(view.rows[1].artifact);
        assert!(view.rows[3].artifact);
        assert_eq!(view.rows[1].text, "# 正文\n预算不能增加。");
        assert_eq!(view.rows[3].text, "# 新正文");
        view.update_rows(
            &history,
            "预算是多少？",
            "原文没有明确金额。",
            true,
            false,
            false,
        );
        assert!(!view.rows[3].artifact);
        assert_eq!(view.rows[3].text, "原文没有明确金额。");
    }
}
