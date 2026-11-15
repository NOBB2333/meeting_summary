//! An opt-in worker owns one persistent NeMo stream for the whole recording.
//! Its bounded input is independent of the recorder and of the ASR worker.
use crate::{NEMOTRON_DIAR_SHA256, NemoDiarizer, OnlineDiarizer, OnlineSpeakerSnapshot};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::PcmAudio;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum OnlineSpeakerEvent {
    Started {
        capacity: usize,
        model_sha256: String,
        library_sha256: String,
    },
    Snapshot(OnlineSpeakerSnapshot),
    FileInputEnded {
        end_ms: Option<u64>,
        audio_stream_index: Option<u64>,
        fingerprint: Option<opencap_core::MediaFingerprint>,
        error: Option<String>,
    },
    Failed(String),
}

pub struct OnlineSpeakerSession {
    events: Receiver<OnlineSpeakerEvent>,
    event_sender: Sender<OnlineSpeakerEvent>,
    snapshots: Receiver<OnlineSpeakerSnapshot>,
    started_delivered: bool,
    input: Option<Sender<PcmAudio>>,
    cancelled: Arc<AtomicBool>,
    expected_end_ms: Arc<AtomicU64>,
    processed_blocks: Arc<AtomicU64>,
    file_stage: Arc<AtomicU8>,
    worker: JoinHandle<()>,
    feeder: Option<JoinHandle<()>>,
}
impl OnlineSpeakerSession {
    /// Loading a library executes native code. The caller must pass a library
    /// explicitly selected by the user and trusted to implement the pinned ABI.
    /// The model file is checked against the pinned size and SHA-256.
    pub fn start_nemo(library: PathBuf, model: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            library.is_file() && model.is_file(),
            "请选择可信的 NeMo 原生库和说话人模型"
        );
        let (input, receiver): (Sender<PcmAudio>, Receiver<PcmAudio>) = bounded(1400);
        let (sender, events) = unbounded();
        let event_sender = sender.clone();
        // A snapshot includes the session's history. At most one intermediate
        // copy may wait for the UI; final/error events use the control channel.
        let (snapshot_sender, snapshots): (
            Sender<OnlineSpeakerSnapshot>,
            Receiver<OnlineSpeakerSnapshot>,
        ) = bounded(1);
        let stale_snapshots = snapshots.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let expected_end_ms = Arc::new(AtomicU64::new(u64::MAX));
        let worker_expected_end = expected_end_ms.clone();
        let processed_blocks = Arc::new(AtomicU64::new(0));
        let file_stage = Arc::new(AtomicU8::new(0));
        let worker_processed_blocks = processed_blocks.clone();
        let worker = thread::Builder::new()
            .name("opencap-online-speakers".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        // The opaque native stream is constructed and used only here.
                        let mut diarizer = unsafe { NemoDiarizer::open(&library, &model) }
                            .context("在线说话人模型启动失败")?;
                        let _ = sender.send(OnlineSpeakerEvent::Started {
                            capacity: diarizer.capacity(),
                            model_sha256: NEMOTRON_DIAR_SHA256.into(),
                            library_sha256: diarizer.library_sha256.clone(),
                        });
                        let mut last_update = Instant::now();
                        let mut first_audio = true;
                        loop {
                            if worker_cancelled.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            match receiver.recv_timeout(Duration::from_millis(100)) {
                                Ok(audio) => {
                                    if first_audio && !audio.samples.is_empty() {
                                        first_audio = false;
                                        if audio.source != opencap_core::AudioSource::ImportedMedia
                                        {
                                            anyhow::ensure!(
                                                audio.pts_us <= 2_000,
                                                "录制开头的说话人音频缺失至 {}ms",
                                                audio.pts_us / 1000
                                            );
                                        }
                                    }
                                    diarizer
                                        .push(&audio)
                                        .context("在线说话人音频不连续或处理失败")?;
                                    worker_processed_blocks.fetch_add(1, Ordering::Relaxed);
                                    if last_update.elapsed() >= Duration::from_secs(2) {
                                        let snapshot = diarizer.snapshot()?;
                                        if let Err(crossbeam_channel::TrySendError::Full(
                                            snapshot,
                                        )) = snapshot_sender.try_send(snapshot)
                                        {
                                            // Keep the most recent completed state if the UI is
                                            // behind. A concurrent drain may already have removed
                                            // the old snapshot; either way, retry without blocking.
                                            let _ = stale_snapshots.try_recv();
                                            let _ = snapshot_sender.try_send(snapshot);
                                        }
                                        last_update = Instant::now();
                                    }
                                }
                                Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
                                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        if !worker_cancelled.load(Ordering::Relaxed) {
                            let mut snapshot = diarizer.finish()?;
                            let expected = worker_expected_end.load(Ordering::Acquire);
                            if expected != u64::MAX
                                && expected > snapshot.input_end_ms.saturating_add(2)
                            {
                                let actual = snapshot.input_end_ms;
                                snapshot.finished = false;
                                let _ = sender.send(OnlineSpeakerEvent::Snapshot(snapshot));
                                anyhow::bail!("说话人音频尾部缺失 {actual}–{expected}ms");
                            }
                            let _ = sender.send(OnlineSpeakerEvent::Snapshot(snapshot));
                        }
                        Ok(())
                    }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("在线说话人线程异常退出".into()),
                };
                if let Some(error) = error {
                    // A file feeder may still be hashing a large source or
                    // waiting on a full queue. Stop it as soon as the model
                    // worker fails, without waiting for the UI to poll.
                    worker_cancelled.store(true, Ordering::Relaxed);
                    let _ = sender.send(OnlineSpeakerEvent::Failed(error));
                }
            })?;
        Ok(Self {
            events,
            event_sender,
            snapshots,
            started_delivered: false,
            input: Some(input),
            cancelled,
            expected_end_ms,
            processed_blocks,
            file_stage,
            worker,
            feeder: None,
        })
    }
    /// Replay a file on its own worker. Backpressure is confined to this
    /// independent decoder, so a slow diarizer does not drop the long-file
    /// tail or stall the transcription worker.
    pub fn start_nemo_file(
        library: PathBuf,
        model: PathBuf,
        source: PathBuf,
        audio_stream_index: Option<u64>,
    ) -> Result<Self> {
        let mut session = Self::start_nemo(library, model)?;
        let input = session.input.take().context("在线说话人输入不可用")?;
        let cancelled = session.cancelled.clone();
        let expected_end = session.expected_end_ms.clone();
        let events = session.event_sender.clone();
        let file_stage = session.file_stage.clone();
        let feeder = thread::Builder::new()
            .name("opencap-speaker-file-input".into())
            .spawn(move || {
                let mut end_ms = None;
                let mut actual_stream = None;
                let mut verified_fingerprint = None;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        file_stage.store(1, Ordering::Relaxed);
                        let before = opencap_core::fingerprint_media(&source, Some(&cancelled))?;
                        file_stage.store(2, Ordering::Relaxed);
                        let decoded = crate::audio::decode_file_with_expected_stream(
                            &source,
                            audio_stream_index,
                            |audio, _| {
                                anyhow::ensure!(
                                    !cancelled.load(Ordering::Relaxed),
                                    "文件说话人输入已取消"
                                );
                                let frames =
                                    audio.samples.len() / usize::from(audio.channels.max(1));
                                let end = (audio.pts_us.max(0) as u64 / 1000).saturating_add(
                                    (frames as u64).saturating_mul(1000)
                                        / u64::from(audio.sample_rate.max(1)),
                                );
                                let mut block = audio;
                                loop {
                                    anyhow::ensure!(
                                        !cancelled.load(Ordering::Relaxed),
                                        "文件说话人输入已取消"
                                    );
                                    match input.send_timeout(block, Duration::from_millis(100)) {
                                        Ok(()) => break,
                                        Err(crossbeam_channel::SendTimeoutError::Timeout(
                                            returned,
                                        )) => block = returned,
                                        Err(crossbeam_channel::SendTimeoutError::Disconnected(
                                            _,
                                        )) => anyhow::bail!("说话人处理线程已停止"),
                                    }
                                }
                                end_ms = Some(end);
                                Ok(())
                            },
                        )?;
                        actual_stream = Some(u64::try_from(decoded.audio_stream_index)?);
                        anyhow::ensure!(end_ms.is_some(), "文件没有可用音频");
                        file_stage.store(3, Ordering::Relaxed);
                        anyhow::ensure!(
                            opencap_core::fingerprint_media(&source, Some(&cancelled))? == before,
                            "文件说话人处理期间媒体发生变化"
                        );
                        verified_fingerprint = Some(before);
                        expected_end.store(end_ms.unwrap(), Ordering::Release);
                        Ok(())
                    }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("文件说话人输入线程异常退出".into()),
                };
                file_stage.store(4, Ordering::Relaxed);
                let _ = events.send(OnlineSpeakerEvent::FileInputEnded {
                    end_ms,
                    audio_stream_index: actual_stream,
                    fingerprint: verified_fingerprint,
                    error,
                });
                drop(input);
            })?;
        session.feeder = Some(feeder);
        Ok(session)
    }
    pub fn audio_sender(&self) -> Option<Sender<PcmAudio>> {
        self.input.clone()
    }
    /// Startup is applied before any queued snapshot, and the terminal event
    /// after it. This keeps an older intermediate snapshot from replacing the
    /// final result when the consumer resumes after a stall.
    pub fn drain_events(&mut self) -> Vec<OnlineSpeakerEvent> {
        let mut started = Vec::new();
        let mut terminal = Vec::new();
        for event in self.events.try_iter() {
            match event {
                OnlineSpeakerEvent::Started { .. } => {
                    self.started_delivered = true;
                    started.push(event);
                }
                _ => terminal.push(event),
            }
        }
        if self.started_delivered {
            started.extend(self.snapshots.try_iter().map(OnlineSpeakerEvent::Snapshot));
        }
        started.extend(terminal);
        started
    }
    pub fn finish_input(&mut self, expected_end_ms: Option<u64>) {
        if let Some(end) = expected_end_ms {
            self.expected_end_ms.store(end, Ordering::Release);
        }
        self.input.take();
    }
    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.input.take();
    }
    pub fn finished(&self) -> bool {
        self.worker.is_finished()
            && self
                .feeder
                .as_ref()
                .is_none_or(|worker| worker.is_finished())
    }
    /// Counts completed backend pushes, including audio that has no new labels.
    pub fn processed_blocks(&self) -> u64 {
        self.processed_blocks.load(Ordering::Relaxed)
    }
    /// 1: initial fingerprint, 2: decode/feed, 3: final fingerprint, 4: input ended.
    pub fn file_stage(&self) -> u8 {
        self.file_stage.load(Ordering::Relaxed)
    }
}
impl Drop for OnlineSpeakerSession {
    fn drop(&mut self) {
        self.cancel();
    }
}
