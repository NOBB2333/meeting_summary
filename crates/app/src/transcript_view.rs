//! Cache transcript presentation and lay out only visible, variable-height rows.
use super::*;
use gpui::component::{
    input::Input,
    scroll::Scrollbar,
    text::{SelectionFormat, TextView},
};
use opencap_asr::{ReviewState, Segment, SentenceReview, Transcript};

pub(super) struct ReviewSelection {
    pub id: u64,
    pub original: bool,
}
#[cfg(feature = "internal-player")]
#[derive(Clone)]
pub(super) struct PlaybackSeek {
    pub milliseconds: u64,
    pub end_ms: u64,
    pub segment_id: u64,
    pub revision: u64,
    pub text: SharedString,
    pub run_id: Option<String>,
}
#[cfg(feature = "internal-player")]
pub(super) struct PlaybackSelection {
    pub segment: PlaybackSeek,
}
#[cfg(feature = "internal-player")]
pub(super) struct PlaybackRangeSelection {
    pub run_id: Option<String>,
    pub rows: Vec<PlaybackRangeRow>,
}
#[cfg(feature = "internal-player")]
pub(super) struct PlaybackRangeRow {
    pub id: u64,
    pub revision: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: SharedString,
}
#[cfg(feature = "internal-player")]
pub(super) struct PlaybackTokenSeek {
    pub segment_id: u64,
    pub text_revision: u64,
    pub source_run_id: String,
    pub text_sha256: String,
    pub text_range: std::ops::Range<usize>,
    pub start_ms: u64,
}
#[cfg(feature = "internal-player")]
pub(super) struct PlaybackWordRange {
    pub segment_id: u64,
    pub source_run_id: String,
    pub alignment_run_id: String,
    pub text_revision: u64,
    pub text_sha256: String,
    pub word: opencap_asr::AlignedWord,
}

#[derive(Clone, PartialEq)]
struct Row {
    id: u64,
    revision: u64,
    start_ms: u64,
    end_ms: u64,
    metadata: SharedString,
    speaker: SharedString,
    text: SharedString,
    finalized: bool,
    confirmed: bool,
    correcting: bool,
    terminal_unconfirmed: bool,
    task_live: bool,
    review: Option<SentenceReview>,
    #[cfg(feature = "internal-player")]
    token_starts: Option<opencap_asr::ExactTokenStarts>,
    #[cfg(feature = "internal-player")]
    word_alignment: Option<opencap_asr::WordAlignment>,
    #[cfg(feature = "internal-player")]
    timing_evidence: (Option<serde_json::Value>, Option<serde_json::Value>),
}
impl Row {
    fn matches(&self, segment: &Segment, task_live: bool) -> bool {
        self.id == segment.id
            && self.revision == segment.revision
            && self.start_ms == segment.start_ms
            && self.end_ms == segment.end_ms
            && self.finalized == segment.finalized
            && self.confirmed == segment.is_confirmed()
            && self.correcting == segment.correcting
            && self.terminal_unconfirmed
                == segment
                    .extra
                    .get("terminal_unconfirmed")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            && self.task_live == task_live
            && self.review == segment.review
            && self.text.as_ref() == segment.text
            && {
                #[cfg(feature = "internal-player")]
                {
                    self.timing_evidence.0.as_ref() == segment.extra.get("raw_token_timing")
                        && self.timing_evidence.1.as_ref() == segment.extra.get("word_alignment_v1")
                }
                #[cfg(not(feature = "internal-player"))]
                {
                    true
                }
            }
    }
    fn new(
        segment: &Segment,
        speaker: String,
        run: Option<&opencap_asr::TranscriptionRun>,
        task_live: bool,
    ) -> Self {
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
                if segment.end_ms > segment.start_ms {
                    opencap_asr::timestamp(segment.start_ms, '.')
                } else {
                    "时间不可用".into()
                },
                match segment.review.as_ref().map(|review| review.state) {
                    _ if segment
                        .extra
                        .get("terminal_unconfirmed")
                        .and_then(|value| value.as_bool())
                        == Some(true) =>
                        "识别未完成 · 暂存文字待核对",
                    _ if !task_live
                        && (!segment.finalized
                            || segment.correcting
                            || segment
                                .review
                                .as_ref()
                                .is_some_and(|review| review.state == ReviewState::Pending)) =>
                        "未定稿 · 检查点文字待核对",
                    Some(ReviewState::Pending) => "整句校正中 · 下一句继续识别",
                    Some(ReviewState::Reviewed) => "已复核",
                    Some(ReviewState::Restored) => "已选流式原文",
                    Some(ReviewState::Deferred) => "待补转 · 已保留流式原文",
                    Some(ReviewState::Failed) => "复核未完成 · 已保留流式原文",
                    None if segment.is_confirmed() => "已确认",
                    None if segment.correcting => "整句校正中 · 下一句继续识别",
                    None => "识别中 · 后续语音可修正",
                },
            )
            .into(),
            speaker: speaker.into(),
            text: segment.text.clone().into(),
            finalized: segment.finalized,
            confirmed: segment.is_confirmed(),
            correcting: segment.correcting,
            terminal_unconfirmed: segment
                .extra
                .get("terminal_unconfirmed")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            task_live,
            review: segment.review.clone(),
            #[cfg(feature = "internal-player")]
            token_starts: opencap_asr::exact_token_starts(segment, run),
            #[cfg(feature = "internal-player")]
            word_alignment: opencap_asr::validated_word_alignment(segment, run),
            #[cfg(feature = "internal-player")]
            timing_evidence: (
                segment.extra.get("raw_token_timing").cloned(),
                segment.extra.get("word_alignment_v1").cloned(),
            ),
        }
    }
}

pub(super) struct TranscriptView {
    pub state: ListState,
    rows: Rc<Vec<Rc<Row>>>,
    run_key: Option<(String, String)>,
    #[cfg(feature = "internal-player")]
    #[allow(clippy::type_complexity)]
    timing_run_key: Option<(
        Option<String>,
        Option<serde_json::Value>,
        Option<serde_json::Value>,
        Option<serde_json::Value>,
    )>,
    live: bool,
    compact: bool,
    read_only: bool,
    task_running: Option<bool>,
    speaker_turns: Vec<(u64, u64, u32, bool)>,
    speaker_names: Vec<(u32, String)>,
    search_input: Option<Entity<InputState>>,
    search_subscription: Option<Subscription>,
    query: String,
    matches: Vec<(usize, std::ops::Range<usize>)>,
    active_match: usize,
    #[cfg(feature = "internal-player")]
    playback_timeline: opencap_media::SegmentTimeline,
    #[cfg(feature = "internal-player")]
    playback_active: std::collections::BTreeSet<u64>,
    #[cfg(feature = "internal-player")]
    playback_follow: bool,
    #[cfg(feature = "internal-player")]
    playback_attached: bool,
    #[cfg(feature = "internal-player")]
    playback_available: bool,
    #[cfg(feature = "internal-player")]
    playback_words: std::collections::BTreeMap<u64, Vec<std::ops::Range<usize>>>,
    #[cfg(feature = "internal-player")]
    playback_word_rows: std::collections::BTreeMap<u64, opencap_asr::WordAlignment>,
    #[cfg(feature = "internal-player")]
    playback_gaps: Vec<(u64, u64)>,
    #[cfg(feature = "internal-player")]
    drag_anchor: Option<(u64, u64)>,
    #[cfg(feature = "internal-player")]
    rows_generation: u64,
    #[cfg(feature = "internal-player")]
    expanded_token_row: Option<u64>,
    #[cfg(feature = "internal-player")]
    token_page: usize,
    #[cfg(feature = "ui-smoke")]
    pub measured_rows: Rc<std::cell::RefCell<std::collections::BTreeSet<usize>>>,
}
impl EventEmitter<ReviewSelection> for TranscriptView {}
#[cfg(feature = "internal-player")]
impl EventEmitter<PlaybackSeek> for TranscriptView {}
#[cfg(feature = "internal-player")]
impl EventEmitter<PlaybackSelection> for TranscriptView {}
#[cfg(feature = "internal-player")]
impl EventEmitter<PlaybackRangeSelection> for TranscriptView {}
#[cfg(feature = "internal-player")]
impl EventEmitter<PlaybackTokenSeek> for TranscriptView {}
#[cfg(feature = "internal-player")]
impl EventEmitter<PlaybackWordRange> for TranscriptView {}
impl TranscriptView {
    #[cfg(feature = "ui-smoke")]
    pub(super) fn smoke_row_count(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn reveal_segment(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(|row| row.id == id) {
            #[cfg(feature = "internal-player")]
            {
                self.playback_follow = false;
            }
            self.state.set_follow_mode(FollowMode::Normal);
            self.state.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }
    pub fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(160.)),
            rows: Rc::new(Vec::new()),
            run_key: None,
            #[cfg(feature = "internal-player")]
            timing_run_key: None,
            live: false,
            compact: false,
            read_only: false,
            task_running: None,
            speaker_turns: Vec::new(),
            speaker_names: Vec::new(),
            search_input: None,
            search_subscription: None,
            query: String::new(),
            matches: Vec::new(),
            active_match: 0,
            #[cfg(feature = "internal-player")]
            playback_timeline: opencap_media::SegmentTimeline::default(),
            #[cfg(feature = "internal-player")]
            playback_active: Default::default(),
            #[cfg(feature = "internal-player")]
            playback_follow: true,
            #[cfg(feature = "internal-player")]
            playback_attached: false,
            #[cfg(feature = "internal-player")]
            playback_available: false,
            #[cfg(feature = "internal-player")]
            playback_words: Default::default(),
            #[cfg(feature = "internal-player")]
            playback_word_rows: Default::default(),
            #[cfg(feature = "internal-player")]
            playback_gaps: Vec::new(),
            #[cfg(feature = "internal-player")]
            drag_anchor: None,
            #[cfg(feature = "internal-player")]
            rows_generation: 0,
            #[cfg(feature = "internal-player")]
            expanded_token_row: None,
            #[cfg(feature = "internal-player")]
            token_page: 0,
            #[cfg(feature = "ui-smoke")]
            measured_rows: Rc::new(std::cell::RefCell::new(Default::default())),
        }
    }
    pub fn replace(
        &mut self,
        transcript: &Transcript,
        new_document: bool,
        task_running: bool,
        cx: &mut Context<Self>,
    ) {
        self.task_running = Some(task_running);
        self.update_rows(transcript, new_document);
        cx.notify();
    }
    pub(super) fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            cx.notify();
        }
    }
    pub(super) fn set_read_only(&mut self) {
        self.read_only = true;
    }
    #[cfg(feature = "internal-player")]
    pub(super) fn set_playback_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.playback_available != available {
            self.playback_available = available;
            cx.notify();
        }
    }
    fn update_rows(&mut self, transcript: &Transcript, new_document: bool) {
        let live = self
            .task_running
            .unwrap_or(!transcript.complete && !transcript.cancelled)
            && !transcript.complete
            && !transcript.cancelled;
        #[cfg(feature = "internal-player")]
        let duplicate_ids = {
            let mut seen = std::collections::HashSet::new();
            let mut duplicates = std::collections::HashSet::new();
            for segment in &transcript.segments {
                if !seen.insert(segment.id) {
                    duplicates.insert(segment.id);
                }
            }
            duplicates
        };
        #[cfg(feature = "internal-player")]
        {
            self.playback_timeline = opencap_media::SegmentTimeline::new(
                transcript
                    .segments
                    .iter()
                    .filter(|s| !duplicate_ids.contains(&s.id))
                    .filter(|s| {
                        s.extra
                            .get("terminal_unconfirmed")
                            .and_then(|value| value.as_bool())
                            != Some(true)
                    })
                    .filter(|s| live || s.is_confirmed())
                    .map(|s| opencap_media::TimedSegment {
                        id: s.id,
                        start_ms: s.start_ms,
                        end_ms: s.end_ms,
                    })
                    .collect(),
            );
            self.playback_gaps = transcript.gaps.clone();
            if new_document {
                self.playback_active.clear();
                self.playback_words.clear();
                self.playback_follow = true;
                self.expanded_token_row = None;
                self.token_page = 0;
            }
        }
        let speakers: Vec<_> = transcript
            .speaker_turns
            .iter()
            .map(|s| {
                (
                    s.start_ms,
                    s.end_ms,
                    s.speaker,
                    s.extra
                        .get("identity_state")
                        .and_then(|value| value.as_str())
                        == Some("tentative"),
                )
            })
            .collect();
        let names: Vec<_> = transcript
            .speaker_names
            .keys()
            .map(|id| (*id, transcript.speaker_name(*id)))
            .collect();
        let run_key = transcript
            .run
            .as_ref()
            .map(|run| (run.run_id.clone(), run.model_id.clone()));
        let run_changed = self.run_key != run_key;
        #[cfg(feature = "internal-player")]
        let timing_run_key = transcript.run.as_ref().map(|run| {
            (
                run.input_asset_id.clone(),
                run.extra.get("input_media_fingerprint").cloned(),
                run.extra.get("input_audio").cloned(),
                run.extra.get("input_integrity").cloned(),
            )
        });
        #[cfg(feature = "internal-player")]
        let timing_run_changed = self.timing_run_key != timing_run_key;
        #[cfg(not(feature = "internal-player"))]
        let timing_run_changed = false;
        let labels_changed =
            new_document || self.speaker_turns != speakers || self.speaker_names != names;
        let timeline = transcript.speaker_timeline();
        // Retain every segment while live. Shared strings avoid rebuilding all
        // history and its metadata whenever the current hypothesis changes.
        let rows: Vec<Rc<Row>> = transcript
            .segments
            .iter()
            .enumerate()
            .map(|(index, s)| {
                let existing = (!new_document && !run_changed && !timing_run_changed)
                    .then(|| self.rows.get(index).filter(|row| row.matches(s, live)))
                    .flatten();
                if let Some(row) = existing
                    && !labels_changed
                {
                    return row.clone();
                }
                let speaker = timeline.label(s.start_ms, s.end_ms);
                if let Some(row) = existing
                    && row.speaker.as_ref() == speaker.as_str()
                {
                    return row.clone();
                }
                Rc::new(Row::new(s, speaker, transcript.run.as_ref(), live))
            })
            .collect();
        #[cfg(feature = "internal-player")]
        {
            self.playback_word_rows = rows
                .iter()
                .filter(|row| !duplicate_ids.contains(&row.id))
                .filter_map(|row| {
                    row.word_alignment
                        .clone()
                        .map(|alignment| (row.id, alignment))
                })
                .collect();
            self.playback_words.clear();
        }
        #[cfg(feature = "internal-player")]
        if new_document
            || run_changed
            || self.rows.len() != rows.len()
            || self.rows.iter().zip(&rows).any(|(old, new)| old != new)
        {
            self.rows_generation = self.rows_generation.saturating_add(1);
        }
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
        self.speaker_names = names;
        self.run_key = run_key;
        #[cfg(feature = "internal-player")]
        {
            self.timing_run_key = timing_run_key;
        }
        self.rows = Rc::new(rows);
        self.refresh_matches();
    }
    fn refresh_matches(&mut self) {
        if self.query.is_empty() {
            self.matches.clear();
            self.active_match = 0;
            return;
        }
        self.matches = self
            .rows
            .iter()
            .enumerate()
            .flat_map(|(i, row)| {
                match_ranges(&row.text, &self.query)
                    .into_iter()
                    .map(move |range| (i, range))
            })
            .collect();
        self.active_match = self.active_match.min(self.matches.len().saturating_sub(1));
    }
    pub(super) fn set_query(&mut self, query: String) {
        if self.query == query {
            return;
        }
        self.query = query;
        #[cfg(feature = "internal-player")]
        if !self.query.is_empty() {
            self.playback_follow = false;
        }
        self.active_match = 0;
        self.refresh_matches();
        self.reveal_match();
    }
    #[cfg(feature = "internal-player")]
    pub(super) fn set_playback(
        &mut self,
        position: Option<u64>,
        verified_media: bool,
        cx: &mut Context<Self>,
    ) {
        let attached = position.is_some();
        let active: std::collections::BTreeSet<_> = position
            .filter(|position| {
                !self
                    .playback_gaps
                    .iter()
                    .any(|(start, end)| start <= position && position < end)
            })
            .map(|position| {
                self.playback_timeline
                    .active_at(position)
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();
        let words: std::collections::BTreeMap<u64, Vec<std::ops::Range<usize>>> = position
            .filter(|_| verified_media)
            .map(|position| {
                active
                    .iter()
                    .filter_map(|id| {
                        let ranges: Vec<_> = self
                            .playback_word_rows
                            .get(id)?
                            .words
                            .iter()
                            .filter(|word| word.start_ms <= position && position < word.end_ms)
                            .map(|word| word.start_byte..word.end_byte)
                            .collect();
                        (!ranges.is_empty()).then_some((*id, ranges))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if active == self.playback_active
            && attached == self.playback_attached
            && words == self.playback_words
        {
            return;
        }
        let active_changed = active != self.playback_active;
        self.playback_attached = attached;
        self.playback_active = active;
        self.playback_words = words;
        if self.playback_follow
            && active_changed
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| self.playback_active.contains(&row.id))
        {
            self.state.set_follow_mode(FollowMode::Normal);
            self.state.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }
    fn reveal_match(&self) {
        if let Some((row, _)) = self.matches.get(self.active_match) {
            self.state.set_follow_mode(FollowMode::Normal);
            self.state.scroll_to(ListOffset {
                item_ix: *row,
                offset_in_item: px(0.),
            });
        }
    }
    fn step_match(&mut self, backwards: bool) {
        if self.matches.is_empty() {
            return;
        }
        #[cfg(feature = "internal-player")]
        {
            self.playback_follow = false;
        }
        self.active_match = if backwards {
            (self.active_match + self.matches.len() - 1) % self.matches.len()
        } else {
            (self.active_match + 1) % self.matches.len()
        };
        self.reveal_match();
    }
}
fn match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    text.to_ascii_lowercase()
        .match_indices(&query.to_ascii_lowercase())
        .map(|(start, word)| start..start + word.len())
        .collect()
}
fn selectable_html(text: &str, query: &str, playback: &[std::ops::Range<usize>]) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "<br/>")
    }
    let mut html = String::from("<p>");
    let search = match_ranges(text, query);
    let mut boundaries = vec![0, text.len()];
    for range in search.iter().chain(playback) {
        if range.start <= range.end && text.get(range.clone()).is_some() {
            boundaries.extend([range.start, range.end]);
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        let searching = search
            .iter()
            .any(|r| r.start <= range.start && range.end <= r.end);
        let playing = playback
            .iter()
            .any(|r| r.start <= range.start && range.end <= r.end);
        if playing {
            html.push_str("<mark color=\"#bae6fd\">");
            if searching {
                html.push_str("<strong>");
            }
        } else if searching {
            html.push_str("<mark>");
        }
        html.push_str(&escape(&text[range]));
        if playing && searching {
            html.push_str("</strong>");
        }
        if playing || searching {
            html.push_str("</mark>");
        }
    }
    html.push_str("</p>");
    html
}
impl Render for TranscriptView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.search_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索逐字稿…")
                    .default_value(self.query.clone())
            });
            self.search_subscription = Some(cx.observe(&input, |this, input, cx| {
                this.set_query(input.read(cx).value().to_string());
                cx.notify();
            }));
            self.search_input = Some(input);
        }
        if self.rows.is_empty() {
            return div()
                .size_full()
                .child(label("文字会按时间保留在这里，可随时向上查看历史。"));
        }
        let rows = self.rows.clone();
        #[cfg(feature = "internal-player")]
        let run_id = self.run_key.as_ref().map(|(id, _)| id.clone());
        let following = self.state.is_following_tail();
        let compact = self.compact;
        let read_only = self.read_only;
        let query = self.query.clone();
        let copy_rows = rows.clone();
        let selection_view = cx.entity().downgrade();
        #[cfg(feature = "internal-player")]
        let playback_active = self.playback_active.clone();
        #[cfg(feature = "internal-player")]
        let playback_attached = self.playback_attached;
        #[cfg(feature = "internal-player")]
        let playback_available = self.playback_available;
        #[cfg(feature = "internal-player")]
        let playback_words = self.playback_words.clone();
        #[cfg(feature = "internal-player")]
        let expanded_token_row = self.expanded_token_row;
        #[cfg(feature = "internal-player")]
        let token_page = self.token_page;
        #[cfg(feature = "ui-smoke")]
        let measured = self.measured_rows.clone();
        div()
            .size_full()
            // Let the containing home page move only once this list reaches
            // the edge; otherwise keep trackpad scrolling inside the text.
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                #[cfg(feature = "internal-player")]
                {
                    this.playback_follow = false;
                }
                let delta = event.delta.pixel_delta(px(20.)).y;
                let top = this.state.logical_scroll_top();
                let at_top = top.item_ix == 0 && top.offset_in_item <= px(0.);
                let remaining = this.state.max_offset_for_scrollbar().y
                    + this.state.scroll_px_offset_for_scrollbar().y;
                let at_end = this.state.is_following_tail()
                    || this.state.is_scrolled_to_end() == Some(true)
                    || remaining <= px(1.);
                let page_edge = this.compact && ((delta > px(0.) && at_top) || (delta < px(0.) && at_end));
                if !page_edge {
                    cx.stop_propagation();
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    #[cfg(feature = "internal-player")]
                    if this.playback_attached {
                        this.playback_follow = false;
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .gap_2()
            .when(!compact, |d| d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Input::new(self.search_input.as_ref().unwrap())
                            .flex_1()
                            .min_w(px(100.)),
                    )
                    .child(label(if self.query.is_empty() {
                        "搜索原文".into()
                    } else {
                        format!(
                            "{}/{}",
                            if self.matches.is_empty() {
                                0
                            } else {
                                self.active_match + 1
                            },
                            self.matches.len()
                        )
                    }))
                    .child(
                        button("transcript-prev", "上一处", false)
                            .ghost()
                            .disabled(self.matches.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_match(true);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("transcript-next", "下一处", false)
                            .ghost()
                            .disabled(self.matches.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step_match(false);
                                cx.notify();
                            })),
                    )
                    .child(button("copy-transcript", "复制全文", false).on_click(
                        move |_, _, cx| {
                            let text = copy_rows
                                .iter()
                                .map(|row| if row.terminal_unconfirmed {
                                    format!("[未定稿暂存文字] {}", row.text)
                                } else if !row.confirmed {
                                    format!("[识别或复核中，尚未定稿] {}", row.text)
                                } else { row.text.to_string() })
                                .collect::<Vec<_>>()
                                .join("\n");
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        },
                    )),
            ))
            .when(cfg!(feature = "internal-player"), |d| {
                #[cfg(feature = "internal-player")]
                {
                    if self.playback_attached && !self.playback_follow {
                        return d.child(
                            button("follow-playback", "跟随播放", false)
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.playback_follow = true;
                                    if let Some(index) = this
                                        .rows
                                        .iter()
                                        .position(|row| this.playback_active.contains(&row.id))
                                    {
                                        this.state.scroll_to(ListOffset {
                                            item_ix: index,
                                            offset_in_item: px(0.),
                                        });
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                }
                d
            })
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
                                    #[cfg(feature = "internal-player")]
                                    {
                                        this.playback_follow = false;
                                    }
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
                            let review_selection = (!read_only).then_some(row.review.as_ref()).flatten().and_then(|review| {
                                let revised = review.revised_text.as_ref()?;
                                if !row.finalized || revised == &review.original_text {
                                    return None;
                                }
                                match review.state {
                                    ReviewState::Reviewed => Some((true, "选择流式原文")),
                                    ReviewState::Restored => Some((false, "选择复核结果")),
                                    _ => None,
                                }
                            });
                            let selection_view = selection_view.clone();
                            let row_id = row.id;
                            #[cfg(feature = "internal-player")]
                            let row_start = row.start_ms;
                            #[cfg(feature = "internal-player")]
                            let row_end = row.end_ms;
                            #[cfg(feature = "internal-player")]
                            let row_revision = row.revision;
                            #[cfg(feature = "internal-player")]
                            let row_text = row.text.clone();
                            #[cfg(feature = "internal-player")]
                            let row_run_id = run_id.clone();
                            #[cfg(feature = "internal-player")]
                            let identity = PlaybackSeek {
                                milliseconds: row_start, end_ms: row_end, segment_id: row_id,
                                revision: row_revision, text: row_text, run_id: row_run_id,
                            };
                            #[cfg(feature = "internal-player")]
                            let seek_identity = identity.clone();
                            #[cfg(feature = "internal-player")]
                            let seek_view = selection_view.clone();
                            #[cfg(feature = "internal-player")]
                            let select_view = selection_view.clone();
                            #[cfg(feature = "internal-player")]
                            let drag_view = selection_view.clone();
                            #[cfg(feature = "internal-player")]
                            let release_view = selection_view.clone();
                            #[cfg(feature = "internal-player")]
                            let token_toggle_view = selection_view.clone();
                            #[cfg(feature = "internal-player")]
                            let seek = (playback_attached || playback_available).then(||
                                button(("seek-transcript", row_id as usize), "从此处播放", false)
                                    .disabled(!row.confirmed || row.end_ms <= row.start_ms)
                                    .on_click(move |_, _, cx| {
                                        let _ = seek_view.update(cx, |_, cx| {
                                            cx.emit(seek_identity.clone())
                                        });
                                    }),
                            );
                            #[cfg(not(feature = "internal-player"))]
                            let seek: Option<Button> = None;
                            #[cfg(feature = "internal-player")]
                            let select = playback_attached.then(||
                                button(
                                    ("select-transcript-segment", row_id as usize),
                                    "选段",
                                    false,
                                )
                                .disabled(!playback_attached || !row.confirmed || row.end_ms <= row.start_ms)
                                .on_click(move |_, _, cx| {
                                    let _ = select_view.update(cx, |_, cx| {
                                        cx.emit(PlaybackSelection { segment: identity.clone() })
                                    });
                                }),
                            );
                            #[cfg(not(feature = "internal-player"))]
                            let select: Option<Button> = None;
                            #[cfg(feature = "internal-player")]
                            let token_toggle = (row.token_starts.is_some() || row.word_alignment.is_some()).then(|| {
                                button(("token-toggle", row_id as usize), if expanded_token_row == Some(row_id) { "收起时间详情" } else { "时间详情" }, false)
                                    .ghost().on_click(move |_, _, cx| {
                                        let _ = token_toggle_view.update(cx, |view, cx| {
                                            view.expanded_token_row = if view.expanded_token_row == Some(row_id) { None } else { Some(row_id) };
                                            view.token_page = 0;
                                            cx.notify();
                                        });
                                    })
                            });
                            #[cfg(not(feature = "internal-player"))]
                            let token_toggle: Option<Button> = None;
                            #[cfg(feature = "internal-player")]
                            let word_panel = row.word_alignment.as_ref().filter(|_| expanded_token_row == Some(row_id)).map(|alignment| {
                                const WORDS_PER_PAGE: usize = 30;
                                let pages = alignment.words.len().saturating_sub(1) / WORDS_PER_PAGE + 1;
                                let page = token_page.min(pages - 1);
                                let mut words = div().flex().flex_wrap().gap_1();
                                for (index, word) in alignment.words.iter().enumerate().skip(page * WORDS_PER_PAGE).take(WORDS_PER_PAGE) {
                                    let separator_start = index.checked_sub(1)
                                        .map(|previous| alignment.words[previous].end_byte).unwrap_or(0);
                                    if let Some(separator) = row.text.get(separator_start..word.start_byte).filter(|text| !text.is_empty()) {
                                        words = words.child(label(separator.to_owned()));
                                    }
                                    let text = row.text.get(word.start_byte..word.end_byte).unwrap_or("").to_owned();
                                    let selected_word = word.clone();
                                    let source_run_id = alignment.source_run_id.clone();
                                    let alignment_run_id = alignment.alignment_run_id.clone();
                                    let text_revision = alignment.text_revision;
                                    let text_sha256 = alignment.text_sha256.clone();
                                    let word_view = selection_view.clone();
                                    words = words.child(button(format!("aligned-word-{row_id}-{index}"), text, false)
                                        .ghost().disabled(!playback_attached).on_click(move |_, _, cx| {
                                            let _ = word_view.update(cx, |_, cx| cx.emit(PlaybackWordRange {
                                                segment_id: row_id, source_run_id: source_run_id.clone(),
                                                alignment_run_id: alignment_run_id.clone(), text_revision,
                                                text_sha256: text_sha256.clone(), word: selected_word.clone(),
                                            }));
                                        }));
                                }
                                if page + 1 == pages
                                    && let Some(separator) = alignment.words.last()
                                        .and_then(|word| row.text.get(word.end_byte..))
                                        .filter(|text| !text.is_empty()) {
                                    words = words.child(label(separator.to_owned()));
                                }
                                let mut panel = div().flex().flex_col().gap_1()
                                    .child(label(if alignment.timing_detail.as_deref() == Some("decoder_emission_frame_80ms_not_forced_alignment") {
                                        "模型词时间使用约 80 毫秒发射帧网格，未做强制对齐或精度验收；打开并核对原媒体后可回听。"
                                    } else {
                                        "已对齐的词：打开并核对原媒体后可播放对应范围；普通文字为未计时的空白或标点。"
                                    }))
                                    .child(words);
                                if pages > 1 {
                                    let previous = selection_view.clone();
                                    let next = selection_view.clone();
                                    panel = panel.child(div().flex().items_center().gap_2()
                                        .child(button(format!("words-prev-{row_id}"), "上一组", false).disabled(page == 0).on_click(move |_, _, cx| {
                                            let _ = previous.update(cx, |view, cx| { view.token_page = page.saturating_sub(1); cx.notify(); });
                                        }))
                                        .child(label(format!("{} / {}", page + 1, pages)))
                                        .child(button(format!("words-next-{row_id}"), "下一组", false).disabled(page + 1 >= pages).on_click(move |_, _, cx| {
                                            let _ = next.update(cx, |view, cx| { view.token_page = (page + 1).min(pages - 1); cx.notify(); });
                                        })));
                                }
                                panel
                            });
                            #[cfg(not(feature = "internal-player"))]
                            let word_panel: Option<Div> = None;
                            #[cfg(feature = "internal-player")]
                            let token_panel = row.token_starts.as_ref().filter(|_| expanded_token_row == Some(row_id) && row.word_alignment.is_none()).map(|mapping| {
                                const TOKENS_PER_PAGE: usize = 30;
                                let pages = mapping.tokens.len().saturating_sub(1) / TOKENS_PER_PAGE + 1;
                                let page = token_page.min(pages - 1);
                                let mut panel = div().flex().flex_col().gap_1()
                                    .child(label("识别 token 的真实起点；不代表词尾或强制对齐。"));
                                let mut tokens = div().flex().flex_wrap().gap_1();
                                for (index, token) in mapping.tokens.iter().enumerate().skip(page * TOKENS_PER_PAGE).take(TOKENS_PER_PAGE) {
                                    let text = row.text.get(token.text_range.clone()).unwrap_or("").to_owned();
                                    let start = token.start_ms;
                                    let text_range = token.text_range.clone();
                                    let text_revision = mapping.text_revision;
                                    let source_run_id = mapping.source_run_id.clone();
                                    let text_sha256 = mapping.text_sha256.clone();
                                    let token_view = selection_view.clone();
                                    tokens = tokens.child(button(format!("token-{row_id}-{index}"),
                                        format!("{} · {}", text, opencap_asr::timestamp(start, '.')), false)
                                        .ghost().on_click(move |_, _, cx| {
                                            let _ = token_view.update(cx, |_, cx| cx.emit(PlaybackTokenSeek {
                                                segment_id: row_id, text_revision, source_run_id: source_run_id.clone(),
                                                text_sha256: text_sha256.clone(), text_range: text_range.clone(), start_ms: start,
                                            }));
                                        }));
                                }
                                panel = panel.child(tokens);
                                if pages > 1 {
                                    let previous = selection_view.clone();
                                    let next = selection_view.clone();
                                    panel = panel.child(div().flex().items_center().gap_2()
                                        .child(button(format!("tokens-prev-{row_id}"), "上一组", false).disabled(page == 0).on_click(move |_, _, cx| {
                                            let _ = previous.update(cx, |view, cx| { view.token_page = page.saturating_sub(1); cx.notify(); });
                                        }))
                                        .child(label(format!("{} / {}", page + 1, pages)))
                                        .child(button(format!("tokens-next-{row_id}"), "下一组", false).disabled(page + 1 >= pages).on_click(move |_, _, cx| {
                                            let _ = next.update(cx, |view, cx| { view.token_page = (page + 1).min(pages - 1); cx.notify(); });
                                        })));
                                }
                                panel
                            });
                            #[cfg(not(feature = "internal-player"))]
                            let token_panel: Option<Div> = None;
                            div()
                                .w_full()
                                .pr_3()
                                .pb_3()
                                .when(compact, |d| d.p_3().mb_2().rounded_md().border_1().border_color(rgb(BORDER)).bg(rgb(0xf8fafd)))
                                .when(cfg!(feature = "internal-player"), |d| {
                                    #[cfg(feature = "internal-player")]
                                    {
                                        if playback_active.contains(&row.id) {
                                            return d.bg(rgb(SOFT)).rounded_md();
                                        }
                                    }
                                    d
                                })
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_2()
                                        .child(label(row.metadata.clone()).flex_1().min_w(px(110.)))
                                        .child(div().flex().flex_wrap().items_center().gap_1().ml_auto()
                                            .when_some(seek, |d, seek| d.child(seek))
                                            .when_some(select, |d, select| d.child(select))
                                            .when_some(token_toggle, |d, toggle| d.child(toggle))
                                            .when_some(review_selection, |d, (original, title)| {
                                                d.child(
                                                    button(("review-selection", row_id as usize), title, false)
                                                        .ghost().on_click(move |_, _, cx| {
                                                            let _ = selection_view.update(cx, |_, cx| {
                                                                cx.emit(ReviewSelection { id: row_id, original });
                                                            });
                                                        }),
                                                )
                                            })),
                                )
                                .when_some(token_panel, |d, panel| d.child(panel))
                                .when_some(word_panel, |d, panel| d.child(panel))
                                .child(
                                    div()
                                        .id(("transcript-selectable", row.id as usize))
                                        .text_base()
                                        .text_color(rgb(if row.confirmed { TEXT } else { ACCENT }))
                                        .when(cfg!(feature = "internal-player"), |d| {
                                            #[cfg(feature = "internal-player")]
                                            let d = d.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                                                if gpui::base::TextSelection::is_selectable_at(event.position, window, cx) {
                                                    let _ = drag_view.update(cx, |view, _| {
                                                        view.drag_anchor = Some((row_id, view.rows_generation));
                                                    });
                                                }
                                            }).on_mouse_up(MouseButton::Left, move |event, window, cx| {
                                                let selected = gpui::base::TextSelection::is_selectable_at(event.position, window, cx)
                                                    && gpui::base::TextSelection::has_selection(window, cx)
                                                    && !gpui::base::TextSelection::selected_text(window, cx).trim().is_empty();
                                                let _ = release_view.update(cx, |view, cx| {
                                                    if let Some((first_id, generation)) = view.drag_anchor.take()
                                                        && selected && generation == view.rows_generation {
                                                        let first = view.rows.iter().position(|row| row.id == first_id);
                                                        let last = view.rows.iter().position(|row| row.id == row_id);
                                                        if let (Some(first), Some(last)) = (first, last) {
                                                            let rows = view.rows[first.min(last)..=first.max(last)].iter().map(|row| PlaybackRangeRow {
                                                                id: row.id, revision: row.revision, start_ms: row.start_ms,
                                                                end_ms: row.end_ms, text: row.text.clone(),
                                                            }).collect();
                                                            cx.emit(PlaybackRangeSelection {
                                                                run_id: view.run_key.as_ref().map(|(id, _)| id.clone()), rows,
                                                            });
                                                        }
                                                    }
                                                });
                                            });
                                            d
                                        })
                                        .child(
                                            TextView::html(
                                                ("transcript-text", row.id as usize),
                                                selectable_html(&row.text, &query, {
                                                    #[cfg(feature = "internal-player")]
                                                    { playback_words.get(&row.id).map(Vec::as_slice).unwrap_or(&[]) }
                                                    #[cfg(not(feature = "internal-player"))]
                                                    { &[] }
                                                }),
                                            )
                                            .selectable(true)
                                            .selection_format(SelectionFormat::Plain)
                                            .scrollable(false),
                                        ),
                                )
                                .when_some(row.review.as_ref(), |d, review| {
                                    let other = if review.state == ReviewState::Restored {
                                        review
                                            .revised_text
                                            .as_deref()
                                            .map(|text| ("复核结果", text))
                                    } else if review.state == ReviewState::Reviewed {
                                        Some(("流式原文", review.original_text.as_str()))
                                    } else {
                                        None
                                    };
                                    d.when_some(
                                        other.filter(|(_, text)| *text != row.text.as_ref()),
                                        |d, (kind, text)| d.child(label(format!("{kind}：{text}"))),
                                    )
                                })
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
    fn chinese_search_navigates_history_and_highlights_literal_text_without_changing_copy() {
        let mut transcript = live_transcript(50);
        transcript.segments[3].text = "研发预算 A&B <减少>".into();
        transcript.segments[40].text = "确认预算不能增加".into();
        let mut view = TranscriptView::new();
        view.update_rows(&transcript, true);
        view.set_query("预算".into());
        assert_eq!(view.matches.len(), 2);
        assert_eq!(view.state.logical_scroll_top().item_ix, 3);
        view.step_match(false);
        assert_eq!(view.state.logical_scroll_top().item_ix, 40);
        assert!(!view.state.is_following_tail());
        let html = super::selectable_html(&transcript.segments[3].text, "预算", &[]);
        assert!(html.contains("<mark>预算</mark>") && html.contains("A&amp;B &lt;减少&gt;"));
        assert_eq!(view.rows[3].text.as_ref(), "研发预算 A&B <减少>");
        assert_eq!(super::match_ranges("会议 API api", "API").len(), 2);
        view.set_query("不存在".into());
        view.step_match(true);
        assert!(view.matches.is_empty());
    }
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
                    extra: Default::default(),
                    id,
                    revision: 1,
                    start_ms: id * 1000,
                    end_ms: id * 1000 + 900,
                    text: format!("第 {id} 段会议内容"),
                    finalized: true,
                    correcting: false,
                    review: None,
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
            extra: Default::default(),
            id: 5000,
            revision: 1,
            start_ms: 5_000_000,
            end_ms: 5_000_500,
            text: "正在说的新内容".into(),
            finalized: false,
            correcting: false,
            review: None,
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
            extra: Default::default(),
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
