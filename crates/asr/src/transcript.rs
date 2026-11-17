use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Provenance of a sentence-level second ASR pass. The streaming text remains
/// available even if a later model changes the displayed sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Pending,
    Reviewed,
    Restored,
    Deferred,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentenceReview {
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
    pub state: ReviewState,
    pub original_text: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revised_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
    pub id: u64,
    pub revision: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub finalized: bool,
    /// Endpoint reached; the independent sentence decoder may still replace it.
    #[serde(default)]
    pub correcting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<SentenceReview>,
}
impl Segment {
    /// Only stable content may be treated as a confirmed transcript source.
    pub fn is_confirmed(&self) -> bool {
        self.finalized
            && !self.correcting
            && !self
                .review
                .as_ref()
                .is_some_and(|review| review.state == ReviewState::Pending)
            && self
                .extra
                .get("terminal_unconfirmed")
                .and_then(|value| value.as_bool())
                != Some(true)
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    /// Absent on historical transcripts; never inferred from current settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<TranscriptionRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<crate::TranscriptCorrection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_duration_ms: Option<u64>,
    pub source: Option<PathBuf>,
    pub model: String,
    pub complete: bool,
    pub cancelled: bool,
    pub gaps: Vec<(u64, u64)>,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub speaker_turns: Vec<SpeakerTurn>,
    #[serde(default)]
    pub speaker_names: std::collections::BTreeMap<u32, crate::SpeakerName>,
    #[serde(default)]
    pub speaker_voiceprints: std::collections::BTreeMap<u32, crate::Voiceprint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub speaker_history: Vec<SpeakerAnalysisVersion>,
    #[serde(default)]
    pub speaker_memory_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_analysis_seconds: Option<f64>,
    #[serde(default)]
    pub output_warnings: Vec<String>,
    /// Runtime-only indication that the current in-memory primary JSON could
    /// not be saved; derived export failures are tracked separately.
    #[serde(skip)]
    pub primary_save_pending: bool,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerAnalysisVersion {
    pub version_id: String,
    pub created_unix_ms: u64,
    pub turns: Vec<SpeakerTurn>,
    pub names: std::collections::BTreeMap<u32, crate::SpeakerName>,
    pub voiceprints: std::collections::BTreeMap<u32, crate::Voiceprint>,
    pub status: String,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A snapshot records only values known to have been selected for this run.
/// Unavailable hashes, stream identifiers and timings remain absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionRun {
    pub run_id: String,
    pub started_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
    pub application_version: String,
    pub backend: String,
    pub runtime_version: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<String>,
    pub provider: String,
    pub threads: u32,
    pub requested_language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_language: Option<String>,
    pub requested_hotwords: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_hotwords: Option<Vec<String>>,
    pub punctuation_requested: bool,
    pub punctuation_effective: String,
    pub revision_scheduled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_model: Option<String>,
    pub input_kind: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_asset_id: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl TranscriptionRun {
    /// Record that early text used the fallback before the model became ready.
    pub fn mark_late_punctuation(
        &mut self,
        start_ms: u64,
        fingerprint: &opencap_core::MediaFingerprint,
    ) {
        self.punctuation_effective = "ct_transformer_partial".into();
        self.extra
            .insert("punctuation_started_ms".into(), start_ms.into());
        self.extra.insert(
            "punctuation_fingerprint_verification".into(),
            "before_after_load_matched".into(),
        );
        self.extra.insert(
            "punctuation_fingerprint".into(),
            serde_json::json!(fingerprint),
        );
    }
}

/// An exact mapping of backend token *starts* to UTF-8 byte ranges. This is
/// not forced alignment and says nothing about when the token stops sounding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedTokenStart {
    pub text_range: std::ops::Range<usize>,
    pub start_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactTokenStarts {
    pub source_run_id: String,
    pub model_id: String,
    pub text_revision: u64,
    pub text_sha256: String,
    pub tokens: Vec<MappedTokenStart>,
}

/// Word boundaries supplied by an actual ASR word-timing or forced-alignment
/// run. Offsets are UTF-8 bytes in the exact displayed segment text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlignedWord {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WordAlignment {
    pub source_run_id: String,
    /// Fingerprint and audio stream used by the alignment job, independent
    /// of the transcript's own record of its original media.
    #[serde(default)]
    pub source_media_fingerprint: Option<opencap_core::MediaFingerprint>,
    #[serde(default)]
    pub source_audio_stream_index: Option<u64>,
    #[serde(default)]
    pub source_asset_id: Option<String>,
    pub segment_id: u64,
    pub segment_start_ms: u64,
    pub segment_end_ms: u64,
    pub text_revision: u64,
    pub text_sha256: String,
    pub alignment_run_id: String,
    pub model_id: String,
    /// Only real model word boundaries or forced-alignment results qualify.
    pub method: String,
    /// Optional details of the backend-reported time grid and model files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_sha256: Option<String>,
    pub words: Vec<AlignedWord>,
}

/// Validate persisted alignment against the current text and run every time
/// it is used. Untimed whitespace and punctuation may separate words; they
/// are never assigned a fabricated time. Never infer a word end from a token
/// start or the next word.
pub fn validated_word_alignment(
    segment: &Segment,
    run: Option<&TranscriptionRun>,
) -> Option<WordAlignment> {
    use sha2::{Digest, Sha256};
    let run = run?;
    if !segment.is_confirmed()
        || run.run_id.is_empty()
        || segment.text.len() > 100_000
        || segment.end_ms <= segment.start_ms
    {
        return None;
    }
    let alignment: WordAlignment =
        serde_json::from_value(segment.extra.get("word_alignment_v1")?.clone()).ok()?;
    let fingerprint: opencap_core::MediaFingerprint =
        serde_json::from_value(run.extra.get("input_media_fingerprint")?.clone()).ok()?;
    let stream_index = run
        .extra
        .get("input_audio")?
        .get("audio_stream_index")?
        .as_u64()?;
    let asset_id = run
        .input_asset_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?;
    if alignment.source_run_id != run.run_id
        || alignment.segment_id != segment.id
        || run.extra.get("input_integrity")?.as_str()? != "hash_matched_before_and_after"
        || alignment.source_media_fingerprint.as_ref() != Some(&fingerprint)
        || alignment.source_audio_stream_index != Some(stream_index)
        || alignment.source_asset_id.as_deref() != Some(asset_id)
        || alignment.segment_start_ms != segment.start_ms
        || alignment.segment_end_ms != segment.end_ms
        || alignment.text_revision != segment.revision
        || alignment.text_sha256 != format!("{:x}", Sha256::digest(segment.text.as_bytes()))
        || alignment.alignment_run_id.trim().is_empty()
        || alignment.model_id.trim().is_empty()
        || !matches!(
            alignment.method.as_str(),
            "asr_word_timing" | "forced_alignment"
        )
        || alignment.words.is_empty()
        || alignment.words.len() > 10_000
    {
        return None;
    }
    let mut text_end = 0usize;
    let mut last_start = segment.start_ms;
    let mut last_end = segment.start_ms;
    let untimed_separator = |text: &str| {
        text.chars().all(|ch| {
            ch.is_whitespace()
                || ch.is_ascii_punctuation()
                || matches!(
                    ch,
                    '，' | '。'
                        | '、'
                        | '；'
                        | '：'
                        | '！'
                        | '？'
                        | '（'
                        | '）'
                        | '【'
                        | '】'
                        | '《'
                        | '》'
                        | '〈'
                        | '〉'
                        | '“'
                        | '”'
                        | '‘'
                        | '’'
                        | '…'
                        | '—'
                        | '·'
                )
        })
    };
    for word in &alignment.words {
        if word.start_byte < text_end
            || word.end_byte <= word.start_byte
            || !segment
                .text
                .get(text_end..word.start_byte)
                .is_some_and(untimed_separator)
            || segment.text.get(word.start_byte..word.end_byte).is_none()
            || word.start_ms < last_start
            || word.end_ms < last_end
            || word.start_ms < segment.start_ms
            || word.end_ms > segment.end_ms
            || word.end_ms <= word.start_ms
        {
            return None;
        }
        text_end = word.end_byte;
        last_start = word.start_ms;
        last_end = word.end_ms;
    }
    segment
        .text
        .get(text_end..)
        .filter(|text| untimed_separator(text))
        .map(|_| alignment)
}

/// Match without normalization, punctuation guessing, or proportional timing.
/// Any text edit, mismatch, malformed timestamp, or missing run rejects the
/// mapping instead of projecting old token times onto different text.
pub fn exact_token_starts(
    segment: &Segment,
    run: Option<&TranscriptionRun>,
) -> Option<ExactTokenStarts> {
    use sha2::{Digest, Sha256};
    let run = run?;
    if !segment.is_confirmed() || run.run_id.is_empty() {
        return None;
    }
    let evidence = segment.extra.get("raw_token_timing")?;
    if evidence.get("timing_method")?.as_str()? != "sherpa_backend_token_start"
        || evidence.get("source_run_id")?.as_str()? != run.run_id
        || evidence.get("source_segment_id")?.as_u64()? != segment.id
        || evidence.get("source_segment_start_ms")?.as_u64()? != segment.start_ms
        || evidence.get("source_segment_end_ms")?.as_u64()? != segment.end_ms
        || evidence.get("recognizer_segment_revision")?.as_u64()? != segment.revision
        || evidence.get("raw_text")?.as_str()? != segment.text
    {
        return None;
    }
    let hash = format!("{:x}", Sha256::digest(segment.text.as_bytes()));
    if evidence.get("raw_text_sha256")?.as_str()? != hash {
        return None;
    }
    let raw = evidence.get("tokens")?.as_array()?;
    if raw.is_empty() || raw.len() > 10_000 || segment.text.len() > 100_000 {
        return None;
    }
    let mut cursor = 0usize;
    let mut previous = segment.start_ms;
    let mut tokens = Vec::with_capacity(raw.len());
    for token in raw {
        let text = token.get("token")?.as_str()?;
        let start_ms = token.get("start_ms")?.as_u64()?;
        if text.is_empty()
            || start_ms < previous
            || start_ms < segment.start_ms
            || start_ms >= segment.end_ms
        {
            return None;
        }
        let end = cursor.checked_add(text.len())?;
        if segment.text.get(cursor..end) != Some(text) {
            return None;
        }
        tokens.push(MappedTokenStart {
            text_range: cursor..end,
            start_ms,
        });
        cursor = end;
        previous = start_ms;
    }
    if cursor != segment.text.len() {
        return None;
    }
    Some(ExactTokenStarts {
        source_run_id: run.run_id.clone(),
        model_id: run.model_id.clone(),
        text_revision: segment.revision,
        text_sha256: hash,
        tokens,
    })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerTurn {
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
}

/// A sorted overlap index reused while rendering or exporting many segments.
pub struct SpeakerTimeline<'a> {
    transcript: &'a Transcript,
    turns: Vec<&'a SpeakerTurn>,
    prefix_max_end: Vec<u64>,
}
impl<'a> SpeakerTimeline<'a> {
    pub fn new(transcript: &'a Transcript) -> Self {
        let mut turns: Vec<_> = transcript
            .speaker_turns
            .iter()
            .filter(|turn| turn.start_ms < turn.end_ms)
            .collect();
        turns.sort_by_key(|turn| (turn.start_ms, turn.end_ms, turn.speaker));
        let mut end = 0;
        let prefix_max_end = turns
            .iter()
            .map(|turn| {
                end = end.max(turn.end_ms);
                end
            })
            .collect();
        Self {
            transcript,
            turns,
            prefix_max_end,
        }
    }
    pub fn label(&self, start: u64, end: u64) -> String {
        speaker_label_for(self.transcript, self.overlapping(start, end), start, end)
    }
    pub(crate) fn overlapping(
        &self,
        start: u64,
        end: u64,
    ) -> impl Iterator<Item = &'a SpeakerTurn> + '_ {
        let upper = self.turns.partition_point(|turn| turn.start_ms < end);
        let lower = self.prefix_max_end[..upper].partition_point(|latest| *latest <= start);
        self.turns[lower..upper].iter().copied()
    }
}

fn speaker_label_for<'a>(
    transcript: &Transcript,
    turns: impl Iterator<Item = &'a SpeakerTurn>,
    start: u64,
    end: u64,
) -> String {
    if transcript.speaker_turns.is_empty() {
        return String::new();
    }
    if end <= start {
        return "说话人未确定".into();
    }
    let mut overlaps = std::collections::BTreeMap::<u32, bool>::new();
    let mut covered = Vec::new();
    for turn in turns {
        let left = start.max(turn.start_ms);
        let right = end.min(turn.end_ms);
        if left < right {
            let tentative = turn
                .extra
                .get("identity_state")
                .and_then(|value| value.as_str())
                == Some("tentative");
            overlaps
                .entry(turn.speaker)
                .and_modify(|value| *value |= tentative)
                .or_insert(tentative);
            covered.push((left, right));
        }
    }
    if overlaps.is_empty() {
        return "说话人未确定".into();
    }
    let labels: Vec<_> = overlaps
        .into_iter()
        .map(|(id, tentative)| {
            let name = transcript.speaker_name(id);
            if tentative {
                format!("{name}（暂定）")
            } else {
                name
            }
        })
        .collect();
    if labels.len() > 1 {
        return format!("{}（多人／待核对）", labels.join(" / "));
    }
    covered.sort_unstable();
    let mut cursor = start;
    let mut total = 0u64;
    for (left, right) in covered {
        let left = left.max(cursor);
        if right > left {
            total = total.saturating_add(right - left);
            cursor = right;
        }
    }
    if total < end - start {
        format!("{} / 未确定", labels[0])
    } else {
        labels[0].clone()
    }
}
impl Transcript {
    /// Stored completion flags alone are insufficient for older or damaged
    /// checkpoints that still contain gaps or unstable segments.
    pub fn is_complete(&self) -> bool {
        self.complete
            && !self.cancelled
            && self.gaps.is_empty()
            && self
                .segments
                .iter()
                .all(|segment| segment.is_confirmed() && segment.end_ms >= segment.start_ms)
    }
    /// Freeze unfinished hypotheses after the owning task has terminated.
    /// They remain visible recovery data, not confirmed transcript sources.
    /// This is idempotent so a repeated terminal notification cannot keep
    /// changing the document revision.
    pub fn mark_terminal_unconfirmed(&mut self) -> u64 {
        let mut count = 0u64;
        for segment in &mut self.segments {
            let already_marked = segment
                .extra
                .get("terminal_unconfirmed")
                .and_then(|value| value.as_bool())
                == Some(true);
            let pending_review = segment
                .review
                .as_ref()
                .is_some_and(|review| review.state == ReviewState::Pending);
            if !segment.finalized || segment.correcting || pending_review || already_marked {
                if !already_marked || segment.finalized || segment.correcting || pending_review {
                    segment.revision = segment.revision.saturating_add(1);
                }
                segment.finalized = false;
                segment.correcting = false;
                segment
                    .extra
                    .insert("terminal_unconfirmed".into(), true.into());
                if let Some(review) = &mut segment.review
                    && review.state == ReviewState::Pending
                {
                    review.state = ReviewState::Deferred;
                    review.reason = Some("recognition_ended_before_review".into());
                }
                count = count.saturating_add(1);
            }
        }
        if count > 0
            && let Some(run) = &mut self.run
        {
            run.extra
                .insert("terminal_unconfirmed_segments".into(), count.into());
        }
        count
    }
    /// Explicitly choose a completed sentence's streaming or second-pass
    /// result. Never let this replace an in-flight or AI-corrected document.
    pub fn select_sentence_review(&mut self, id: u64, original: bool) -> bool {
        if self.correction.is_some() {
            return false;
        }
        let Some(segment) = self.segments.iter_mut().find(|segment| segment.id == id) else {
            return false;
        };
        if !segment.is_confirmed() {
            return false;
        }
        let Some(review) = segment.review.as_mut() else {
            return false;
        };
        if !matches!(review.state, ReviewState::Reviewed | ReviewState::Restored) {
            return false;
        }
        let chosen = if original {
            Some(&review.original_text)
        } else {
            review.revised_text.as_ref()
        };
        let Some(chosen) = chosen else {
            return false;
        };
        if *chosen == segment.text {
            return false;
        }
        segment.text.clone_from(chosen);
        segment.revision = segment.revision.saturating_add(1);
        review.state = if original {
            ReviewState::Restored
        } else {
            ReviewState::Reviewed
        };
        true
    }

    pub fn speaker_name(&self, id: u32) -> String {
        self.speaker_names
            .get(&id)
            .map(|n| {
                if n.matched {
                    format!("{}（声纹匹配待核对）", n.name)
                } else {
                    n.name.clone()
                }
            })
            .unwrap_or_else(|| format!("说话人 {id}"))
    }
    pub fn speaker_count(&self) -> usize {
        self.speaker_turns
            .iter()
            .map(|s| s.speaker)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }
    pub fn title(&self) -> String {
        self.extra
            .get("document_title")
            .and_then(|value| value.as_str())
            .filter(|title| !title.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                self.source
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "会议录音".into())
    }
    /// Human-readable output, with explicit provenance for optional AI proofreading.
    pub fn document_markdown(&self) -> String {
        let speakers = self.speaker_timeline();
        let mut text = format!("# {} · 逐字稿\n\n", escape_markdown(&self.title()));
        if self.correction.is_some() {
            text.push_str(
                "本稿经过 AI 全文语义校正，原始识别文字保留，可在应用内恢复。请核对重要内容。\n\n",
            );
        } else if self.is_complete() {
            text.push_str("本稿为语音识别原文，未经过 AI 改写。\n\n");
        }
        if !self.is_complete() {
            text.push_str("转录未完成；已确认内容与未定稿暂存文字分别列出。\n\n");
        }
        if self.speaker_turns.is_empty() {
            text.push_str("说话人：尚未分析；可在应用内运行本地说话人分析。\n\n");
        }
        for s in self.segments.iter().filter(|s| s.is_confirmed()) {
            let label = speakers.label(s.start_ms, s.end_ms);
            let label = if label.is_empty() {
                "说话人未分析"
            } else {
                &label
            };
            let time = if s.end_ms > s.start_ms {
                timestamp(s.start_ms, '.')
            } else {
                "时间不可用".into()
            };
            text.push_str(&format!(
                "**{} · {} · 段{}**\n\n{}\n\n",
                escape_markdown(label),
                time,
                s.id,
                escape_markdown(&s.text)
            ));
        }
        let unconfirmed: Vec<_> = self
            .segments
            .iter()
            .filter(|s| {
                !s.is_confirmed()
                    && s.extra
                        .get("terminal_unconfirmed")
                        .and_then(|value| value.as_bool())
                        == Some(true)
            })
            .collect();
        if !unconfirmed.is_empty() {
            text.push_str("## 未定稿暂存文字\n\n以下为识别中断时保留的临时结果，未经确认，不作为纪要依据。\n\n");
            for s in unconfirmed {
                text.push_str(&format!(
                    "**段{} · {} · 未定稿**\n\n{}\n\n",
                    s.id,
                    timestamp(s.start_ms, '.'),
                    escape_markdown(&s.text)
                ));
            }
        }
        text.push_str(&self.speaker_index_markdown());
        text
    }
    pub fn export_word(&self, base: &Path) -> Result<()> {
        crate::export_minutes_docx(
            &self.document_markdown(),
            &base.with_extension("transcript.docx"),
        )
    }
    pub fn speaker_index_markdown(&self) -> String {
        let mut people = std::collections::BTreeMap::<u32, (u64, u64)>::new();
        for t in &self.speaker_turns {
            let entry = people.entry(t.speaker).or_insert((t.start_ms, 0));
            entry.0 = entry.0.min(t.start_ms);
            entry.1 += t.end_ms.saturating_sub(t.start_ms);
        }
        if people.is_empty() {
            return String::new();
        }
        let mut md = String::from(
            "\n\n## 说话人索引（本地分析）\n\n未命名编号仅用于本场会议；姓名来自用户命名或本地声纹匹配，需核对。交替或重叠句子的归属需结合逐字稿核实。\n\n| 说话人 | 首次发言 | 累计发言 |\n| --- | --- | --- |\n",
        );
        for (speaker, (start, duration)) in people {
            md.push_str(&format!(
                "| {} | {} | {:.1} 秒 |\n",
                escape_markdown(&self.speaker_name(speaker)).replace('|', "\\|"),
                timestamp(start, '.'),
                duration as f64 / 1000.
            ));
        }
        md
    }
    /// Utterances can span turns. Without word timestamps we report ambiguity
    /// instead of silently assigning the entire sentence to one person.
    pub fn speaker_label(&self, start: u64, end: u64) -> String {
        speaker_label_for(self, self.speaker_turns.iter(), start, end)
    }
    pub fn speaker_timeline(&self) -> SpeakerTimeline<'_> {
        SpeakerTimeline::new(self)
    }
    /// A hypothesis is a replacement for one utterance, never an appended delta.
    /// Late updates cannot roll back a final utterance or a newer revision.
    pub fn apply(&mut self, mut segment: Segment) -> bool {
        if let Some(old) = self.segments.iter_mut().find(|s| s.id == segment.id) {
            if old.finalized || old.revision >= segment.revision {
                return false;
            }
            for (key, value) in &old.extra {
                segment
                    .extra
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
            if let (Some(previous), Some(next)) = (&old.review, &mut segment.review) {
                for (key, value) in &previous.extra {
                    next.extra
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
            *old = segment;
        } else {
            let index = self.segments.partition_point(|s| s.id < segment.id);
            self.segments.insert(index, segment);
        }
        true
    }
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .filter(|s| s.is_confirmed())
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Lightweight recovery data. Full document generation belongs at completion/export.
    pub fn checkpoint(&self, base: &Path) -> Result<()> {
        let _meeting_activity = opencap_core::managed_meeting_activity(base)?;
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(
            &base.with_extension("transcript.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn save(&self, base: &Path) -> Result<()> {
        let warnings = self.write_outputs(base);
        anyhow::ensure!(warnings.is_empty(), "{}", warnings.join("；"));
        Ok(())
    }
    /// Preserve completed analysis even when an external editor locks a derived file.
    /// Callers show these warnings and offer retry without rerunning inference.
    pub fn sync_outputs(&mut self, base: &Path) {
        // Export retries alone do not repair archive or identity registration.
        let persistent_warnings: Vec<_> = self
            .output_warnings
            .iter()
            .filter(|warning| {
                warning.starts_with("识别历史版本未归档：")
                    || warning.starts_with("会议身份记录失败：")
                    || warning.starts_with("媒体资产指纹登记失败：")
            })
            .cloned()
            .collect();
        self.primary_save_pending = false;
        self.output_warnings = self.write_outputs(base);
        self.output_warnings.extend(persistent_warnings);
        // The final checkpoint below is authoritative for the primary JSON.
        // A failure of the preliminary checkpoint no longer describes the
        // final state if that retry succeeds.
        self.output_warnings
            .retain(|warning| !warning.starts_with("转写数据未同步："));
        if let Err(error) = self.checkpoint(base) {
            self.primary_save_pending = true;
            let message = format!("转写数据未同步：{error:#}");
            if !self.output_warnings.contains(&message) {
                self.output_warnings.push(message);
            }
        }
    }
    /// Synchronize a derived edit only if the current primary JSON still
    /// matches the caller's saved baseline. The lock covers check and write;
    /// non-cooperating external writers can still race the final replacement.
    pub fn sync_outputs_guarded(&mut self, base: &Path, expected_sha256: &str) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let _guard = opencap_core::meeting_metadata_guard(base)?;
        if let Some(manifest) = opencap_core::meeting_manifest(base)? {
            anyhow::ensure!(
                self.run.as_ref().and_then(|run| run.meeting_id.as_deref())
                    == Some(manifest.meeting_id.as_str()),
                "逐字稿与会议清单身份不一致或未记录"
            );
        }
        let path = base.with_extension("transcript.json");
        let before = std::fs::symlink_metadata(&path).context("无法读取逐字稿基线")?;
        anyhow::ensure!(
            before.is_file()
                && !before.file_type().is_symlink()
                && before.len() <= 64 * 1024 * 1024,
            "逐字稿基线不是普通文件或超过 64 MiB"
        );
        let mut file = opencap_core::open_regular_file_for_read(&path)?;
        let identity = same_file::Handle::from_file(file.try_clone()?)?;
        let opened = file.metadata()?;
        anyhow::ensure!(
            opened.len() == before.len()
                && opened.modified()? == before.modified()?
                && same_file::Handle::from_file(opencap_core::open_regular_file_for_read(&path)?)?
                    == identity,
            "逐字稿在核对前发生变化"
        );
        let mut digest = Sha256::new();
        let mut total = 0u64;
        let mut buffer = [0u8; 128 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            anyhow::ensure!(total <= 64 * 1024 * 1024, "逐字稿基线超过 64 MiB");
            digest.update(&buffer[..read]);
        }
        let after = file.metadata()?;
        let current = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            current.is_file()
                && !current.file_type().is_symlink()
                && opened.len() == after.len()
                && after.len() == current.len()
                && total == after.len()
                && opened.modified()? == after.modified()?
                && after.modified()? == current.modified()?
                && same_file::Handle::from_file(opencap_core::open_regular_file_for_read(&path)?)?
                    == identity,
            "逐字稿在核对期间发生变化"
        );
        anyhow::ensure!(
            format!("{:x}", digest.finalize()) == expected_sha256,
            "磁盘逐字稿自任务开始后已变化"
        );
        self.sync_outputs(base);
        Ok(())
    }
    fn write_outputs(&self, base: &Path) -> Vec<String> {
        let _meeting_activity = match opencap_core::managed_meeting_activity(base) {
            Ok(guard) => guard,
            Err(error) => return vec![format!("会议目录不可写：{error:#}")],
        };
        let mut warnings = Vec::new();
        if let Err(error) = self.checkpoint(base) {
            warnings.push(format!("转写数据未同步：{error:#}"));
        }
        let mut md = format!("# {} · 逐字稿\n\n", escape_markdown(&self.title()));
        if self.correction.is_some() {
            md.push_str("AI 全文语义校正稿；原始识别文字保留，可在应用内恢复。\n\n");
        }
        if !self.is_complete() {
            md.push_str("转录尚未完整完成；原媒体保留，可重新转录。\n\n");
        }
        let mut srt = String::new();
        let mut subtitle_number = 0usize;
        let speakers = self.speaker_timeline();
        for s in self.segments.iter().filter(|s| s.is_confirmed()) {
            let speaker = speakers.label(s.start_ms, s.end_ms);
            let time = if s.end_ms > s.start_ms {
                format!(
                    "{}–{}",
                    timestamp(s.start_ms, '.'),
                    timestamp(s.end_ms, '.')
                )
            } else {
                "时间不可用".into()
            };
            md.push_str(&format!("- [段{} {} {}] {}\n", s.id, time, speaker, s.text));
            if s.end_ms > s.start_ms {
                subtitle_number += 1;
                srt.push_str(&format!(
                    "{}\n{} --> {}\n{}\n\n",
                    subtitle_number,
                    timestamp(s.start_ms, ','),
                    timestamp(s.end_ms, ','),
                    format!("{} {}", speaker, s.text).trim()
                ));
            }
        }
        let mut plain = format!("{} · 逐字稿\n\n", self.title());
        if self.correction.is_some() {
            plain.push_str("AI 全文语义校正稿；原始识别文字保留，可在应用内恢复。\n\n");
        }
        if !self.is_complete() {
            plain.push_str("转录尚未完整完成；原媒体保留，可重新转录。\n\n");
        }
        for s in self.segments.iter().filter(|s| s.is_confirmed()) {
            let speaker = speakers.label(s.start_ms, s.end_ms);
            plain.push_str(&format!(
                "{} {}\n{}\n\n",
                if speaker.is_empty() {
                    "说话人未分析"
                } else {
                    &speaker
                },
                if s.end_ms > s.start_ms {
                    timestamp(s.start_ms, '.')
                } else {
                    "时间不可用".into()
                },
                s.text
            ));
        }
        let unconfirmed: Vec<_> = self
            .segments
            .iter()
            .filter(|s| {
                !s.is_confirmed()
                    && s.extra
                        .get("terminal_unconfirmed")
                        .and_then(|value| value.as_bool())
                        == Some(true)
            })
            .collect();
        if !unconfirmed.is_empty() {
            md.push_str("\n## 未定稿暂存文字\n\n以下为识别中断时保留的临时结果，未经确认，不作为纪要依据。\n\n");
            plain.push_str("未定稿暂存文字（识别中断时保留，未经确认，不作为纪要依据）\n\n");
            for s in unconfirmed {
                md.push_str(&format!(
                    "- 段{} {} · 未定稿：{}\n",
                    s.id,
                    timestamp(s.start_ms, '.'),
                    escape_markdown(&s.text)
                ));
                plain.push_str(&format!(
                    "段{} {} · 未定稿\n{}\n\n",
                    s.id,
                    timestamp(s.start_ms, '.'),
                    s.text
                ));
            }
        }
        for (extension, bytes) in [
            ("transcript.md", md.as_bytes()),
            ("transcript.txt", plain.as_bytes()),
            ("srt", srt.as_bytes()),
        ] {
            if let Err(error) = atomic_write(&base.with_extension(extension), bytes) {
                warnings.push(format!(
                    "{extension} 未同步（可能正被其他应用占用）：{error:#}"
                ));
            }
        }
        if self.is_complete()
            && let Err(error) = self.export_word(base)
        {
            warnings.push(format!("Word 未同步（可能正被其他应用占用）：{error:#}"));
        }
        warnings
    }
}
pub fn escape_markdown(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if "\\`*_{}[]<>#+-.!|".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".opencap-write-{}.tmp",
        opencap_core::new_stable_id()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // Never delete the previous artifact before replacement succeeds.
        std::fs::rename(&temporary, path)?;
        opencap_core::sync_parent_directory(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Keep a byte-for-byte copy before a backfill replaces the active transcript.
/// The immutable copy also retains fields unknown to the current application.
pub fn archive_transcript_before_backfill(base: &Path) -> Result<Option<PathBuf>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    const LIMIT: u64 = 64 * 1024 * 1024;
    let _meeting_activity = opencap_core::managed_meeting_activity(base)?;
    let source = base.with_extension("transcript.json");
    match std::fs::metadata(&source) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut file = opencap_core::open_regular_file_for_read(&source)?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file() && before.len() <= LIMIT,
        "原稿不是普通文件或超过 64 MiB"
    );
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "原稿超过 64 MiB");
    let after = file.metadata()?;
    let current = std::fs::metadata(&source)?;
    anyhow::ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && after.len() == current.len()
            && after.modified()? == current.modified()?,
        "原稿归档期间发生变化，请重试"
    );
    anyhow::ensure!(
        identity
            == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(&source)?)?,
        "原稿归档期间被替换，请重试"
    );
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let archive = base.with_extension(format!("transcript.run-backfill-{digest}.json"));
    if let Err(error) = opencap_core::atomic_create_bytes(&archive, &bytes) {
        let identical = (|| -> Result<bool> {
            let before = std::fs::symlink_metadata(&archive)?;
            anyhow::ensure!(
                before.is_file()
                    && !before.file_type().is_symlink()
                    && before.len() == bytes.len() as u64,
                "归档不是匹配的普通文件"
            );
            let observed = opencap_core::fingerprint_media(&archive, None)?;
            let after = std::fs::symlink_metadata(&archive)?;
            anyhow::ensure!(
                after.is_file()
                    && !after.file_type().is_symlink()
                    && before.len() == after.len()
                    && before.modified()? == after.modified()?,
                "归档在核对期间发生变化"
            );
            Ok(observed.size_bytes == bytes.len() as u64 && observed.sha256 == digest)
        })()
        .unwrap_or(false);
        if !identical {
            return Err(error.context("补转归档未创建，已有文件保留"));
        }
    }
    if let Some(name) = archive.file_name() {
        opencap_core::register_meeting_asset(
            base,
            "transcript_history",
            opencap_core::AssetOwnership::Managed,
            Path::new(name),
        )?;
    }
    Ok(Some(archive))
}

/// Publish an immutable recognition snapshot without replacing an existing
/// version. The active transcript remains authoritative if archiving fails.
pub fn archive_completed_run(base: &Path, transcript: &Transcript) -> Result<Option<PathBuf>> {
    let Some(run) = &transcript.run else {
        return Ok(None);
    };
    anyhow::ensure!(
        !run.run_id.is_empty()
            && run.run_id.len() <= 64
            && run
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "识别版本标识无效"
    );
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let destination = base.with_extension(format!("transcript.run-{}.json", run.run_id));
    let bytes = serde_json::to_vec_pretty(transcript)?;
    if let Err(error) = opencap_core::atomic_create_bytes(&destination, &bytes) {
        // A previous attempt may have published the complete snapshot but
        // failed to register it. Only reuse an exact, ordinary file; never
        // overwrite or adopt a conflicting version under the same run ID.
        let identical = (|| -> Result<bool> {
            use sha2::{Digest, Sha256};
            let before = std::fs::symlink_metadata(&destination)?;
            anyhow::ensure!(
                before.is_file()
                    && !before.file_type().is_symlink()
                    && before.len() == bytes.len() as u64,
                "识别历史不是匹配的普通文件"
            );
            let observed = opencap_core::fingerprint_media(&destination, None)?;
            let after = std::fs::symlink_metadata(&destination)?;
            anyhow::ensure!(
                after.is_file()
                    && !after.file_type().is_symlink()
                    && before.len() == after.len()
                    && before.modified()? == after.modified()?,
                "识别历史在核对期间发生变化"
            );
            Ok(observed.size_bytes == bytes.len() as u64
                && observed.sha256 == format!("{:x}", Sha256::digest(&bytes)))
        })()
        .unwrap_or(false);
        if !identical {
            return Err(error.context("识别历史版本未发布，已有文件保留"));
        }
    }
    if let Some(name) = destination.file_name() {
        opencap_core::register_meeting_asset(
            base,
            "transcript_history",
            opencap_core::AssetOwnership::Managed,
            Path::new(name),
        )?;
    }
    Ok(Some(destination))
}

/// Save the current draft as a separately identified recovery copy. It is not
/// an original recognition snapshot and never replaces an existing history.
pub fn archive_current_recovery(base: &Path, transcript: &Transcript) -> Result<PathBuf> {
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let mut recovery = transcript.clone();
    recovery.extra.insert(
        "history_snapshot_kind".into(),
        "manual_recovery_current_draft".into(),
    );
    let destination = base.with_extension(format!(
        "transcript.run-recovery-{}.json",
        opencap_core::new_stable_id()
    ));
    opencap_core::atomic_create_bytes(&destination, &serde_json::to_vec_pretty(&recovery)?)
        .map_err(|error| {
            anyhow::anyhow!(
                "恢复副本写入未完成，目标 {}：{error:#}",
                destination.display()
            )
        })?;
    if let Some(name) = destination.file_name() {
        opencap_core::register_meeting_asset(
            base,
            "transcript_history",
            opencap_core::AssetOwnership::Managed,
            Path::new(name),
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "恢复副本已写入 {}，但登记失败：{error:#}",
                destination.display()
            )
        })?;
    }
    Ok(destination)
}

/// Repair a failed archive only when the bytes match the digest captured
/// before the original publication attempt; never replace an existing file.
pub fn repair_archive_registration(base: &Path, transcript: &Transcript) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let run = transcript
        .run
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("没有可修复的识别运行"))?;
    anyhow::ensure!(
        !run.run_id.is_empty()
            && run.run_id.len() <= 64
            && run
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "识别版本标识无效"
    );
    let expected = run
        .extra
        .get("pending_archive_sha256")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("未记录原始归档指纹，不能自动登记"))?;
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let path = base.with_extension(format!("transcript.run-{}.json", run.run_id));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The original archive was serialized before these failure
            // annotations were added. A later edit makes the digest differ,
            // so it cannot be mistaken for the immutable original snapshot.
            let mut original = transcript.clone();
            if let Some(run) = &mut original.run {
                run.extra.remove("pending_archive_sha256");
            }
            original.output_warnings.retain(|warning| {
                ![
                    "识别历史版本未归档：",
                    "会议身份记录失败：",
                    "最终运行状态未保存：",
                ]
                .iter()
                .any(|prefix| warning.starts_with(*prefix))
            });
            let bytes = serde_json::to_vec_pretty(&original)?;
            anyhow::ensure!(
                bytes.len() <= 64 * 1024 * 1024
                    && format!("{:x}", Sha256::digest(&bytes)) == expected,
                "原始归档缺失，当前稿与待发布快照不一致；请另存恢复副本"
            );
            opencap_core::atomic_create_bytes(&path, &bytes).map_err(|error| {
                anyhow::anyhow!("无法重新发布原始归档，已有文件保留：{error:#}")
            })?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 64 * 1024 * 1024,
        "历史文件不是普通文件或超过 64 MiB"
    );
    let mut bytes = Vec::new();
    let mut file = std::fs::File::open(&path)?;
    let opened = file.metadata()?;
    (&mut file)
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        current.is_file()
            && !current.file_type().is_symlink()
            && metadata.len() == opened.len()
            && opened.len() == after.len()
            && after.len() == current.len()
            && bytes.len() as u64 == after.len()
            && metadata.modified()? == opened.modified()?
            && opened.modified()? == after.modified()?
            && after.modified()? == current.modified()?,
        "历史文件在核对期间发生变化；未登记"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            (metadata.dev(), metadata.ino()) == (opened.dev(), opened.ino())
                && (after.dev(), after.ino()) == (current.dev(), current.ino()),
            "历史文件在核对期间被替换；未登记"
        );
    }
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected,
        "历史文件与预期指纹不一致；原文件保留"
    );
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("历史文件路径无效"))?;
    opencap_core::register_meeting_asset(
        base,
        "transcript_history",
        opencap_core::AssetOwnership::Managed,
        Path::new(name),
    )?;
    Ok(path)
}
pub fn timestamp(ms: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_blocked_word_export_keeps_analysis_and_syncs_other_outputs_then_retries() {
        let root =
            std::env::temp_dir().join(format!("opencap-speaker-sync-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let base = root.join("会议");
        let word = base.with_extension("transcript.docx");
        std::fs::create_dir_all(&word).unwrap(); // Inject a derived-file write failure.
        let mut transcript = Transcript {
            complete: true,
            segments: vec![s(1, "预算不能增加", true)],
            speaker_turns: vec![SpeakerTurn {
                extra: Default::default(),
                start_ms: 0,
                end_ms: 5000,
                speaker: 1,
            }],
            ..Default::default()
        };
        transcript.speaker_names.insert(
            1,
            crate::SpeakerName {
                extra: Default::default(),
                name: "张三".into(),
                matched: false,
                profile_id: None,
            },
        );
        transcript.sync_outputs(&base);
        assert_eq!(transcript.speaker_count(), 1);
        assert!(
            transcript
                .output_warnings
                .iter()
                .any(|w| w.contains("Word"))
        );
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert_eq!(saved.speaker_count(), 1);
        assert_eq!(saved.speaker_name(1), "张三");
        assert!(!saved.output_warnings.is_empty());
        for ext in ["transcript.txt", "transcript.md", "srt"] {
            let text = std::fs::read_to_string(base.with_extension(ext)).unwrap();
            assert!(
                text.contains("张三") && text.contains("预算不能增加"),
                "{ext}: {text}"
            );
        }
        std::fs::remove_dir(&word).unwrap();
        transcript.sync_outputs(&base);
        assert!(transcript.output_warnings.is_empty());
        assert!(word.is_file());
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert!(saved.output_warnings.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn speaker_count_counts_groups_instead_of_the_largest_label() {
        let transcript = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 1,
                },
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: 3,
                },
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 2000,
                    end_ms: 3000,
                    speaker: 3,
                },
            ],
            ..Default::default()
        };
        assert_eq!(transcript.speaker_count(), 2);
    }
    fn s(revision: u64, text: &str, finalized: bool) -> Segment {
        Segment {
            extra: Default::default(),
            id: 1,
            revision,
            start_ms: 1200,
            end_ms: 2400,
            text: text.into(),
            finalized,
            correcting: false,
            review: None,
        }
    }
    #[test]
    fn context_replaces_wrong_character_without_duplicates() {
        let mut t = Transcript::default();
        assert!(t.apply(s(1, "工", false)));
        assert!(t.apply(s(2, "公司", false)));
        assert!(t.apply(s(3, "公司下周开会", true)));
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.text(), "公司下周开会");
        assert!(!t.apply(s(2, "公", false)));
        assert!(!t.apply(s(4, "错误迟到结果", false)));
    }
    #[test]
    fn sentence_review_versions_roundtrip_and_late_updates_cannot_override_selection() {
        let mut segment = s(4, "公司下周录屏", true);
        segment.review = Some(SentenceReview {
            extra: Default::default(),
            state: ReviewState::Reviewed,
            original_text: "公司下周陆平".into(),
            revised_text: Some("公司下周录屏".into()),
            model: "SenseVoice".into(),
            reason: None,
            audio_ms: Some(1400),
            queue_ms: Some(20),
            processing_ms: Some(100),
        });
        let mut transcript = Transcript::default();
        assert!(transcript.apply(segment));
        assert!(transcript.select_sentence_review(1, true));
        assert_eq!(transcript.text(), "公司下周陆平");
        assert!(!transcript.apply(s(100, "迟到结果", true)));
        let saved = serde_json::to_string(&transcript).unwrap();
        let mut reopened: Transcript = serde_json::from_str(&saved).unwrap();
        assert!(reopened.select_sentence_review(1, false));
        assert_eq!(reopened.text(), "公司下周录屏");
        assert_eq!(
            reopened.segments[0].review.as_ref().unwrap().state,
            ReviewState::Reviewed
        );
    }
    #[test]
    fn unconfirmed_text_cannot_enter_minutes() {
        let mut t = Transcript::default();
        t.apply(s(1, "可能上线", false));
        assert!(t.text().is_empty());
        assert_eq!(timestamp(3_661_007, ','), "01:01:01,007");
    }
    #[test]
    fn existing_transcripts_load_without_a_correction_state() {
        let legacy = r#"{"id":1,"revision":2,"start_ms":0,"end_ms":1000,"text":"历史原文","finalized":true}"#;
        let segment: Segment = serde_json::from_str(legacy).unwrap();
        assert!(segment.finalized && !segment.correcting);
        let mut transcript = Transcript::default();
        transcript.apply(segment);
        assert_eq!(transcript.text(), "历史原文");
    }
    #[test]
    fn brief_interruption_is_marked_ambiguous_inside_a_long_sentence() {
        let t = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 0,
                    end_ms: 4000,
                    speaker: 1,
                },
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 4000,
                    end_ms: 4300,
                    speaker: 2,
                },
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 4300,
                    end_ms: 10000,
                    speaker: 1,
                },
            ],
            ..Default::default()
        };
        // Overlap is reported as multiple candidates, not as a single owner.
        assert_eq!(
            t.speaker_label(0, 10000),
            "说话人 1 / 说话人 2（多人／待核对）"
        );
    }
    #[test]
    fn completed_transcript_exports_separate_word_and_plain_text_without_ai() {
        let dir = std::env::temp_dir().join(format!(
            "opencap-transcript-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = dir.join("meeting");
        let mut transcript = Transcript::default();
        transcript.apply(s(1, "不能上线，预算 2 万元。", true));
        transcript.checkpoint(&base).unwrap();
        let restored: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert_eq!(restored.text(), transcript.text());
        assert!(!restored.complete);
        for extension in ["transcript.md", "transcript.txt", "srt", "transcript.docx"] {
            assert!(!base.with_extension(extension).exists());
        }
        transcript.save(&base).unwrap();
        assert!(!base.with_extension("transcript.docx").exists());
        transcript.complete = true;
        transcript.save(&base).unwrap();
        assert!(
            std::fs::read_to_string(base.with_extension("transcript.txt"))
                .unwrap()
                .contains("不能上线，预算 2 万元。")
        );
        assert!(
            std::fs::read(base.with_extension("transcript.docx"))
                .unwrap()
                .starts_with(b"PK")
        );
        assert!(!base.with_extension("summary.docx").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
