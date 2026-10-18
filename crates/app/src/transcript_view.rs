//! Cache transcript presentation and lay out only visible, variable-height rows.
use super::*;
use gpui::component::scroll::Scrollbar;
use opencap_asr::{Segment, Transcript};

#[derive(Clone, PartialEq)]
struct Row {
    id: u64,
    revision: u64,
    start_ms: u64,
    end_ms: u64,
    metadata: SharedString,
    text: SharedString,
    finalized: bool,
    correcting: bool,
}
impl Row {
    fn matches(&self, segment: &Segment) -> bool {
        self.id == segment.id
            && self.revision == segment.revision
            && self.start_ms == segment.start_ms
            && self.end_ms == segment.end_ms
            && self.finalized == segment.finalized
            && self.correcting == segment.correcting
            && self.text.as_ref() == segment.text
    }
    fn new(segment: &Segment, transcript: &Transcript) -> Self {
        let speaker = transcript.speaker_label(segment.start_ms, segment.end_ms);
        Self {
            id: segment.id,
            revision: segment.revision,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            metadata: format!(
                "{}{}  ·  {}",
                if speaker.is_empty() {
                    String::new()
                } else {
                    format!("{speaker}  ·  ")
                },
                opencap_asr::timestamp(segment.start_ms, '.'),
                if segment.finalized {
                    "已确认"
                } else if segment.correcting {
                    "整句校正中 · 下一句继续识别"
                } else {
                    "识别中 · 后续语音可修正"
                }
            )
            .into(),
            text: segment.text.clone().into(),
            finalized: segment.finalized,
            correcting: segment.correcting,
        }
    }
}

pub(super) struct TranscriptView {
    pub state: ListState,
    rows: Rc<Vec<Rc<Row>>>,
    live: bool,
    speaker_turns: Vec<(u64, u64, u32)>,
    #[cfg(feature = "ui-smoke")]
    pub measured_rows: Rc<std::cell::RefCell<std::collections::BTreeSet<usize>>>,
}
impl TranscriptView {
    pub fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(160.)),
            rows: Rc::new(Vec::new()),
            live: false,
            speaker_turns: Vec::new(),
            #[cfg(feature = "ui-smoke")]
            measured_rows: Rc::new(std::cell::RefCell::new(Default::default())),
        }
    }
    pub fn replace(&mut self, transcript: &Transcript, new_document: bool, cx: &mut Context<Self>) {
        self.update_rows(transcript, new_document);
        cx.notify();
    }
    fn update_rows(&mut self, transcript: &Transcript, new_document: bool) {
        let speakers: Vec<_> = transcript
            .speaker_turns
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.speaker))
            .collect();
        let labels_changed = new_document || self.speaker_turns != speakers;
        // Retain every segment while live. Shared strings avoid rebuilding all
        // history and its metadata whenever the current hypothesis changes.
        let rows: Vec<Rc<Row>> = transcript
            .segments
            .iter()
            .enumerate()
            .map(|(index, s)| {
                match self
                    .rows
                    .get(index)
                    .filter(|row| !labels_changed && row.matches(s))
                {
                    Some(row) => row.clone(),
                    None => Rc::new(Row::new(s, transcript)),
                }
            })
            .collect();
        let live = !transcript.complete && !transcript.cancelled;
        if new_document || self.rows.first().map(|r| r.id) != rows.first().map(|r| r.id) {
            self.state.reset_with_uniform_height(rows.len(), px(100.));
        } else {
            let mut changed = self
                .rows
                .iter()
                .zip(&rows)
                .enumerate()
                .filter_map(|(index, (old, new))| (old != new).then_some(index));
            if let Some(first) = changed.next() {
                let last = changed.next_back().unwrap_or(first);
                // Remeasure once, retaining the reading anchor even when speaker labels change.
                self.state.remeasure_items(first..last + 1);
            }
            let old_count = self.rows.len();
            if old_count != rows.len() {
                self.state.splice(
                    old_count.min(rows.len())..old_count,
                    rows.len().saturating_sub(old_count),
                );
            }
        }
        if live != self.live || new_document {
            self.state.set_follow_mode(if live {
                FollowMode::Tail
            } else {
                FollowMode::Normal
            });
        }
        self.live = live;
        self.speaker_turns = speakers;
        self.rows = Rc::new(rows);
    }
}
impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rows.is_empty() {
            return div()
                .size_full()
                .child(label("文字会按时间保留在这里，可随时向上查看历史。"));
        }
        let rows = self.rows.clone();
        let following = self.state.is_following_tail();
        #[cfg(feature = "ui-smoke")]
        let measured = self.measured_rows.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(label(format!(
                        "共 {} 段 · {}",
                        rows.len(),
                        if self.live && following {
                            "跟随最新文字"
                        } else if self.live {
                            "正在查看历史 · 新文字不会打断阅读"
                        } else {
                            "全部历史内容"
                        }
                    )))
                    .child(
                        button("transcript-latest", "回到最新", false)
                            .disabled(self.live && following)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.live {
                                    this.state.set_follow_mode(FollowMode::Tail);
                                } else {
                                    this.state.scroll_to_end();
                                }
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
                    .child(
                        list(self.state.clone(), move |index, _, _| {
                            #[cfg(feature = "ui-smoke")]
                            measured.borrow_mut().insert(index);
                            let row = &rows[index];
                            div()
                                .w_full()
                                .pr_3()
                                .pb_3()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label(row.metadata.clone()))
                                .child(
                                    div()
                                        .text_base()
                                        .text_color(rgb(if row.finalized { TEXT } else { ACCENT }))
                                        .child(row.text.clone()),
                                )
                                .into_any()
                        })
                        .size_full(),
                    )
                    .child(Scrollbar::vertical(&self.state)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::TranscriptView;
    use gpui::{ListOffset, px};
    use opencap_asr::{Segment, Transcript};
    #[test]
    fn background_sentence_correction_replaces_its_row_while_next_sentence_stays_visible() {
        let mut transcript = live_transcript(2);
        transcript.segments[0].finalized = false;
        transcript.segments[0].correcting = true;
        transcript.segments[1].finalized = false;
        let mut view = TranscriptView::new();
        view.update_rows(&transcript, true);
        assert!(view.rows[0].metadata.contains("整句校正中"));
        assert!(view.rows[1].metadata.contains("识别中"));
        let second = view.rows[1].clone();
        let mut corrected = transcript.segments[0].clone();
        corrected.revision += 1;
        corrected.finalized = true;
        corrected.correcting = false;
        corrected.text = "后台校正后的完整句子".into();
        assert!(transcript.apply(corrected));
        view.update_rows(&transcript, false);
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].text.as_ref(), "后台校正后的完整句子");
        assert!(view.rows[0].metadata.contains("已确认"));
        assert!(std::rc::Rc::ptr_eq(&second, &view.rows[1]));
    }
    fn live_transcript(count: u64) -> Transcript {
        Transcript {
            segments: (0..count)
                .map(|id| Segment {
                    id,
                    revision: 1,
                    start_ms: id * 1000,
                    end_ms: id * 1000 + 900,
                    text: format!("第 {id} 段会议内容"),
                    finalized: true,
                    correcting: false,
                })
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn long_live_history_survives_new_text_without_moving_the_reading_anchor() {
        let mut view = TranscriptView::new();
        let mut transcript = live_transcript(5000);
        view.update_rows(&transcript, true);
        assert_eq!(view.state.item_count(), 5000);
        assert_eq!(view.rows[0].text.as_ref(), "第 0 段会议内容");
        assert!(view.state.is_following_tail());
        view.state.scroll_to(ListOffset {
            item_ix: 12,
            offset_in_item: px(16.),
        });
        assert!(!view.state.is_following_tail());
        let first_text = view.rows[0].text.as_ptr();
        let first_metadata = view.rows[0].metadata.as_ptr();
        transcript.segments.last_mut().unwrap().revision += 1;
        transcript.segments.last_mut().unwrap().text = "当前句获得修正".into();
        transcript.segments.push(Segment {
            id: 5000,
            revision: 1,
            start_ms: 5_000_000,
            end_ms: 5_000_500,
            text: "正在说的新内容".into(),
            finalized: false,
            correcting: false,
        });
        view.update_rows(&transcript, false);
        assert_eq!(view.state.item_count(), 5001);
        assert_eq!(view.state.logical_scroll_top().item_ix, 12);
        assert_eq!(view.state.logical_scroll_top().offset_in_item, px(16.));
        assert!(!view.state.is_following_tail());
        assert_eq!(first_text, view.rows[0].text.as_ptr());
        assert_eq!(first_metadata, view.rows[0].metadata.as_ptr());
        transcript.complete = true;
        view.update_rows(&transcript, false);
        assert_eq!(view.state.logical_scroll_top().item_ix, 12);
        assert_eq!(view.rows.last().unwrap().text.as_ref(), "正在说的新内容");
        view.state.scroll_to_end();
        assert_eq!(view.state.logical_scroll_top().item_ix, 5001);
    }
    #[test]
    fn switching_meetings_resets_the_view_and_speaker_analysis_refreshes_labels() {
        let mut view = TranscriptView::new();
        let mut transcript = live_transcript(100);
        transcript.complete = true;
        view.update_rows(&transcript, true);
        view.state.scroll_to(ListOffset {
            item_ix: 50,
            offset_in_item: px(0.),
        });
        transcript.speaker_turns.push(opencap_asr::SpeakerTurn {
            start_ms: 0,
            end_ms: 900,
            speaker: 1,
        });
        view.update_rows(&transcript, false);
        assert!(view.rows[0].metadata.contains("说话人 1"));
        assert_eq!(view.state.logical_scroll_top().item_ix, 50);
        view.update_rows(&live_transcript(2), true);
        assert_eq!(view.state.item_count(), 2);
        assert!(view.state.is_following_tail());
    }
}
