//! Optional sentence correction must never hold up the streaming decoder.
use crate::punctuation::Punctuator;
use crate::{AsrEvent, ReviewState, Segment, Transcript};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const MAX_REVISION_SAMPLES: usize = 30 * 16000;
const REVISION_QUEUE_CAPACITY: usize = 8;

struct Job {
    segment: Segment,
    audio: Vec<f32>,
    queued_at: Instant,
}

struct RevisionDecode {
    text: String,
    timing_evidence: Option<serde_json::Value>,
}

/// Keep the model's original token arrays relative to the actual second-pass
/// input. Its media origin is not currently tracked, so these values cannot
/// be used for seeking or word alignment.
fn revision_timing_evidence(
    result: &sherpa_onnx::OfflineRecognizerResult,
    samples: usize,
    model: crate::ModelKind,
    verified_model_hash: Option<&str>,
) -> Option<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let tokens = &result.tokens;
    if tokens.is_empty()
        || tokens.len() > 10_000
        || result.text.len() > 1_000_000
        || tokens.iter().any(|token| token.len() > 4096)
        || tokens
            .iter()
            .try_fold(0usize, |total, token| total.checked_add(token.len()))
            .is_none_or(|total| total > 1_000_000)
    {
        return None;
    }
    let valid = |values: &[f32]| {
        values.len() == tokens.len()
            && values
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
    };
    let starts = result.timestamps.as_deref().filter(|values| valid(values));
    let durations = result.durations.as_deref().filter(|values| valid(values));
    if starts.is_none() && durations.is_none() {
        return None;
    }
    let raw = result.text.trim();
    Some(serde_json::json!({
        "timing_precision": "token_model", "timing_method": "sherpa_offline_second_pass_raw",
        "model_id": model.directory(), "verified_model_manifest_sha256": verified_model_hash,
        "raw_text": raw, "raw_text_sha256": format!("{:x}", Sha256::digest(raw.as_bytes())),
        "input_samples": samples, "input_sample_rate_hz": 16000,
        "media_origin": null, "time_unit": "unverified", "display_mapping": null,
        "timestamp_status": if result.timestamps.is_none() { "not_returned" }
            else if starts.is_some() { "unverified_backend_values" } else { "invalid_or_count_mismatch" },
        "duration_status": if result.durations.is_none() { "not_returned" }
            else if durations.is_some() { "unverified_backend_values" } else { "invalid_or_count_mismatch" },
        "tokens": tokens.iter().enumerate().map(|(index, token)| serde_json::json!({
            "token": token, "backend_timestamp_raw": starts.map(|values| values[index]),
            "backend_duration_raw": durations.map(|values| values[index])
        })).collect::<Vec<_>>()
    }))
}

pub(crate) struct Reviser {
    requests: Option<Sender<Job>>,
    results: Receiver<Segment>,
    pending: BTreeMap<u64, Segment>,
    cancelled: Arc<AtomicBool>,
    model: String,
    punctuation: Option<Arc<Punctuator>>,
    health: Arc<AtomicU8>,
    failure: Arc<Mutex<Option<String>>>,
}

impl Reviser {
    pub fn new(
        kind: crate::ModelKind,
        dir: PathBuf,
        language: opencap_core::TranscriptionLanguage,
        cancelled: Arc<AtomicBool>,
        events: Sender<AsrEvent>,
        punctuation: Arc<Punctuator>,
        expected_fingerprint: Option<String>,
    ) -> anyhow::Result<Self> {
        let health = Arc::new(AtomicU8::new(0));
        let worker_health = health.clone();
        let failure = Arc::new(Mutex::new(None));
        let worker_failure = failure.clone();
        let _ = events.send(AsrEvent::RevisionStatus(
            crate::RevisionRuntimeState::Loading,
            None,
        ));
        let model_cancelled = cancelled.clone();
        let mut reviser = Self::spawn_with_evidence(cancelled, move || {
            let model = (|| -> anyhow::Result<_> {
                let check = || -> anyhow::Result<()> {
                    if let Some(expected) = &expected_fingerprint {
                        let root = dir
                            .parent()
                            .ok_or_else(|| anyhow::anyhow!("复核模型目录无效"))?;
                        let observed =
                            crate::models::fingerprint_model(kind, root, &model_cancelled, false)?;
                        anyhow::ensure!(
                            &observed.manifest_sha256 == expected,
                            "复核模型与历史指纹不一致"
                        );
                    }
                    Ok(())
                };
                check()?;
                let model = super::worker::offline_recognizer(kind, &dir, language)?;
                check()?;
                Ok(model)
            })();
            let state = if model.is_ok() {
                crate::RevisionRuntimeState::Ready
            } else {
                crate::RevisionRuntimeState::Failed
            };
            let reason = model.as_ref().err().map(|error| format!("{error:#}"));
            *worker_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = reason.clone();
            worker_health.store(if model.is_ok() { 1 } else { 2 }, Ordering::Release);
            let _ = events.send(AsrEvent::RevisionStatus(state, reason));
            let verified_model_hash = expected_fingerprint.clone();
            move |audio: &[f32]| {
                let model = model.as_ref().ok()?;
                let stream = model.create_stream();
                stream.accept_waveform(16000, audio);
                model.decode(&stream);
                stream.get_result().map(|result| RevisionDecode {
                    timing_evidence: revision_timing_evidence(
                        &result,
                        audio.len(),
                        kind,
                        verified_model_hash.as_deref(),
                    ),
                    text: result.text.trim().to_owned(),
                })
            }
        })?;
        reviser.model = kind.label().into();
        reviser.punctuation = Some(punctuation);
        reviser.health = health;
        reviser.failure = failure;
        Ok(reviser)
    }

    #[cfg(test)]
    fn spawn<F, D>(cancelled: Arc<AtomicBool>, create: F) -> anyhow::Result<Self>
    where
        F: FnOnce() -> D + Send + 'static,
        D: FnMut(&[f32]) -> Option<String>,
    {
        Self::spawn_with_evidence(cancelled, move || {
            let mut decode = create();
            move |audio: &[f32]| {
                decode(audio).map(|text| RevisionDecode {
                    text,
                    timing_evidence: None,
                })
            }
        })
    }

    fn spawn_with_evidence<F, D>(cancelled: Arc<AtomicBool>, create: F) -> anyhow::Result<Self>
    where
        F: FnOnce() -> D + Send + 'static,
        D: FnMut(&[f32]) -> Option<RevisionDecode>,
    {
        // Model initialization may take several seconds. Keep a small bounded
        // burst of completed sentences (at most ~15 MiB PCM) without delaying
        // live text; sustained overload still retains the original transcript.
        let (requests, rx) = bounded::<Job>(REVISION_QUEUE_CAPACITY);
        let (tx, results) = unbounded();
        let stopped = cancelled.clone();
        thread::Builder::new()
            .name("opencap-asr-revision".into())
            .spawn(move || {
                let mut decode = create();
                for mut job in rx {
                    let started = Instant::now();
                    let result = if stopped.load(Ordering::Relaxed) {
                        None
                    } else {
                        decode(&job.audio).filter(|result| !result.text.is_empty())
                    };
                    if let Some(review) = job.segment.review.as_mut() {
                        review.queue_ms =
                            Some(started.duration_since(job.queued_at).as_millis() as u64);
                        review.processing_ms = Some(started.elapsed().as_millis() as u64);
                        review.state = if result.is_some() {
                            ReviewState::Reviewed
                        } else {
                            ReviewState::Failed
                        };
                        review.revised_text = result.as_ref().map(|result| result.text.clone());
                        if let Some(evidence) = result
                            .as_ref()
                            .and_then(|result| result.timing_evidence.clone())
                        {
                            review
                                .extra
                                .insert("asr_timing_evidence_v1".into(), evidence);
                        }
                        if result.is_none() {
                            review.reason = Some(
                                if stopped.load(Ordering::Relaxed) {
                                    "cancelled"
                                } else {
                                    "empty_or_unavailable"
                                }
                                .into(),
                            );
                        }
                    }
                    if let Some(result) = result {
                        job.segment.text = result.text;
                    }
                    if tx.send(finalize(job.segment)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            results,
            pending: BTreeMap::new(),
            cancelled,
            model: "文件模型".into(),
            punctuation: None,
            health: Arc::new(AtomicU8::new(0)),
            failure: Arc::new(Mutex::new(None)),
        })
    }

    pub fn health(&self) -> crate::RevisionRuntimeState {
        match self.health.load(Ordering::Acquire) {
            1 => crate::RevisionRuntimeState::Ready,
            2 => crate::RevisionRuntimeState::Failed,
            _ => crate::RevisionRuntimeState::Loading,
        }
    }
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn submit(&mut self, segment: &Segment, audio: Vec<f32>) -> bool {
        let Some(requests) = &self.requests else {
            return false;
        };
        if requests
            .try_send(Job {
                segment: segment.clone(),
                audio,
                queued_at: Instant::now(),
            })
            .is_err()
        {
            return false;
        }
        self.pending.insert(segment.id, segment.clone());
        true
    }

    pub fn poll(&mut self, transcript: &mut Transcript, events: &Sender<AsrEvent>) -> bool {
        let mut changed = false;
        loop {
            match self.results.try_recv() {
                Ok(segment) => changed |= self.apply(segment, transcript, events),
                Err(TryRecvError::Disconnected) => {
                    self.health.store(2, Ordering::Release);
                    let mut failure = self
                        .failure
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if failure.is_none() {
                        *failure = Some("复核线程意外退出".into());
                    }
                    drop(failure);
                    changed |= self.fallback(transcript, events, "worker_disconnected");
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        changed
    }

    pub fn finish(&mut self, transcript: &mut Transcript, events: &Sender<AsrEvent>) {
        self.requests.take();
        // Never hold up saving indefinitely if an inference call hangs or a
        // slow machine has a backlog. Preserve streaming text for the rest.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.pending.is_empty()
            && !self.cancelled.load(Ordering::Relaxed)
            && Instant::now() < deadline
        {
            match self.results.recv_timeout(Duration::from_millis(100)) {
                Ok(segment) => {
                    self.apply(segment, transcript, events);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
        }
        self.fallback(transcript, events, "drain_timeout_or_disconnected");
    }

    fn apply(
        &mut self,
        mut segment: Segment,
        transcript: &mut Transcript,
        events: &Sender<AsrEvent>,
    ) -> bool {
        self.pending.remove(&segment.id);
        if let Some(punctuation) = &self.punctuation {
            let original = segment.text.clone();
            segment.text = punctuation.finish(&segment.text);
            if let Some(review) = segment.review.as_mut() {
                if review.revised_text.as_deref() == Some(original.as_str()) {
                    review.revised_text = Some(segment.text.clone());
                } else if review.original_text == original {
                    review.original_text = segment.text.clone();
                }
            }
        }
        if transcript.apply(segment.clone()) {
            let _ = events.send(AsrEvent::Segment(segment));
            true
        } else {
            false
        }
    }

    fn fallback(
        &mut self,
        transcript: &mut Transcript,
        events: &Sender<AsrEvent>,
        reason: &str,
    ) -> bool {
        let mut changed = false;
        for (_, mut segment) in std::mem::take(&mut self.pending) {
            if let Some(review) = segment.review.as_mut() {
                review.state = ReviewState::Deferred;
                review.reason = Some(
                    if self.cancelled.load(Ordering::Relaxed) {
                        "cancelled"
                    } else {
                        reason
                    }
                    .into(),
                );
            }
            changed |= self.apply(finalize(segment), transcript, events);
        }
        changed
    }
}

fn finalize(mut segment: Segment) -> Segment {
    segment.revision += 1;
    segment.finalized = true;
    segment.correcting = false;
    segment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SentenceReview;

    fn review_segment() -> Segment {
        Segment {
            extra: Default::default(),
            id: 0,
            revision: 1,
            start_ms: 0,
            end_ms: 1000,
            text: "原始识别".into(),
            finalized: false,
            correcting: true,
            review: Some(SentenceReview {
                extra: Default::default(),
                state: ReviewState::Pending,
                original_text: "原始识别".into(),
                model: "测试模型".into(),
                revised_text: None,
                reason: None,
                audio_ms: Some(1000),
                queue_ms: None,
                processing_ms: None,
            }),
        }
    }

    #[test]
    fn revision_records_original_result_and_real_processing_times() {
        let mut reviser = Reviser::spawn(Arc::new(AtomicBool::new(false)), || {
            |_: &[f32]| Some("复核结果".into())
        })
        .unwrap();
        let mut transcript = Transcript::default();
        let segment = review_segment();
        assert!(reviser.submit(&segment, vec![0.; 16000]));
        transcript.apply(segment);
        let (events, _) = unbounded();
        reviser.finish(&mut transcript, &events);
        let sentence = &transcript.segments[0];
        let review = sentence.review.as_ref().unwrap();
        assert_eq!(sentence.text, "复核结果");
        assert_eq!(review.original_text, "原始识别");
        assert_eq!(review.revised_text.as_deref(), Some("复核结果"));
        assert_eq!(review.state, ReviewState::Reviewed);
        assert!(review.queue_ms.is_some() && review.processing_ms.is_some());
    }

    #[test]
    fn empty_revision_is_visible_as_failed_and_keeps_original_text() {
        let mut reviser =
            Reviser::spawn(Arc::new(AtomicBool::new(false)), || |_: &[f32]| None).unwrap();
        let mut transcript = Transcript::default();
        let segment = review_segment();
        assert!(reviser.submit(&segment, vec![0.; 16000]));
        transcript.apply(segment);
        let (events, _) = unbounded();
        reviser.finish(&mut transcript, &events);
        let sentence = &transcript.segments[0];
        assert_eq!(sentence.text, "原始识别");
        assert_eq!(sentence.review.as_ref().unwrap().state, ReviewState::Failed);
    }

    #[test]
    fn second_pass_keeps_punctuation_and_review_versions_in_sync() {
        let mut reviser = Reviser::spawn(Arc::new(AtomicBool::new(false)), || {
            |_: &[f32]| Some("复核结果".into())
        })
        .unwrap();
        reviser.punctuation = Some(Arc::new(Punctuator::new(
            std::path::Path::new("/not-installed"),
            true,
            opencap_core::TranscriptionLanguage::Chinese,
        )));
        let mut transcript = Transcript::default();
        let segment = review_segment();
        assert!(reviser.submit(&segment, vec![0.; 16000]));
        transcript.apply(segment);
        let (events, _) = unbounded();
        reviser.finish(&mut transcript, &events);
        let segment = &transcript.segments[0];
        assert_eq!(segment.text, "复核结果。");
        assert_eq!(
            segment.review.as_ref().unwrap().revised_text.as_deref(),
            Some("复核结果。")
        );
    }

    #[test]
    fn slow_correction_does_not_block_next_sentence_and_backlog_preserves_native_text() {
        let (started, ready) = bounded(1);
        let (release, wait) = bounded(1);
        let mut reviser = Reviser::spawn(Arc::new(AtomicBool::new(false)), move || {
            move |_: &[f32]| {
                let _ = started.try_send(());
                wait.recv().unwrap();
                Some("公司的录屏".into())
            }
        })
        .unwrap();
        let (events, rx) = unbounded();
        let mut transcript = Transcript::default();
        let sentence = |id| Segment {
            extra: Default::default(),
            id,
            revision: 3,
            start_ms: id * 1000,
            end_ms: id * 1000 + 900,
            text: "公司的陆平".into(),
            finalized: false,
            correcting: true,
            review: None,
        };
        let first = sentence(0);
        assert!(reviser.submit(&first, vec![0.0; 16000]));
        transcript.apply(first);
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        for id in 1..=REVISION_QUEUE_CAPACITY as u64 {
            let segment = sentence(id);
            assert!(reviser.submit(&segment, vec![0.0; 16000]));
            transcript.apply(segment);
        }
        let mut next = sentence(REVISION_QUEUE_CAPACITY as u64 + 1);
        assert!(!reviser.submit(&next, vec![0.0; 16000]));
        next.finalized = true;
        next.correcting = false;
        assert!(transcript.apply(next));
        assert!(!reviser.poll(&mut transcript, &events));
        assert_eq!(transcript.segments.len(), REVISION_QUEUE_CAPACITY + 2);
        // The next sentence is already visible while the first decode is held.
        for _ in 0..=REVISION_QUEUE_CAPACITY {
            release.send(()).unwrap();
        }
        reviser.finish(&mut transcript, &events);
        assert!(
            transcript
                .segments
                .iter()
                .all(|s| s.finalized && !s.correcting)
        );
        assert_eq!(transcript.segments[0].text, "公司的录屏");
        assert_eq!(
            transcript.segments[REVISION_QUEUE_CAPACITY + 1].text,
            "公司的陆平"
        );
        assert_eq!(rx.try_iter().count(), REVISION_QUEUE_CAPACITY + 1);
    }

    #[test]
    fn disconnected_correction_keeps_every_pending_character() {
        let (_tx, results) = unbounded();
        drop(_tx);
        let mut segment = Segment {
            extra: Default::default(),
            id: 0,
            revision: 2,
            start_ms: 0,
            end_ms: 1000,
            text: "这个词不能少最后一个字".into(),
            finalized: false,
            correcting: true,
            review: None,
        };
        let mut transcript = Transcript::default();
        transcript.apply(segment.clone());
        let mut reviser = Reviser {
            requests: None,
            results,
            pending: [(0, segment.clone())].into(),
            cancelled: Arc::new(AtomicBool::new(false)),
            model: "测试模型".into(),
            punctuation: None,
            health: Arc::new(AtomicU8::new(0)),
            failure: Arc::new(Mutex::new(None)),
        };
        let (events, _) = unbounded();
        reviser.finish(&mut transcript, &events);
        segment = finalize(segment);
        assert_eq!(transcript.segments, vec![segment]);
    }

    #[test]
    fn cancelling_a_slow_revision_preserves_visible_words_without_waiting_for_decode() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (started, ready) = bounded(1);
        let (release, wait) = bounded(1);
        let mut reviser = Reviser::spawn(cancelled.clone(), move || {
            move |_: &[f32]| {
                started.send(()).unwrap();
                wait.recv().unwrap();
                Some("取消之后才返回的结果".into())
            }
        })
        .unwrap();
        let segment = Segment {
            extra: Default::default(),
            id: 0,
            revision: 1,
            start_ms: 0,
            end_ms: 1000,
            text: "已经显示的完整原文".into(),
            finalized: false,
            correcting: true,
            review: None,
        };
        assert!(reviser.submit(&segment, vec![0.0; 16000]));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut transcript = Transcript::default();
        transcript.apply(segment);
        let (events, _) = unbounded();
        cancelled.store(true, Ordering::Relaxed);
        reviser.finish(&mut transcript, &events);
        assert_eq!(transcript.text(), "已经显示的完整原文");
        assert!(!transcript.segments[0].correcting);
        release.send(()).unwrap();
        let late = reviser
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(!transcript.apply(late));
        assert_eq!(transcript.text(), "已经显示的完整原文");
    }
}
