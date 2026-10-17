use crate::{
    ModelKind, Segment, Transcript,
    audio::{Normalizer, decode_file},
    prepare,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::PcmAudio;
use sherpa_onnx::*;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum AsrEvent {
    Status(String),
    Segment(Segment),
    Progress(f64),
    Gap(u64, u64),
    Finished(Transcript),
    Error(String),
}
pub struct AsrSession {
    pub events: Receiver<AsrEvent>,
    input: Option<Sender<PcmAudio>>,
    output: Arc<Mutex<Option<PathBuf>>>,
    cancelled: Arc<AtomicBool>,
    expected_end: Arc<AtomicU64>,
    worker: JoinHandle<()>,
}
impl AsrSession {
    pub fn live(root: PathBuf) -> Result<Self> {
        Self::live_with_hotwords(root, crate::DEFAULT_HOTWORDS.into())
    }
    pub fn live_with_hotwords(root: PathBuf, hotwords: String) -> Result<Self> {
        Self::live_with_revision(root, hotwords, true)
    }
    pub fn live_with_revision(root: PathBuf, hotwords: String, revision: bool) -> Result<Self> {
        Self::spawn(
            root,
            ModelKind::Streaming,
            None,
            None,
            false,
            hotwords,
            revision,
        )
    }
    /// The caller selects only installed models; sentence mode uses the same VAD
    /// engine as file transcription, with live PCM instead of a source file.
    pub fn live_with_plan(
        root: PathBuf,
        hotwords: String,
        plan: crate::LiveModelPlan,
    ) -> Result<Self> {
        Self::spawn(
            root,
            plan.kind,
            None,
            None,
            false,
            hotwords,
            plan.context_revision,
        )
    }
    pub fn file(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
    ) -> Result<Self> {
        Self::file_with_hotwords(
            root,
            kind,
            source,
            output,
            realtime,
            crate::DEFAULT_HOTWORDS.into(),
        )
    }
    pub fn file_with_hotwords(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
    ) -> Result<Self> {
        Self::file_with_revision(root, kind, source, output, realtime, hotwords, true)
    }
    pub fn file_with_revision(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
    ) -> Result<Self> {
        Self::spawn(
            root,
            kind,
            Some(source),
            Some(output),
            realtime,
            hotwords,
            revision,
        )
    }
    fn spawn(
        root: PathBuf,
        kind: ModelKind,
        source: Option<PathBuf>,
        destination: Option<PathBuf>,
        realtime: bool,
        hotwords: String,
        context_revision: bool,
    ) -> Result<Self> {
        let (input, rx) = bounded(1400); // about 30 seconds of mixed recording PCM
        let (tx, events) = unbounded();
        let output = Arc::new(Mutex::new(destination));
        let worker_output = output.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let expected_end = Arc::new(AtomicU64::new(0));
        let worker_end = expected_end.clone();
        let worker = thread::Builder::new()
            .name("opencap-asr-cpu".into())
            .spawn(move || {
                let job_started = Instant::now();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _ = tx.send(AsrEvent::Status("正在准备本地识别模型…".into()));
                        prepare(kind, &root, |bytes, total| {
                            let message = total
                                .map(|total| {
                                    format!(
                                        "下载模型 {:.0}%",
                                        bytes as f64 * 100.0 / total.max(1) as f64
                                    )
                                })
                                .unwrap_or_else(|| {
                                    format!("下载模型 {:.1} MB", bytes as f64 / 1e6)
                                });
                            let _ = tx.send(AsrEvent::Status(message));
                        })?;
                        if kind == ModelKind::Streaming && context_revision {
                            let _ = tx.send(AsrEvent::Status("正在准备上下文校正模型…".into()));
                            prepare(ModelKind::Offline, &root, |bytes, total| {
                                let _ = tx.send(AsrEvent::Status(format!(
                                    "下载校正模型 {:.0}%",
                                    bytes as f64 * 100.0 / total.unwrap_or(bytes.max(1)) as f64
                                )));
                            })?;
                        }
                        let revision = if kind == ModelKind::Streaming && context_revision {
                            Some(ModelKind::Offline.path(&root))
                        } else {
                            None
                        };
                        let mut engine = Engine::new(
                            kind,
                            &kind.path(&root),
                            &hotwords,
                            revision.as_deref(),
                            source.is_some(),
                        )?;
                        let _ = tx.send(AsrEvent::Status(
                            if source.is_some() {
                                "文件识别已就绪 · 正在处理音频"
                            } else if kind == ModelKind::Offline {
                                "逐句实时识别已就绪 · 复用文件模型，停顿后显示文字"
                            } else {
                                "流式识别已就绪 · 当前句可修正"
                            }
                            .into(),
                        ));
                        let mut transcript = Transcript {
                            source: source.clone(),
                            model: kind.directory().into(),
                            ..Default::default()
                        };
                        let mut normalizer = Normalizer::new();
                        let mut expected_us: Option<i64> = None;
                        let began = Instant::now();
                        let mut last_progress = Instant::now();
                        let mut last_checkpoint = Instant::now();
                        let mut checkpoint_dirty = false;
                        let read_result = {
                            let mut consume = |audio: PcmAudio,
                                               duration: Option<f64>|
                             -> Result<()> {
                                if let Some(seconds) = duration.filter(|s| s.is_finite() && *s > 0.)
                                {
                                    transcript.source_duration_ms = Some((seconds * 1000.) as u64);
                                }
                                anyhow::ensure!(
                                    !worker_cancelled.load(Ordering::Relaxed),
                                    "转录已取消"
                                );
                                if audio.samples.is_empty() {
                                    let tail = normalizer.flush()?;
                                    engine.feed(
                                        &tail,
                                        expected_us.unwrap_or(0) / 1000,
                                        &mut transcript,
                                        &tx,
                                    )?;
                                    engine.boundary(&mut transcript, &tx)?;
                                    normalizer = Normalizer::new();
                                    expected_us = None;
                                    return Ok(());
                                }
                                if let Some(expected) = expected_us
                                    && audio.pts_us > expected + 200_000
                                {
                                    engine.boundary(&mut transcript, &tx)?;
                                    let gap = (
                                        expected.max(0) as u64 / 1000,
                                        audio.pts_us.max(0) as u64 / 1000,
                                    );
                                    transcript.gaps.push(gap);
                                    let _ = tx.send(AsrEvent::Gap(gap.0, gap.1));
                                    normalizer = Normalizer::new();
                                }
                                let end_us = audio.pts_us
                                    + audio.samples.len() as i64 * 1_000_000
                                        / audio.channels as i64
                                        / audio.sample_rate as i64;
                                let normalized = normalizer.convert(&audio)?;
                                engine.feed(
                                    &normalized,
                                    audio.pts_us.max(0) / 1000,
                                    &mut transcript,
                                    &tx,
                                )?;
                                expected_us = Some(end_us);
                                if let Some(duration) = duration
                                    && last_progress.elapsed() >= Duration::from_millis(300)
                                {
                                    let _ = tx.send(AsrEvent::Progress(
                                        (end_us as f64 / 1e6 / duration).clamp(0.0, 1.0),
                                    ));
                                    last_progress = Instant::now();
                                }
                                if realtime {
                                    let remaining = Duration::from_micros(end_us.max(0) as u64)
                                        .saturating_sub(began.elapsed());
                                    if !remaining.is_zero() {
                                        thread::sleep(remaining.min(Duration::from_millis(200)));
                                    }
                                }
                                checkpoint_dirty |= engine.take_committed();
                                // Never hold the UI-shared path mutex while serializing or writing.
                                if checkpoint_dirty
                                    && last_checkpoint.elapsed() >= Duration::from_secs(5)
                                {
                                    let base = worker_output.lock().unwrap().clone();
                                    if let Some(base) = base {
                                        if transcript.source.is_none() {
                                            transcript.source = Some(base.clone());
                                        }
                                        transcript.checkpoint(&base)?;
                                        checkpoint_dirty = false;
                                        last_checkpoint = Instant::now();
                                    }
                                }
                                Ok(())
                            };
                            if let Some(source) = &source {
                                decode_file(source, &mut consume)
                            } else {
                                let mut result = Ok(());
                                loop {
                                    match rx.recv_timeout(Duration::from_millis(100)) {
                                        Ok(audio) => {
                                            if let Err(e) = consume(audio, None) {
                                                result = Err(e);
                                                break;
                                            }
                                        }
                                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                                            if worker_cancelled.load(Ordering::Relaxed) {
                                                break;
                                            }
                                        }
                                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                            break;
                                        }
                                    }
                                }
                                result
                            }
                        };
                        let tail = normalizer.flush()?;
                        engine.feed(
                            &tail,
                            expected_us.unwrap_or(0) / 1000,
                            &mut transcript,
                            &tx,
                        )?;
                        engine.boundary(&mut transcript, &tx)?;
                        transcript.cancelled = worker_cancelled.load(Ordering::Relaxed);
                        let last_ms = expected_us.unwrap_or(0).max(0) as u64 / 1000;
                        let wanted = worker_end.load(Ordering::Relaxed);
                        if wanted > last_ms + 200 {
                            transcript.gaps.push((last_ms, wanted));
                            let _ = tx.send(AsrEvent::Gap(last_ms, wanted));
                        }
                        transcript.complete = read_result.is_ok()
                            && !transcript.cancelled
                            && transcript.gaps.is_empty();
                        transcript.processing_seconds = Some(job_started.elapsed().as_secs_f64());
                        let base = worker_output.lock().unwrap().clone();
                        if let Some(base) = base {
                            if transcript.source.is_none() {
                                transcript.source = Some(base.clone());
                            }
                            transcript.save(&base)?;
                        }
                        let _ = tx.send(AsrEvent::Finished(transcript));
                        if !worker_cancelled.load(Ordering::Relaxed) {
                            read_result?;
                        }
                        Ok(())
                    }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(_) => Some("识别线程异常，原录制文件已保留".into()),
                };
                if let Some(error) = error {
                    let _ = tx.send(AsrEvent::Error(error));
                }
            })?;
        Ok(Self {
            events,
            input: Some(input),
            output,
            cancelled,
            expected_end,
            worker,
        })
    }
    pub fn audio_sender(&self) -> Sender<PcmAudio> {
        self.input.as_ref().expect("live input open").clone()
    }
    pub fn set_output(&self, output: PathBuf) {
        *self.output.lock().unwrap() = Some(output);
    }
    pub fn finish_input(&mut self) {
        self.input.take();
    }
    pub fn finish_at(&mut self, milliseconds: u64) {
        self.expected_end.store(milliseconds, Ordering::Relaxed);
        self.finish_input();
    }
    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.finish_input();
    }
    pub fn finished(&self) -> bool {
        self.worker.is_finished()
    }
}
fn sense_voice(dir: &Path) -> Result<OfflineRecognizer> {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
        model: Some(dir.join("model.int8.onnx").to_string_lossy().into_owned()),
        language: Some("auto".into()),
        use_itn: true,
    };
    config.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into_owned());
    config.model_config.provider = Some("cpu".into());
    config.model_config.num_threads = 1;
    OfflineRecognizer::create(&config).context("无法加载上下文校正模型")
}
impl Drop for AsrSession {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.input.take();
    }
}

enum Engine {
    Online {
        recognizer: OnlineRecognizer,
        stream: OnlineStream,
        origin: Option<u64>,
        id: u64,
        revision: u64,
        previous: String,
        end: u64,
        committed: bool,
        correction: Option<OfflineRecognizer>,
        utterance_audio: Vec<f32>,
        last_correction: usize,
        partial_revision: bool,
        corrected_prefix: Option<(String, String)>,
    },
    Offline {
        recognizer: OfflineRecognizer,
        vad: VoiceActivityDetector,
        origin: Option<u64>,
        id: u64,
        end: u64,
        committed: bool,
    },
}
impl Engine {
    fn new(
        kind: ModelKind,
        dir: &Path,
        hotwords: &str,
        revision_dir: Option<&Path>,
        partial_revision: bool,
    ) -> Result<Self> {
        let file = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
        match kind {
            ModelKind::Streaming => {
                let mut config = OnlineRecognizerConfig {
                    decoding_method: Some("modified_beam_search".into()),
                    max_active_paths: 4,
                    enable_endpoint: true,
                    rule1_min_trailing_silence: 2.4,
                    rule2_min_trailing_silence: 0.8,
                    rule3_min_utterance_length: 20.0,
                    ..Default::default()
                };
                config.model_config.transducer = OnlineTransducerModelConfig {
                    encoder: file(ModelKind::Streaming.files()[0]),
                    decoder: file(ModelKind::Streaming.files()[1]),
                    joiner: file(ModelKind::Streaming.files()[2]),
                };
                config.model_config.tokens = file("tokens.txt");
                config.model_config.provider = Some("cpu".into());
                config.model_config.num_threads = 1;
                let vocabulary = std::fs::read_to_string(dir.join("tokens.txt"))?;
                let vocabulary: std::collections::HashSet<_> = vocabulary
                    .lines()
                    .filter_map(|line| line.split_whitespace().next())
                    .collect();
                let hotwords: Vec<_> = hotwords
                    .split([',', '，', ';', '；', '\n', ' '])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(50)
                    .collect();
                anyhow::ensure!(
                    hotwords.iter().all(|word| word
                        .chars()
                        .all(|c| vocabulary.contains(c.to_string().as_str()))),
                    "当前中文模型的术语只能包含词表中的中文字符，请修改术语或清空后重试"
                );
                if !hotwords.is_empty() {
                    config.hotwords_buf = Some(hotwords.join("\n").into_bytes());
                    config.hotwords_score = 3.0;
                    config.model_config.modeling_unit = Some("cjkchar".into());
                }
                let recognizer = OnlineRecognizer::create(&config).context("无法加载流式模型")?;
                let stream = recognizer.create_stream();
                Ok(Self::Online {
                    recognizer,
                    stream,
                    origin: None,
                    id: 0,
                    revision: 0,
                    previous: String::new(),
                    end: 0,
                    committed: false,
                    correction: revision_dir.map(sense_voice).transpose()?,
                    utterance_audio: Vec::new(),
                    last_correction: 0,
                    partial_revision,
                    corrected_prefix: None,
                })
            }
            ModelKind::Offline => {
                let mut config = OfflineRecognizerConfig::default();
                config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                    model: file("model.int8.onnx"),
                    language: Some("auto".into()),
                    use_itn: true,
                };
                config.model_config.tokens = file("tokens.txt");
                config.model_config.provider = Some("cpu".into());
                config.model_config.num_threads = 1;
                let recognizer = OfflineRecognizer::create(&config).context("无法加载离线模型")?;
                let config = VadModelConfig {
                    silero_vad: SileroVadModelConfig {
                        model: file("silero_vad.onnx"),
                        threshold: 0.5,
                        min_silence_duration: 0.5,
                        min_speech_duration: 0.25,
                        window_size: 512,
                        max_speech_duration: 15.0,
                    },
                    sample_rate: 16000,
                    num_threads: 1,
                    provider: Some("cpu".into()),
                    ..Default::default()
                };
                let vad = VoiceActivityDetector::create(&config, 30.0)
                    .context("无法加载语音活动检测模型")?;
                Ok(Self::Offline {
                    recognizer,
                    vad,
                    origin: None,
                    id: 0,
                    end: 0,
                    committed: false,
                })
            }
        }
    }
    fn feed(
        &mut self,
        samples: &[f32],
        pts_ms: i64,
        t: &mut Transcript,
        tx: &Sender<AsrEvent>,
    ) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        match self {
            Self::Online {
                stream,
                utterance_audio,
                origin,
                end,
                ..
            } => {
                origin.get_or_insert(pts_ms.max(0) as u64);
                *end = pts_ms.max(0) as u64 + samples.len() as u64 * 1000 / 16000;
                stream.accept_waveform(16000, samples);
                utterance_audio.extend_from_slice(samples);
            }
            Self::Offline {
                vad, origin, end, ..
            } => {
                origin.get_or_insert(pts_ms.max(0) as u64);
                *end = pts_ms.max(0) as u64 + samples.len() as u64 * 1000 / 16000;
                vad.accept_waveform(samples);
            }
        }
        self.decode(false, t, tx)
    }
    fn decode(
        &mut self,
        force_final: bool,
        t: &mut Transcript,
        tx: &Sender<AsrEvent>,
    ) -> Result<()> {
        match self {
            Self::Online {
                recognizer,
                stream,
                origin,
                id,
                revision,
                previous,
                end,
                committed,
                correction,
                utterance_audio,
                last_correction,
                partial_revision,
                corrected_prefix,
            } => {
                while recognizer.is_ready(stream) {
                    recognizer.decode(stream);
                }
                let final_now = force_final || recognizer.is_endpoint(stream);
                let result = recognizer
                    .get_result(stream)
                    .context("流式识别没有返回结果")?;
                let native_text = result.text.trim().to_owned();
                let mut text = if let Some((old, fixed)) = corrected_prefix.as_ref() {
                    native_text
                        .strip_prefix(old)
                        .map(|suffix| format!("{fixed}{suffix}"))
                        .unwrap_or_else(|| native_text.clone())
                } else {
                    native_text.clone()
                };
                // Live recording corrects only at endpoints, avoiding repeated 5/10/15/20s
                // offline decodes of the same audio. File playback may revise partials.
                // Streaming hypotheses remain immediate in either mode.
                if let Some(correction) = correction
                    && !native_text.is_empty()
                    && !utterance_audio.is_empty()
                    && (final_now
                        || (*partial_revision
                            && utterance_audio.len().saturating_sub(*last_correction) >= 5 * 16000))
                {
                    let correction_stream = correction.create_stream();
                    correction_stream.accept_waveform(16000, utterance_audio);
                    correction.decode(&correction_stream);
                    let fixed = correction_stream
                        .get_result()
                        .context("上下文校正没有返回结果")?
                        .text
                        .trim()
                        .to_owned();
                    if !fixed.is_empty() {
                        text = if final_now {
                            fixed
                        } else {
                            bounded_revision(&native_text, &fixed)
                        };
                        *corrected_prefix = Some((native_text, text.clone()));
                    }
                    *last_correction = utterance_audio.len();
                }
                if !text.is_empty() && (text != *previous || final_now) {
                    *revision += 1;
                    let base = origin.unwrap_or(0)
                        + (result.start_time.unwrap_or(0.0).max(0.0) * 1000.0) as u64;
                    let start = base
                        + result
                            .timestamps
                            .as_ref()
                            .and_then(|ts| ts.first())
                            .map(|v| (*v).max(0.0) * 1000.0)
                            .unwrap_or(0.0) as u64;
                    let last = result
                        .timestamps
                        .as_ref()
                        .and_then(|ts| ts.last())
                        .map(|v| base + ((*v).max(0.0) * 1000.0) as u64 + 100)
                        .unwrap_or(*end);
                    let segment = Segment {
                        id: *id,
                        revision: *revision,
                        start_ms: start.min(*end),
                        end_ms: last.min(*end).max(start.min(*end)),
                        text: text.clone(),
                        finalized: final_now,
                    };
                    t.apply(segment.clone());
                    let _ = tx.send(AsrEvent::Segment(segment));
                    *previous = text;
                    if final_now {
                        *committed = true;
                    }
                }
                if final_now {
                    recognizer.reset(stream);
                    *id += 1;
                    *revision = 0;
                    previous.clear();
                    utterance_audio.clear();
                    *last_correction = 0;
                    *corrected_prefix = None;
                }
            }
            Self::Offline {
                recognizer,
                vad,
                origin,
                id,
                end,
                committed,
            } => {
                while let Some(speech) = vad.front() {
                    let stream = recognizer.create_stream();
                    stream.accept_waveform(16000, speech.samples());
                    recognizer.decode(&stream);
                    let result = stream.get_result().context("离线识别没有返回结果")?;
                    let start = origin.unwrap_or(0) + speech.start().max(0) as u64 * 1000 / 16000;
                    let stop = (start + speech.samples().len() as u64 * 1000 / 16000).min(*end);
                    let text = result.text.trim().to_owned();
                    if !text.is_empty() {
                        let segment = Segment {
                            id: *id,
                            revision: 1,
                            start_ms: start,
                            end_ms: stop.max(start),
                            text,
                            finalized: true,
                        };
                        t.apply(segment.clone());
                        let _ = tx.send(AsrEvent::Segment(segment));
                        *committed = true;
                        *id += 1;
                    }
                    vad.pop();
                }
            }
        }
        Ok(())
    }
    fn boundary(&mut self, t: &mut Transcript, tx: &Sender<AsrEvent>) -> Result<()> {
        match self {
            Self::Online { stream, origin, .. } if origin.is_some() => {
                stream.accept_waveform(16000, &vec![0.0; 16000]);
                stream.input_finished();
            }
            Self::Offline { vad, .. } => vad.flush(),
            _ => return Ok(()),
        }
        self.decode(true, t, tx)?;
        match self {
            Self::Online {
                recognizer,
                stream,
                origin,
                ..
            } => {
                *stream = recognizer.create_stream();
                *origin = None;
            }
            Self::Offline { vad, origin, .. } => {
                vad.reset();
                *origin = None;
            }
        }
        Ok(())
    }
    fn take_committed(&mut self) -> bool {
        let value = match self {
            Self::Online { committed, .. } | Self::Offline { committed, .. } => committed,
        };
        std::mem::take(value)
    }
}
fn bounded_revision(native: &str, corrected: &str) -> String {
    // Offline decoders can anticipate the next word. Keep the provisional
    // replacement within the streaming hypothesis length, so its next delta
    // doesn't append that anticipated word a second time. Finals stay verbatim.
    corrected
        .chars()
        .filter(|c| {
            !c.is_whitespace() && !matches!(c, '，' | '。' | '！' | '？' | '；' | '、' | '：')
        })
        .take(native.chars().count())
        .collect()
}
#[cfg(test)]
mod revision_tests {
    use super::*;
    #[test]
    fn corrected_context_does_not_preappend_future_streaming_words() {
        assert_eq!(
            bounded_revision("公司的陆平我们决定", "公司的录屏，我们决定先完成。"),
            "公司的录屏我们决定"
        );
        assert_eq!(
            bounded_revision("预算是两万元", "预算是2万元。"),
            "预算是2万元"
        );
    }
}

#[cfg(test)]
mod live_integration_tests {
    use super::*;
    #[test]
    #[ignore = "requires already installed local models and OPENCAP_LIVE_TEST_AUDIO (synthetic speech)"]
    fn installed_models_emit_live_segments_before_input_closes() {
        let input = PathBuf::from(
            std::env::var_os("OPENCAP_LIVE_TEST_AUDIO").expect("synthetic test audio required"),
        );
        let root = crate::model_root();
        assert!(
            ModelKind::Offline.ready(&root),
            "install models explicitly before this check"
        );
        let dir = std::env::temp_dir().join(format!(
            "opencap-live-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for kind in [ModelKind::Offline, ModelKind::Streaming] {
            if !kind.ready(&root) {
                continue;
            }
            let mut session = AsrSession::live_with_plan(
                root.clone(),
                String::new(),
                crate::LiveModelPlan {
                    kind,
                    context_revision: kind == ModelKind::Streaming,
                },
            )
            .unwrap();
            let base = dir.join(format!("{kind:?}.m4a"));
            session.set_output(base.clone());
            let sender = session.audio_sender();
            let mut end_ms = 0;
            decode_file(&input, |audio, _| {
                end_ms = (audio.pts_us.max(0) as u64
                    + audio.samples.len() as u64 * 1_000_000
                        / audio.channels as u64
                        / audio.sample_rate as u64)
                    / 1000;
                sender.send(audio)?;
                Ok(())
            })
            .unwrap();
            drop(sender);
            // Own input remains open: a segment must be visible before EOF/finalization.
            let mut emitted = false;
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match session
                    .events
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                {
                    AsrEvent::Segment(s) if !s.text.is_empty() => {
                        emitted = true;
                        break;
                    }
                    AsrEvent::Error(e) => panic!("{kind:?}: {e}"),
                    _ => {}
                }
            }
            assert!(emitted, "{kind:?} did not emit live text");
            session.finish_at(end_ms);
            loop {
                match session
                    .events
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap()
                {
                    AsrEvent::Finished(t) => {
                        assert!(t.complete, "{t:?}");
                        assert!(!t.text().is_empty());
                        assert!(base.with_extension("transcript.docx").exists());
                        break;
                    }
                    AsrEvent::Error(e) => panic!("{kind:?}: {e}"),
                    _ => {}
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
