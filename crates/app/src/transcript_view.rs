//! Cache transcript presentation and lay out only visible, variable-height rows.
use super::*;
use gpui::component::scroll::Scrollbar;
use opencap_asr::Transcript;

#[derive(Clone, PartialEq)]
struct Row {
    id: u64,
    metadata: SharedString,
    text: SharedString,
    finalized: bool,
}

pub(super) struct TranscriptView {
    pub state: ListState,
    rows: Rc<Vec<Row>>,
    following: bool,
    #[cfg(feature = "ui-smoke")]
    pub measured_rows: Rc<std::cell::RefCell<std::collections::BTreeSet<usize>>>,
}
impl TranscriptView {
    pub fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(160.)),
            rows: Rc::new(Vec::new()),
            following: false,
            #[cfg(feature = "ui-smoke")]
            measured_rows: Rc::new(std::cell::RefCell::new(Default::default())),
        }
    }
    pub fn replace(&mut self, transcript: &Transcript, new_document: bool, cx: &mut Context<Self>) {
        let start = if transcript.complete {
            0
        } else {
            transcript.segments.len().saturating_sub(40)
        };
        let rows: Vec<Row> = transcript.segments[start..]
            .iter()
            .map(|s| Row {
                id: s.id,
                metadata: format!(
                    "{}  ·  {}  ·  {}",
                    transcript.speaker_label(s.start_ms, s.end_ms),
                    opencap_asr::timestamp(s.start_ms, '.'),
                    if s.finalized {
                        "已确认"
                    } else {
                        "识别中 · 后续语音可修正"
                    }
                )
                .into(),
                text: s.text.clone().into(),
                finalized: s.finalized,
            })
            .collect();
        let following = !transcript.complete && !transcript.cancelled;
        if new_document || self.rows.first().map(|r| r.id) != rows.first().map(|r| r.id) {
            self.state.reset_with_uniform_height(rows.len(), px(100.));
        } else {
            for (index, (old, new)) in self.rows.iter().zip(&rows).enumerate() {
                if old != new {
                    self.state.splice(index..index + 1, 1);
                }
            }
            let old_count = self.rows.len();
            if old_count != rows.len() {
                self.state.splice(
                    old_count.min(rows.len())..old_count,
                    rows.len().saturating_sub(old_count),
                );
            }
        }
        if following != self.following || new_document {
            self.state.set_follow_mode(if following {
                FollowMode::Tail
            } else {
                FollowMode::Normal
            });
        }
        self.following = following;
        self.rows = Rc::new(rows);
        cx.notify();
    }
}
impl Render for TranscriptView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if self.rows.is_empty() {
            return div()
                .size_full()
                .child(label("开启系统声音或麦克风后开始录制，或者导入已有文件。"));
        }
        let rows = self.rows.clone();
        #[cfg(feature = "ui-smoke")]
        let measured = self.measured_rows.clone();
        div()
            .size_full()
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
            .child(Scrollbar::vertical(&self.state))
    }
}
