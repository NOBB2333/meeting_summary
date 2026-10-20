//! Optional sentence correction must never hold up the streaming decoder.
use crate::{AsrEvent, Segment, Transcript};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub(crate) const MAX_REVISION_SAMPLES: usize = 30 * 16000;

struct Job {
    segment: Segment,
    audio: Vec<f32>,
}

pub(crate) struct Reviser {
    requests: Option<Sender<Job>>,
    results: Receiver<Segment>,
    pending: BTreeMap<u64, Segment>,
    cancelled: Arc<AtomicBool>,
}

impl Reviser {
    pub fn new(
        dir: PathBuf,
        language: opencap_core::TranscriptionLanguage,
        cancelled: Arc<AtomicBool>,
        events: Sender<AsrEvent>,
    ) -> anyhow::Result<Self> {
        Self::spawn(cancelled, move || {
            let model = super::worker::sense_voice(&dir, language);
            if let Err(e) = &model {
                let _ = events.send(AsrEvent::Status(format!(
                    "整句校正不可用，继续显示流式原文：{e:#}"
                )));
            }
            move |audio: &[f32]| {
                let model = model.as_ref().ok()?;
                let stream = model.create_stream();
                stream.accept_waveform(16000, audio);
                model.decode(&stream);
                stream
                    .get_result()
                    .map(|result| result.text.trim().to_owned())
            }
        })
    }

    fn spawn<F, D>(cancelled: Arc<AtomicBool>, create: F) -> anyhow::Result<Self>
    where
        F: FnOnce() -> D + Send + 'static,
        D: FnMut(&[f32]) -> Option<String>,
    {
        // At most one active decode and two waiting sentences. If slower than
        // speech, retain native streaming text instead of blocking or growing RAM.
        let (requests, rx) = bounded::<Job>(2);
        let (tx, results) = unbounded();
        let stopped = cancelled.clone();
        thread::Builder::new()
            .name("opencap-asr-revision".into())
            .spawn(move || {
                let mut decode = create();
                for mut job in rx {
                    if !stopped.load(Ordering::Relaxed)
                        && let Some(text) = decode(&job.audio).filter(|s| !s.is_empty())
                    {
                        job.segment.text = text;
                    }
                    let _ = tx.send(finalize(job.segment));
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            results,
            pending: BTreeMap::new(),
            cancelled,
        })
    }

    pub fn submit(&mut self, segment: &Segment, audio: Vec<f32>) -> bool {
        let Some(requests) = &self.requests else {
            return false;
        };
        if requests
            .try_send(Job {
                segment: segment.clone(),
                audio,
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
                    changed |= self.fallback(transcript, events);
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        changed
    }

    pub fn finish(&mut self, transcript: &mut Transcript, events: &Sender<AsrEvent>) {
        self.requests.take();
        while !self.pending.is_empty() && !self.cancelled.load(Ordering::Relaxed) {
            match self.results.recv_timeout(Duration::from_millis(100)) {
                Ok(segment) => {
                    self.apply(segment, transcript, events);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
        }
        self.fallback(transcript, events);
    }

    fn apply(
        &mut self,
        segment: Segment,
        transcript: &mut Transcript,
        events: &Sender<AsrEvent>,
    ) -> bool {
        self.pending.remove(&segment.id);
        if transcript.apply(segment.clone()) {
            let _ = events.send(AsrEvent::Segment(segment));
            true
        } else {
            false
        }
    }

    fn fallback(&mut self, transcript: &mut Transcript, events: &Sender<AsrEvent>) -> bool {
        let mut changed = false;
        for (_, segment) in std::mem::take(&mut self.pending) {
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
            id,
            revision: 3,
            start_ms: id * 1000,
            end_ms: id * 1000 + 900,
            text: "公司的陆平".into(),
            finalized: false,
            correcting: true,
        };
        let first = sentence(0);
        assert!(reviser.submit(&first, vec![0.0; 16000]));
        transcript.apply(first);
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        for id in 1..=2 {
            let segment = sentence(id);
            assert!(reviser.submit(&segment, vec![0.0; 16000]));
            transcript.apply(segment);
        }
        let mut next = sentence(3);
        assert!(!reviser.submit(&next, vec![0.0; 16000]));
        next.finalized = true;
        next.correcting = false;
        assert!(transcript.apply(next));
        assert!(!reviser.poll(&mut transcript, &events));
        assert_eq!(transcript.segments.len(), 4);
        // The next sentence is already visible while the first decode is held.
        for _ in 0..3 {
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
        assert_eq!(transcript.segments[3].text, "公司的陆平");
        assert_eq!(rx.try_iter().count(), 3);
    }

    #[test]
    fn disconnected_correction_keeps_every_pending_character() {
        let (_tx, results) = unbounded();
        drop(_tx);
        let mut segment = Segment {
            id: 0,
            revision: 2,
            start_ms: 0,
            end_ms: 1000,
            text: "这个词不能少最后一个字".into(),
            finalized: false,
            correcting: true,
        };
        let mut transcript = Transcript::default();
        transcript.apply(segment.clone());
        let mut reviser = Reviser {
            requests: None,
            results,
            pending: [(0, segment.clone())].into(),
            cancelled: Arc::new(AtomicBool::new(false)),
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
            id: 0,
            revision: 1,
            start_ms: 0,
            end_ms: 1000,
            text: "已经显示的完整原文".into(),
            finalized: false,
            correcting: true,
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
