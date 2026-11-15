//! Experimental FFmpeg + CPAL playback with a bounded CPU video preview.
//! Output is bounded; seek/pause drop the device stream before starting a new generation.
use crate::{PlaybackRange, PresentationClock};
use anyhow::{Context, Result, ensure};
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender, bounded};
use ffmpeg_next as ff;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Paused,
    Buffering,
    Playing,
    Ended,
    Failed,
    Closing,
    Closed,
}
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSnapshot {
    pub state: PlaybackState,
    pub verifying_source: bool,
    pub source_verified: bool,
    pub position_pending: bool,
    applied_position_request: u64,
    pub generation: u64,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub volume: f32,
    pub speed: f32,
    pub range: Option<PlaybackRange>,
    pub error: Option<String>,
    pub video_error: Option<String>,
    pub has_video: bool,
}

struct VerifiedSource {
    identity: same_file::Handle,
    size: u64,
    modified: std::time::SystemTime,
}
impl VerifiedSource {
    fn capture(path: &Path) -> Result<Self> {
        let identity = same_file::Handle::from_path(path)?;
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            identity,
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }
    fn check(&self, path: &Path) -> Result<()> {
        let metadata = std::fs::metadata(path).context("已核对的媒体不可访问")?;
        ensure!(
            metadata.len() == self.size
                && metadata.modified()? == self.modified
                && same_file::Handle::from_path(path)? == self.identity,
            "媒体在播放期间发生变化，请重新打开并核对"
        );
        Ok(())
    }
}
pub struct PlaybackVideoFrame {
    pub generation: u64,
    pub pts_ms: u64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}
#[derive(Debug, Clone, Copy)]
pub enum PlaybackCommand {
    Play,
    /// Clear any range, seek, and play as a single queued command.
    PlayAt(u64),
    Pause,
    Seek(u64),
    SetVolume(f32),
    SetSpeed(f32),
    SetRange(Option<PlaybackRange>),
    /// Replace the range, seek to its start and play as one queued operation.
    PlayRange(PlaybackRange),
    Close,
}

pub struct PlaybackSession {
    commands: Sender<(PlaybackCommand, u64)>,
    command_submission: parking_lot::Mutex<()>,
    next_position_request: AtomicU64,
    submitted_position_request: AtomicU64,
    expected_audio_stream: Option<u64>,
    expected_fingerprint: Option<opencap_core::MediaFingerprint>,
    verification_cancelled: Arc<AtomicBool>,
    snapshot: Arc<parking_lot::Mutex<PlaybackSnapshot>>,
    worker: Option<JoinHandle<()>>,
    video_frame: Arc<parking_lot::Mutex<Option<PlaybackVideoFrame>>>,
}
impl PlaybackSession {
    pub fn open(path: PathBuf) -> Result<Self> {
        Self::open_with_audio_stream(path, None)
    }
    pub fn open_with_audio_stream(
        path: PathBuf,
        expected_audio_stream: Option<u64>,
    ) -> Result<Self> {
        Self::open_with_source(path, expected_audio_stream, None)
    }
    pub fn open_with_source(
        path: PathBuf,
        expected_audio_stream: Option<u64>,
        expected_fingerprint: Option<opencap_core::MediaFingerprint>,
    ) -> Result<Self> {
        ensure!(path.is_file(), "原媒体不可访问");
        let activity = opencap_core::managed_meeting_activity(&path)?;
        let (commands, rx) = bounded(64);
        let snapshot = Arc::new(parking_lot::Mutex::new(PlaybackSnapshot {
            state: PlaybackState::Paused,
            verifying_source: expected_fingerprint.is_some(),
            source_verified: false,
            position_pending: false,
            applied_position_request: 0,
            generation: 0,
            position_ms: 0,
            duration_ms: None,
            volume: 1.,
            speed: 1.,
            range: None,
            error: None,
            video_error: None,
            has_video: false,
        }));
        let shared = snapshot.clone();
        let video_frame = Arc::new(parking_lot::Mutex::new(None));
        let video_shared = video_frame.clone();
        let verification_cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = verification_cancelled.clone();
        let worker_fingerprint = expected_fingerprint.clone();
        let worker = thread::Builder::new()
            .name("opencap-player".into())
            .spawn(move || {
                let _meeting_activity = activity;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let verified_source = if worker_fingerprint.is_some() {
                            Some(VerifiedSource::capture(&path)?)
                        } else {
                            None
                        };
                        if let Some(expected) = &worker_fingerprint {
                            let actual =
                                opencap_core::fingerprint_media(&path, Some(&worker_cancelled))
                                    .context("无法核对转写原媒体")?;
                            ensure!(
                                &actual == expected,
                                "媒体内容与本次转写记录不一致，已停止回听"
                            );
                            if let Some(identity) = &verified_source {
                                identity.check(&path)?;
                            }
                            shared.lock().source_verified = true;
                        }
                        if worker_cancelled.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        shared.lock().verifying_source = false;
                        run_player(
                            path,
                            rx,
                            &shared,
                            &video_shared,
                            expected_audio_stream,
                            verified_source,
                            &worker_cancelled,
                        )
                    }));
                shared.lock().verifying_source = false;
                if worker_cancelled.load(Ordering::Relaxed) {
                    shared.lock().state = PlaybackState::Closed;
                } else {
                    let error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(format!("{error:#}")),
                        Err(_) => Some("播放线程异常退出，请重新打开媒体".to_owned()),
                    };
                    if let Some(error) = error {
                        let mut state = shared.lock();
                        state.state = PlaybackState::Failed;
                        state.error = Some(error);
                    }
                }
            })?;
        Ok(Self {
            commands,
            command_submission: parking_lot::Mutex::new(()),
            next_position_request: AtomicU64::new(0),
            submitted_position_request: AtomicU64::new(0),
            expected_audio_stream,
            expected_fingerprint,
            verification_cancelled,
            snapshot,
            worker: Some(worker),
            video_frame,
        })
    }
    pub fn command(&self, command: PlaybackCommand) -> Result<()> {
        // Keep request numbers in queue order even when callers submit concurrently.
        let _submission = self.command_submission.lock();
        if !matches!(command, PlaybackCommand::Close)
            && self.verification_cancelled.load(Ordering::Relaxed)
        {
            anyhow::bail!("播放器正在关闭");
        }
        if matches!(command, PlaybackCommand::Close) {
            self.verification_cancelled.store(true, Ordering::Release);
            // A failed worker has already dropped its receiver. Allow the UI
            // to dismiss the failed session instead of reporting a send error.
            if self.finished() {
                self.snapshot.lock().state = PlaybackState::Closed;
                return Ok(());
            }
            self.snapshot.lock().state = PlaybackState::Closing;
        }
        let position_request = if matches!(
            command,
            PlaybackCommand::Play
                | PlaybackCommand::Seek(_)
                | PlaybackCommand::PlayAt(_)
                | PlaybackCommand::PlayRange(_)
                | PlaybackCommand::SetRange(_)
                | PlaybackCommand::SetSpeed(_)
        ) {
            self.next_position_request
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        } else {
            0
        };
        match self.commands.try_send((command, position_request)) {
            Ok(()) => {
                if position_request != 0 {
                    self.submitted_position_request
                        .fetch_max(position_request, Ordering::Release);
                }
                Ok(())
            }
            Err(crossbeam_channel::TrySendError::Full(_))
                if matches!(command, PlaybackCommand::Close) =>
            {
                Ok(())
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_))
                if matches!(command, PlaybackCommand::Close) =>
            {
                // The receiver may already be gone while worker cleanup is
                // still joining decoders and releasing the output device.
                self.snapshot.lock().state = if self.finished() {
                    PlaybackState::Closed
                } else {
                    PlaybackState::Closing
                };
                Ok(())
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                anyhow::bail!("播放操作过于频繁，请稍后重试")
            }
            Err(error) => Err(error).context("播放线程已退出"),
        }
    }
    pub fn snapshot(&self) -> PlaybackSnapshot {
        let mut snapshot = self.snapshot.lock().clone();
        snapshot.position_pending = self.submitted_position_request.load(Ordering::Acquire)
            > snapshot.applied_position_request;
        if self.closing() {
            snapshot.state = PlaybackState::Closing;
        }
        snapshot
    }
    pub fn expected_audio_stream(&self) -> Option<u64> {
        self.expected_audio_stream
    }
    pub fn expected_fingerprint(&self) -> Option<&opencap_core::MediaFingerprint> {
        self.expected_fingerprint.as_ref()
    }
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn closing(&self) -> bool {
        self.verification_cancelled.load(Ordering::Relaxed) && !self.finished()
    }
    pub fn take_video_frame(&self) -> Option<PlaybackVideoFrame> {
        let frame = self.video_frame.lock().take();
        if self.closing() { None } else { frame }
    }
}
impl Drop for PlaybackSession {
    fn drop(&mut self) {
        self.verification_cancelled.store(true, Ordering::Release);
        let _ = self.commands.try_send((PlaybackCommand::Close, 0));
    }
}

struct AudioBlock {
    start_us: u64,
    samples: Vec<f32>,
    speed: f32,
}

/// Convert contiguous media PCM to pitch-preserving output with FFmpeg atempo.
/// The media position carried by each block is derived from the output sample
/// count and requested tempo; its accuracy must be measured against fixtures.
struct PcmProducer<'a> {
    blocks: &'a Sender<AudioBlock>,
    pending: &'a AtomicU64,
    cancelled: &'a AtomicBool,
    rate: u32,
    speed: f32,
    origin_us: u64,
    end_us: Option<u64>,
    input_frames: u64,
    output_frames: u64,
    filter: Option<ff::filter::Graph>,
}
impl<'a> PcmProducer<'a> {
    fn new(
        blocks: &'a Sender<AudioBlock>,
        pending: &'a AtomicU64,
        cancelled: &'a AtomicBool,
        rate: u32,
        speed: f32,
        origin_us: u64,
        end_us: Option<u64>,
    ) -> Result<Self> {
        let filter = if (speed - 1.).abs() < 0.001 {
            None
        } else {
            let mut graph = ff::filter::Graph::new();
            let source = ff::filter::find("abuffer").context("FFmpeg 缺少 abuffer")?;
            let sink = ff::filter::find("abuffersink").context("FFmpeg 缺少 abuffersink")?;
            graph.add(
                &source,
                "in",
                &format!(
                    "time_base=1/{rate}:sample_rate={rate}:sample_fmt=flt:channel_layout=mono"
                ),
            )?;
            graph.add(&sink, "out", "")?;
            {
                let mut out = graph.get("out").context("无法创建音频滤镜输出")?;
                out.set_sample_format(ff::format::Sample::F32(ff::format::sample::Type::Packed));
                out.set_channel_layout(ff::ChannelLayout::MONO);
                out.set_sample_rate(rate);
            }
            graph
                .output("in", 0)?
                .input("out", 0)?
                .parse(&format!("atempo={speed}"))?;
            graph.validate().context("无法初始化保音调倍速")?;
            Some(graph)
        };
        Ok(Self {
            blocks,
            pending,
            cancelled,
            rate,
            speed,
            origin_us,
            end_us,
            input_frames: 0,
            output_frames: 0,
            filter,
        })
    }
    fn send_blocks(&self, start_us: u64, samples: &[f32]) -> Result<()> {
        // atempo can emit a different number of samples than the ideal
        // duration/speed ratio. Keep its estimated media extent within the
        // selected end even when the filter flushes a slightly longer tail.
        let samples = if let Some(end) = self.end_us {
            if start_us >= end {
                return Ok(());
            }
            let frames = (((end - start_us) as f64 * f64::from(self.rate))
                / (1_000_000. * f64::from(self.speed)))
            .floor() as usize;
            &samples[..samples.len().min(frames)]
        } else {
            samples
        };
        for (index, chunk) in samples.chunks(2048).enumerate() {
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            let offset = ((index as f64 * 2048. * 1_000_000. * f64::from(self.speed))
                / f64::from(self.rate)) as u64;
            let mut block = AudioBlock {
                start_us: start_us.saturating_add(offset),
                samples: chunk.to_vec(),
                speed: self.speed,
            };
            loop {
                if self.cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                self.pending
                    .fetch_add(block.samples.len() as u64, Ordering::AcqRel);
                match self.blocks.send_timeout(block, Duration::from_millis(50)) {
                    Ok(()) => break,
                    Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                        self.pending
                            .fetch_sub(returned.samples.len() as u64, Ordering::AcqRel);
                        block = returned;
                    }
                    Err(crossbeam_channel::SendTimeoutError::Disconnected(returned)) => {
                        self.pending
                            .fetch_sub(returned.samples.len() as u64, Ordering::AcqRel);
                        anyhow::bail!("音频输出已关闭");
                    }
                }
            }
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        loop {
            let mut output = ff::frame::Audio::empty();
            let result = self
                .filter
                .as_mut()
                .context("倍速滤镜未初始化")?
                .get("out")
                .context("倍速滤镜输出丢失")?
                .sink()
                .frame(&mut output);
            match result {
                Ok(()) => {
                    let samples = &output.plane::<f32>(0)[..output.samples()];
                    let start = self.origin_us.saturating_add(
                        ((self.output_frames as f64 * 1_000_000. * f64::from(self.speed))
                            / f64::from(self.rate)) as u64,
                    );
                    self.send_blocks(start, samples)?;
                    self.output_frames = self.output_frames.saturating_add(samples.len() as u64);
                }
                Err(ff::Error::Eof)
                | Err(ff::Error::Other {
                    errno: ff::error::EAGAIN,
                }) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn push(&mut self, start_us: i64, samples: &[f32]) -> Result<()> {
        if samples.is_empty() || self.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.filter.is_none() {
            return self.send_blocks(start_us.max(0) as u64, samples);
        }
        let mut input = ff::frame::Audio::new(
            ff::format::Sample::F32(ff::format::sample::Type::Packed),
            samples.len(),
            ff::ChannelLayout::MONO,
        );
        input.set_rate(self.rate);
        input.set_pts(Some(self.input_frames.min(i64::MAX as u64) as i64));
        input.plane_mut::<f32>(0)[..samples.len()].copy_from_slice(samples);
        self.input_frames = self.input_frames.saturating_add(samples.len() as u64);
        self.filter
            .as_mut()
            .context("倍速滤镜未初始化")?
            .get("in")
            .context("倍速滤镜输入丢失")?
            .source()
            .add(&input)?;
        self.drain()
    }
    fn finish(&mut self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) || self.filter.is_none() {
            return Ok(());
        }
        self.filter
            .as_mut()
            .context("倍速滤镜未初始化")?
            .get("in")
            .context("倍速滤镜输入丢失")?
            .source()
            .flush()?;
        self.drain()
    }
}
struct Played {
    generation: u64,
    at: Instant,
    media_start_us: u64,
    frames: u64,
    rate: u32,
    media_duration_us: u64,
}
struct Output {
    stream: Option<cpal::Stream>,
    cancel: Arc<AtomicBool>,
    decoder: Option<JoinHandle<()>>,
    done: Option<Receiver<Result<()>>>,
    presented: Option<Receiver<Played>>,
    pending_frames: Arc<AtomicU64>,
    timing_fault: Arc<AtomicBool>,
    video: Option<VideoOutput>,
    video_clock: Option<(Instant, u64)>,
    video_started: bool,
    speed: f32,
}
enum VideoMessage {
    Frame(PlaybackVideoFrame),
    Failed(String),
    Done,
}
struct VideoOutput {
    frames: Receiver<VideoMessage>,
    worker: JoinHandle<()>,
    next: Option<PlaybackVideoFrame>,
    done: bool,
    last_pts: Option<u64>,
}
impl Output {
    fn close(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        self.cancel.store(true, Ordering::Release);
        drop(self.stream.take());
        if let Some(video) = self.video.take() {
            drop(video.frames);
            let _ = video.worker.join();
        }
        // Called only on the playback worker; never join a decoder on the UI thread.
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn send_video_message(
    frames: &Sender<VideoMessage>,
    mut message: VideoMessage,
    cancelled: &AtomicBool,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        match frames.send_timeout(message, Duration::from_millis(50)) {
            Ok(()) => return Ok(()),
            Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => message = returned,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                anyhow::bail!("视频输出已关闭")
            }
        }
    }
}
fn start_video(
    path: PathBuf,
    target_ms: u64,
    generation: u64,
    cancel: Arc<AtomicBool>,
) -> Result<VideoOutput> {
    let (tx, frames) = bounded(8);
    let worker = thread::Builder::new()
        .name("opencap-player-video".into())
        .spawn(move || {
            let result = decode_video(&path, target_ms, generation, &tx, &cancel);
            let message = match result {
                Ok(()) => VideoMessage::Done,
                Err(error) => VideoMessage::Failed(format!("{error:#}")),
            };
            let _ = send_video_message(&tx, message, &cancel);
        })?;
    Ok(VideoOutput {
        frames,
        worker,
        next: None,
        done: false,
        last_pts: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn start_output(
    path: PathBuf,
    target_ms: u64,
    range: Option<PlaybackRange>,
    generation: u64,
    volume: Arc<AtomicU32>,
    speed: f32,
    expected_audio_stream: Option<u64>,
    verified_source: Option<&VerifiedSource>,
    close_requested: &AtomicBool,
) -> Result<(Output, Option<u64>)> {
    ensure!(!close_requested.load(Ordering::Acquire), "播放器正在关闭");
    ff::init()?;
    if let Some(identity) = verified_source {
        identity.check(&path)?;
    }
    let probe = ff::format::input(&path).context("无法读取原媒体")?;
    ensure!(!close_requested.load(Ordering::Acquire), "播放器正在关闭");
    if let Some(identity) = verified_source {
        identity.check(&path)?;
    }
    let has_audio = probe.streams().best(ff::media::Type::Audio).is_some();
    if let Some(expected) = expected_audio_stream {
        let expected = usize::try_from(expected).context("转写音轨编号超出平台范围")?;
        ensure!(
            probe.streams().any(|stream| stream.index() == expected
                && stream.parameters().medium() == ff::media::Type::Audio),
            "转写记录的音轨不存在或不是音频，无法可靠回听"
        );
    }
    let has_video = probe.streams().best(ff::media::Type::Video).is_some();
    ensure!(has_audio || has_video, "媒体没有可播放的音轨或视频轨");
    let duration = (probe.duration() > 0).then_some((probe.duration() / 1000) as u64);
    drop(probe);
    let cancel = Arc::new(AtomicBool::new(false));
    if !has_audio {
        let video = Some(start_video(path, target_ms, generation, cancel.clone())?);
        return Ok((
            Output {
                stream: None,
                cancel,
                decoder: None,
                done: None,
                presented: None,
                pending_frames: Arc::new(AtomicU64::new(0)),
                timing_fault: Arc::new(AtomicBool::new(false)),
                video,
                video_clock: Some((Instant::now(), target_ms)),
                video_started: false,
                speed,
            },
            duration,
        ));
    }
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("没有可用的音频输出设备")?;
    let default = device.default_output_config().ok();
    let supported_pcm = |format: cpal::SampleFormat| {
        matches!(
            format,
            cpal::SampleFormat::I8
                | cpal::SampleFormat::U8
                | cpal::SampleFormat::I16
                | cpal::SampleFormat::U16
                | cpal::SampleFormat::I24
                | cpal::SampleFormat::U24
                | cpal::SampleFormat::I32
                | cpal::SampleFormat::U32
                | cpal::SampleFormat::I64
                | cpal::SampleFormat::U64
                | cpal::SampleFormat::F32
                | cpal::SampleFormat::F64
        )
    };
    let supported = if let Some(default) = default.filter(|config| {
        supported_pcm(config.sample_format()) && config.sample_rate() > 0 && config.channels() > 0
    }) {
        default
    } else {
        let preferred_rate = default
            .map(|config| config.sample_rate())
            .filter(|rate| *rate > 0)
            .unwrap_or(48_000);
        let preferred_channels = default
            .map(|config| config.channels())
            .filter(|channels| *channels > 0)
            .unwrap_or(2);
        let range = device
            .supported_output_configs()
            .context("无法枚举输出设备格式")?
            .filter(|range| {
                supported_pcm(range.sample_format())
                    && range.channels() > 0
                    && range.min_sample_rate() > 0
                    && range.min_sample_rate() <= range.max_sample_rate()
            })
            .max_by_key(|range| {
                let rate = preferred_rate.clamp(range.min_sample_rate(), range.max_sample_rate());
                (
                    range.channels() == preferred_channels,
                    range.sample_format() == cpal::SampleFormat::F32,
                    std::cmp::Reverse(preferred_rate.abs_diff(rate)),
                )
            })
            .context("默认输出设备没有可用的 PCM 格式")?;
        let rate = preferred_rate.clamp(range.min_sample_rate(), range.max_sample_rate());
        range
            .try_with_sample_rate(rate)
            .context("无法选择输出设备采样率")?
    };
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;
    ensure!(rate > 0 && config.channels > 0, "输出设备格式无效");
    let (blocks, block_rx) = bounded::<AudioBlock>(64);
    let (present_tx, presented) = bounded::<Played>(512);
    let (done_tx, done) = bounded(1);
    let pending_frames = Arc::new(AtomicU64::new(0));
    let timing_fault = Arc::new(AtomicBool::new(false));
    let output_error = timing_fault.clone();
    macro_rules! output_stream {
        ($sample:ty) => {
            make_stream::<$sample>(
                &device,
                config,
                block_rx,
                present_tx,
                generation,
                pending_frames.clone(),
                timing_fault.clone(),
                volume,
                output_error,
            )?
        };
    }
    let stream = match sample_format {
        cpal::SampleFormat::I8 => output_stream!(i8),
        cpal::SampleFormat::U8 => output_stream!(u8),
        cpal::SampleFormat::I24 => output_stream!(cpal::I24),
        cpal::SampleFormat::U24 => output_stream!(cpal::U24),
        cpal::SampleFormat::I32 => output_stream!(i32),
        cpal::SampleFormat::U32 => output_stream!(u32),
        cpal::SampleFormat::I64 => output_stream!(i64),
        cpal::SampleFormat::U64 => output_stream!(u64),
        cpal::SampleFormat::F64 => output_stream!(f64),
        cpal::SampleFormat::F32 => make_stream::<f32>(
            &device,
            config,
            block_rx,
            present_tx,
            generation,
            pending_frames.clone(),
            timing_fault.clone(),
            volume,
            output_error,
        )?,
        cpal::SampleFormat::I16 => make_stream::<i16>(
            &device,
            config,
            block_rx,
            present_tx,
            generation,
            pending_frames.clone(),
            timing_fault.clone(),
            volume,
            output_error,
        )?,
        cpal::SampleFormat::U16 => make_stream::<u16>(
            &device,
            config,
            block_rx,
            present_tx,
            generation,
            pending_frames.clone(),
            timing_fault.clone(),
            volume,
            output_error,
        )?,
        other => anyhow::bail!("暂不支持输出设备采样格式 {other:?}"),
    };
    let decoder_cancel = cancel.clone();
    let pending = pending_frames.clone();
    let video_path = path.clone();
    let decoder = thread::Builder::new()
        .name("opencap-player-decode".into())
        .spawn(move || {
            let result = decode_audio(
                &path,
                target_ms,
                range,
                rate,
                speed,
                &blocks,
                &pending,
                &decoder_cancel,
                expected_audio_stream,
            );
            let _ = done_tx.send(result);
        })?;
    let video = if has_video {
        match start_video(video_path, target_ms, generation, cancel.clone()) {
            Ok(video) => Some(video),
            Err(error) => {
                cancel.store(true, Ordering::Release);
                drop(stream);
                let _ = decoder.join();
                return Err(error);
            }
        }
    } else {
        None
    };
    let play_result = if close_requested.load(Ordering::Acquire) {
        Err(anyhow::anyhow!("播放器正在关闭"))
    } else {
        stream.play().map_err(anyhow::Error::from)
    };
    if let Err(error) = play_result {
        cancel.store(true, Ordering::Release);
        drop(stream);
        if let Some(video) = video {
            drop(video.frames);
            let _ = video.worker.join();
        }
        let _ = decoder.join();
        return Err(error);
    }
    Ok((
        Output {
            stream: Some(stream),
            cancel,
            decoder: Some(decoder),
            done: Some(done),
            presented: Some(presented),
            pending_frames,
            timing_fault,
            video,
            video_clock: None,
            video_started: false,
            speed,
        },
        duration,
    ))
}

#[allow(clippy::too_many_arguments)]
fn make_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    blocks: Receiver<AudioBlock>,
    presented: Sender<Played>,
    generation: u64,
    pending: Arc<AtomicU64>,
    fault: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    output_error: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let mut current: Option<AudioBlock> = None;
    let mut offset = 0usize;
    let stream = device.build_output_stream(
        config,
        move |output: &mut [T], info: &cpal::OutputCallbackInfo| {
            let timestamp = info.timestamp();
            let callback = Instant::now();
            let device_delay = timestamp
                .playback
                .saturating_duration_since(timestamp.callback);
            let predicted = callback.checked_add(device_delay).unwrap_or(callback);
            let frames = output.len() / channels;
            if !output.len().is_multiple_of(channels) {
                for sample in &mut output[frames * channels..] {
                    *sample = T::from_sample(0.);
                }
                fault.store(true, Ordering::Release);
            }
            let mut written = 0usize;
            while written < frames {
                if current
                    .as_ref()
                    .is_none_or(|block| offset >= block.samples.len())
                {
                    current = blocks.try_recv().ok();
                    offset = 0;
                }
                let Some(block) = current.as_ref() else {
                    for sample in &mut output[written * channels..] {
                        *sample = T::from_sample(0.);
                    }
                    break;
                };
                let count = (block.samples.len() - offset).min(frames - written);
                let gain = f32::from_bits(volume.load(Ordering::Relaxed)).clamp(0., 1.);
                for (frame, sample) in output[written * channels..(written + count) * channels]
                    .chunks_exact_mut(channels)
                    .zip(&block.samples[offset..offset + count])
                {
                    let scaled = sample * gain;
                    let value = if scaled.is_finite() {
                        // dasp's float-to-integer conversion expects [-1, 1);
                        // +1 can overflow packed 24-bit sample types.
                        let below_one = f32::from_bits(1.0f32.to_bits() - 1);
                        T::from_sample(scaled.clamp(-1., below_one))
                    } else {
                        // Never send NaN/Inf to a device or an integer sample
                        // conversion. The worker observes the fault and stops.
                        fault.store(true, Ordering::Release);
                        T::from_sample(0.)
                    };
                    for channel in frame {
                        *channel = value;
                    }
                }
                let at = predicted
                    .checked_add(Duration::from_secs_f64(written as f64 / f64::from(rate)))
                    .unwrap_or(predicted);
                let media_start_us = block.start_us.saturating_add(
                    ((offset as f64 * 1_000_000. * f64::from(block.speed)) / f64::from(rate))
                        as u64,
                );
                if presented
                    .try_send(Played {
                        generation,
                        at,
                        media_start_us,
                        frames: count as u64,
                        rate,
                        media_duration_us: ((count as f64 * 1_000_000. * f64::from(block.speed))
                            / f64::from(rate)) as u64,
                    })
                    .is_err()
                {
                    fault.store(true, Ordering::Release);
                }
                pending.fetch_sub(count as u64, Ordering::AcqRel);
                offset += count;
                written += count;
            }
        },
        move |_error| {
            output_error.store(true, Ordering::Release);
        },
        None,
    )?;
    Ok(stream)
}

fn publish_clock(output: &Output, clock: &mut PresentationClock) -> Result<()> {
    ensure!(
        !output.timing_fault.load(Ordering::Acquire),
        "输出设备或呈现时钟异常，播放已停止"
    );
    let Some(presented) = &output.presented else {
        return Ok(());
    };
    for event in presented.try_iter() {
        let duration = event.media_duration_us;
        if duration > 0 {
            clock.schedule(
                event.generation,
                event.at,
                event.media_start_us,
                duration,
                event.frames,
                event.rate,
            )?;
        }
    }
    Ok(())
}

fn run_player(
    path: PathBuf,
    commands: Receiver<(PlaybackCommand, u64)>,
    shared: &parking_lot::Mutex<PlaybackSnapshot>,
    latest_video: &parking_lot::Mutex<Option<PlaybackVideoFrame>>,
    expected_audio_stream: Option<u64>,
    verified_source: Option<VerifiedSource>,
    close_requested: &AtomicBool,
) -> Result<()> {
    let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let mut clock = PresentationClock::new();
    let mut output: Option<Output> = None;
    let mut range: Option<PlaybackRange> = None;
    let mut speed = 1.0f32;
    let mut decode_done = false;
    let mut position = 0u64;
    let mut generation = clock.reset(0);
    let mut deferred_command = None;
    loop {
        if close_requested.load(Ordering::Acquire) {
            shared.lock().state = PlaybackState::Closing;
            latest_video.lock().take();
            if let Some(active) = output.take() {
                active.close();
            }
            shared.lock().state = PlaybackState::Closed;
            return Ok(());
        }
        if let Some(active) = output.as_mut() {
            let video_only = active.video_clock.is_some();
            let mut pending = 0;
            let mut scheduled = false;
            if let Some((started, base)) = active.video_clock {
                position = if active.video_started {
                    base.saturating_add(
                        (started.elapsed().as_secs_f64() * 1000. * f64::from(active.speed)) as u64,
                    )
                } else {
                    base
                };
            } else {
                publish_clock(active, &mut clock)?;
                position = clock.position_ms(Instant::now());
                if let Some(done) = &active.done
                    && let Ok(result) = done.try_recv()
                {
                    result?;
                    decode_done = true;
                }
                pending = active.pending_frames.load(Ordering::Acquire);
                if pending == 0 {
                    // Events are published before pending is decremented.
                    publish_clock(active, &mut clock)?;
                    position = clock.position_ms(Instant::now());
                }
                scheduled = clock.has_pending(Instant::now());
            }
            let mut saw_frame = false;
            if let Some(video) = &mut active.video {
                loop {
                    if video.next.is_none() {
                        match video.frames.try_recv() {
                            Ok(VideoMessage::Frame(frame)) => {
                                video.next = Some(frame);
                                saw_frame = true;
                            }
                            Ok(VideoMessage::Failed(error)) => {
                                if video_only {
                                    anyhow::bail!("视频解码失败：{error}");
                                }
                                shared.lock().video_error = Some(error);
                                video.done = true;
                                break;
                            }
                            Ok(VideoMessage::Done) => {
                                video.done = true;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if video.next.as_ref().is_some_and(|frame| {
                        frame.pts_ms <= position && frame.generation == generation
                    }) {
                        video.last_pts = video.next.as_ref().map(|frame| frame.pts_ms);
                        *latest_video.lock() = video.next.take();
                    } else {
                        break;
                    }
                }
            }
            if video_only && !active.video_started && saw_frame {
                active.video_started = true;
                active.video_clock = Some((Instant::now(), position));
            }
            if video_only
                && active.video.as_ref().is_some_and(|video| {
                    video.done && video.last_pts.is_none() && video.next.is_none()
                })
            {
                anyhow::bail!("视频没有可显示的帧");
            }
            let limit = match (range.map(|r| r.end_ms), shared.lock().duration_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, b) => b,
            };
            if video_only && let Some(limit) = limit {
                position = position.min(limit);
            }
            let video_complete = video_only
                && (limit.is_some_and(|limit| position >= limit)
                    || (limit.is_none()
                        && active.video.as_ref().is_some_and(|video| {
                            video.done
                                && video.next.is_none()
                                && video.last_pts.is_none_or(|last| position >= last)
                        })));
            let playing = if video_only {
                !video_complete
            } else {
                pending > 0 || scheduled
            };
            {
                let mut state = shared.lock();
                state.position_ms = position;
                state.state = if scheduled || (video_only && active.video_started) {
                    PlaybackState::Playing
                } else {
                    PlaybackState::Buffering
                };
            }
            if (decode_done && !playing) || video_complete {
                if let Some(active) = output.take() {
                    active.close();
                }
                if let Some(selected) = range.filter(|r| r.repeat) {
                    position = selected.start_ms;
                    generation = clock.reset(position);
                    latest_video.lock().take();
                    let (next, duration) = start_output(
                        path.clone(),
                        position,
                        range,
                        generation,
                        volume.clone(),
                        speed,
                        expected_audio_stream,
                        verified_source.as_ref(),
                        close_requested,
                    )?;
                    let has_video = next.video.is_some();
                    output = Some(next);
                    decode_done = false;
                    let mut state = shared.lock();
                    state.generation = generation;
                    state.duration_ms = duration;
                    state.has_video = has_video;
                    state.position_ms = position;
                } else {
                    let mut state = shared.lock();
                    state.state = PlaybackState::Ended;
                    state.position_ms = position;
                }
            }
        }
        let received = if let Some(command) = deferred_command.take() {
            Ok(command)
        } else {
            commands.recv_timeout(Duration::from_millis(20))
        };
        // Coalesce only adjacent commands with identical semantics. Other
        // commands remain ordered and act as barriers.
        let received = match received {
            Ok((PlaybackCommand::Seek(mut target), mut request)) => {
                loop {
                    match commands.try_recv() {
                        Ok((PlaybackCommand::Seek(next), next_request)) => {
                            target = next;
                            request = next_request;
                        }
                        Ok(other) => {
                            deferred_command = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                Ok((PlaybackCommand::Seek(target), request))
            }
            Ok((PlaybackCommand::PlayAt(mut target), mut request)) => {
                loop {
                    match commands.try_recv() {
                        Ok((PlaybackCommand::PlayAt(next), next_request)) => {
                            target = next;
                            request = next_request;
                        }
                        Ok(other) => {
                            deferred_command = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                Ok((PlaybackCommand::PlayAt(target), request))
            }
            Ok((PlaybackCommand::PlayRange(mut selected), mut request)) => {
                loop {
                    match commands.try_recv() {
                        Ok((PlaybackCommand::PlayRange(next), next_request)) => {
                            selected = next;
                            request = next_request;
                        }
                        Ok(other) => {
                            deferred_command = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                Ok((PlaybackCommand::PlayRange(selected), request))
            }
            other => other,
        };
        if close_requested.load(Ordering::Acquire) {
            continue;
        }
        let (received, position_request) = match received {
            Ok((command, request)) => (Ok(command), request),
            Err(error) => (Err(error), 0),
        };
        match received {
            Ok(PlaybackCommand::PlayAt(target)) => {
                if let Some(active) = output.take() {
                    active.close();
                }
                range = None;
                position = shared
                    .lock()
                    .duration_ms
                    .map_or(target, |duration| target.min(duration));
                generation = clock.reset(position);
                latest_video.lock().take();
                let (next, duration) = start_output(
                    path.clone(),
                    position,
                    range,
                    generation,
                    volume.clone(),
                    speed,
                    expected_audio_stream,
                    verified_source.as_ref(),
                    close_requested,
                )?;
                let has_video = next.video.is_some();
                output = Some(next);
                decode_done = false;
                let mut state = shared.lock();
                state.range = None;
                state.position_ms = position;
                state.generation = generation;
                state.duration_ms = duration;
                state.has_video = has_video;
                state.state = PlaybackState::Buffering;
                state.error = None;
                state.video_error = None;
            }
            Ok(PlaybackCommand::PlayRange(selected)) => {
                if let Some(active) = output.take() {
                    active.close();
                }
                range = Some(selected);
                position = selected.start_ms;
                generation = clock.reset(position);
                latest_video.lock().take();
                let (next, duration) = start_output(
                    path.clone(),
                    position,
                    range,
                    generation,
                    volume.clone(),
                    speed,
                    expected_audio_stream,
                    verified_source.as_ref(),
                    close_requested,
                )?;
                let has_video = next.video.is_some();
                output = Some(next);
                decode_done = false;
                let mut state = shared.lock();
                state.range = range;
                state.position_ms = position;
                state.generation = generation;
                state.duration_ms = duration;
                state.has_video = has_video;
                state.state = PlaybackState::Buffering;
                state.error = None;
                state.video_error = None;
            }
            Ok(PlaybackCommand::Play) if output.is_none() => {
                if shared.lock().state == PlaybackState::Ended {
                    position = range.map_or(0, |selected| selected.start_ms);
                }
                if let Some(selected) = range
                    && (position < selected.start_ms || position >= selected.end_ms)
                {
                    position = selected.start_ms;
                }
                generation = clock.reset(position);
                latest_video.lock().take();
                let (next, duration) = start_output(
                    path.clone(),
                    position,
                    range,
                    generation,
                    volume.clone(),
                    speed,
                    expected_audio_stream,
                    verified_source.as_ref(),
                    close_requested,
                )?;
                let has_video = next.video.is_some();
                output = Some(next);
                decode_done = false;
                let mut state = shared.lock();
                state.generation = generation;
                state.state = PlaybackState::Buffering;
                state.duration_ms = duration;
                state.has_video = has_video;
                state.error = None;
                state.video_error = None;
            }
            Ok(PlaybackCommand::Pause) => {
                if let Some(active) = output.take() {
                    if let Some((started, base)) = active.video_clock {
                        position = if active.video_started {
                            base.saturating_add(
                                (started.elapsed().as_secs_f64() * 1000. * f64::from(active.speed))
                                    as u64,
                            )
                        } else {
                            base
                        };
                    } else {
                        publish_clock(&active, &mut clock)?;
                        position = clock.position_ms(Instant::now());
                    }
                    active.close();
                    generation = clock.reset(position);
                }
                let mut state = shared.lock();
                state.generation = generation;
                state.position_ms = position;
                state.state = PlaybackState::Paused;
            }
            Ok(PlaybackCommand::Seek(target)) => {
                let resume = output.is_some();
                if let Some(active) = output.take() {
                    active.close();
                }
                position = shared
                    .lock()
                    .duration_ms
                    .map_or(target, |duration| target.min(duration));
                generation = clock.reset(position);
                latest_video.lock().take();
                if resume {
                    let (next, duration) = start_output(
                        path.clone(),
                        position,
                        range,
                        generation,
                        volume.clone(),
                        speed,
                        expected_audio_stream,
                        verified_source.as_ref(),
                        close_requested,
                    )?;
                    let has_video = next.video.is_some();
                    output = Some(next);
                    decode_done = false;
                    shared.lock().duration_ms = duration;
                    shared.lock().has_video = has_video;
                }
                let mut state = shared.lock();
                state.generation = generation;
                state.position_ms = position;
                state.state = if resume {
                    PlaybackState::Buffering
                } else {
                    PlaybackState::Paused
                };
            }
            Ok(PlaybackCommand::SetVolume(value)) => {
                if value.is_finite() {
                    let value = value.clamp(0., 1.);
                    volume.store(value.to_bits(), Ordering::Relaxed);
                    shared.lock().volume = value;
                }
            }
            Ok(PlaybackCommand::SetSpeed(value)) => {
                if value.is_finite()
                    && (0.5..=2.0).contains(&value)
                    && (value - speed).abs() > 0.001
                {
                    let resume = output.is_some();
                    if let Some(active) = output.take() {
                        active.close();
                    }
                    speed = value;
                    generation = clock.reset(position);
                    latest_video.lock().take();
                    if resume {
                        let (next, duration) = start_output(
                            path.clone(),
                            position,
                            range,
                            generation,
                            volume.clone(),
                            speed,
                            expected_audio_stream,
                            verified_source.as_ref(),
                            close_requested,
                        )?;
                        let has_video = next.video.is_some();
                        output = Some(next);
                        decode_done = false;
                        let mut state = shared.lock();
                        state.duration_ms = duration;
                        state.has_video = has_video;
                    }
                    let mut state = shared.lock();
                    state.speed = speed;
                    state.generation = generation;
                    state.position_ms = position;
                    state.state = if resume {
                        PlaybackState::Buffering
                    } else {
                        PlaybackState::Paused
                    };
                }
            }
            Ok(PlaybackCommand::SetRange(value)) => {
                range = value;
                shared.lock().range = range;
                let resume = output.is_some();
                if let Some(active) = output.take() {
                    active.close();
                }
                if let Some(selected) = range
                    && (position < selected.start_ms || position >= selected.end_ms)
                {
                    position = selected.start_ms;
                }
                generation = clock.reset(position);
                latest_video.lock().take();
                if resume {
                    let (next, duration) = start_output(
                        path.clone(),
                        position,
                        range,
                        generation,
                        volume.clone(),
                        speed,
                        expected_audio_stream,
                        verified_source.as_ref(),
                        close_requested,
                    )?;
                    let has_video = next.video.is_some();
                    output = Some(next);
                    decode_done = false;
                    shared.lock().duration_ms = duration;
                    shared.lock().has_video = has_video;
                }
                let mut state = shared.lock();
                state.generation = generation;
                state.position_ms = position;
                state.state = if resume {
                    PlaybackState::Buffering
                } else {
                    PlaybackState::Paused
                };
            }
            Ok(PlaybackCommand::Close) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                shared.lock().state = PlaybackState::Closing;
                latest_video.lock().take();
                if let Some(active) = output.take() {
                    active.close();
                }
                shared.lock().state = PlaybackState::Closed;
                return Ok(());
            }
            Ok(PlaybackCommand::Play) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
        }
        if position_request != 0 {
            let mut state = shared.lock();
            state.applied_position_request = state.applied_position_request.max(position_request);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_audio(
    path: &std::path::Path,
    target_ms: u64,
    range: Option<PlaybackRange>,
    rate: u32,
    speed: f32,
    blocks: &Sender<AudioBlock>,
    pending: &AtomicU64,
    cancelled: &AtomicBool,
    expected_audio_stream: Option<u64>,
) -> Result<()> {
    ff::init()?;
    let mut input = ff::format::input(path)?;
    let duration_us = (input.duration() > 0).then_some(input.duration());
    let origin = unsafe { (*input.as_ptr()).start_time };
    let origin = if origin == ff::ffi::AV_NOPTS_VALUE {
        0
    } else {
        origin
    };
    let stream = if let Some(expected) = expected_audio_stream {
        let expected = usize::try_from(expected).context("转写音轨编号超出平台范围")?;
        input
            .streams()
            .find(|stream| {
                stream.index() == expected && stream.parameters().medium() == ff::media::Type::Audio
            })
            .context("转写记录的音轨不存在或不是音频")?
    } else {
        input
            .streams()
            .best(ff::media::Type::Audio)
            .context("文件没有音轨")?
    };
    let index = stream.index();
    if let Some(expected) = expected_audio_stream {
        ensure!(
            u64::try_from(index).ok() == Some(expected),
            "播放音轨已变化，无法可靠回听转写来源"
        );
    }
    let tb = stream.time_base();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    let layout = if decoder.channel_layout().is_empty() {
        ff::ChannelLayout::default(decoder.channels() as i32)
    } else {
        decoder.channel_layout()
    };
    let input_rate = decoder.rate();
    ensure!(input_rate > 0, "音轨采样率无效");
    let mut resampler = ff::software::resampling::Context::get(
        decoder.format(),
        layout,
        input_rate,
        ff::format::Sample::F32(ff::format::sample::Type::Packed),
        ff::ChannelLayout::MONO,
        rate,
    )?;
    let target_us = target_ms.saturating_mul(1000).min(i64::MAX as u64) as i64;
    let selected_end = range.map(|r| r.end_ms.saturating_mul(1000).min(i64::MAX as u64) as i64);
    let stop_us = match (selected_end, duration_us) {
        (Some(selected), Some(duration)) => Some(selected.min(duration)),
        (Some(selected), None) => Some(selected),
        (None, duration) => duration,
    };
    if target_us > 0 {
        let seek = origin.saturating_add(target_us);
        input.seek(seek, ..seek).context("无法定位媒体")?;
        decoder.flush();
    }
    let mut cursor = target_us;
    let mut started = false;
    let reached_end = std::cell::Cell::new(false);
    let mut producer = PcmProducer::new(
        blocks,
        pending,
        cancelled,
        rate,
        speed,
        target_us.max(0) as u64,
        stop_us.map(|stop| stop.max(0) as u64),
    )?;
    let mut emit = |start_us: i64, samples: &[f32]| producer.push(start_us, samples);
    let mut consume = |frame: &mut ff::frame::Audio,
                       resampler: &mut ff::software::resampling::Context|
     -> Result<()> {
        if frame.channel_layout().is_empty() {
            frame.set_channel_layout(layout);
        }
        let timestamp = frame
            .timestamp()
            .map(|pts| ff::Rescale::rescale(&pts, tb, (1, 1_000_000)).saturating_sub(origin));
        if !started {
            ensure!(timestamp.is_some(), "音轨缺少定位所需的媒体时间戳");
        }
        let delayed = resampler
            .delay()
            .map_or(0, |delay| delay.output.max(0) * 1_000_000 / i64::from(rate));
        let mut at = timestamp.unwrap_or(cursor).saturating_sub(delayed);
        let capacity =
            (frame.samples() as u64 * u64::from(rate) / u64::from(input_rate) + 256) as usize;
        let mut converted = ff::frame::Audio::new(
            ff::format::Sample::F32(ff::format::sample::Type::Packed),
            capacity,
            ff::ChannelLayout::MONO,
        );
        resampler.run(frame, &mut converted)?;
        let mut samples = &converted.plane::<f32>(0)[..converted.samples()];
        if !started {
            started = true;
        }
        if at < cursor {
            let skip = (((cursor - at) as u64)
                .saturating_mul(u64::from(rate))
                .saturating_add(999_999)
                / 1_000_000) as usize;
            let skip = skip.min(samples.len());
            samples = &samples[skip..];
            at = at.saturating_add(skip as i64 * 1_000_000 / i64::from(rate));
        }
        if at > cursor + 2_000 {
            let gap_end = stop_us.map_or(at, |stop| at.min(stop));
            let mut gap = (gap_end.saturating_sub(cursor).max(0) as u64)
                .saturating_mul(u64::from(rate))
                / 1_000_000;
            let zero = [0f32; 2048];
            while gap > 0 && !cancelled.load(Ordering::Acquire) {
                let count = gap.min(zero.len() as u64) as usize;
                emit(cursor, &zero[..count])?;
                cursor = cursor.saturating_add(count as i64 * 1_000_000 / i64::from(rate));
                gap -= count as u64;
            }
            if stop_us.is_some_and(|stop| at >= stop) {
                reached_end.set(true);
                return Ok(());
            }
        }
        at = cursor.max(at);
        if let Some(stop) = stop_us {
            if at >= stop {
                reached_end.set(true);
                return Ok(());
            }
            let max = ((stop - at) as u64).saturating_mul(u64::from(rate)) / 1_000_000;
            if samples.len() as u64 > max {
                samples = &samples[..max as usize];
                reached_end.set(true);
            }
        }
        emit(at, samples)?;
        cursor = at.saturating_add(samples.len() as i64 * 1_000_000 / i64::from(rate));
        Ok(())
    };
    loop {
        if cancelled.load(Ordering::Acquire) || reached_end.get() {
            break;
        }
        let mut packet = ff::Packet::empty();
        let mut input_eof = false;
        match packet.read(&mut input) {
            Ok(()) if packet.stream() == index => decoder.send_packet(&packet)?,
            Ok(()) => continue,
            Err(ff::Error::Eof) => {
                decoder.send_eof()?;
                input_eof = true;
            }
            Err(error) => return Err(error.into()),
        }
        loop {
            let mut frame = ff::frame::Audio::empty();
            match decoder.receive_frame(&mut frame) {
                Ok(()) => consume(&mut frame, &mut resampler)?,
                Err(ff::Error::Eof) => break,
                Err(ff::Error::Other {
                    errno: ff::error::EAGAIN,
                }) => break,
                Err(error) => return Err(error.into()),
            }
            if cancelled.load(Ordering::Acquire) || reached_end.get() {
                break;
            }
        }
        if input_eof {
            break;
        }
    }
    if !cancelled.load(Ordering::Acquire) && !reached_end.get() {
        loop {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            let mut frame = ff::frame::Audio::new(
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                4096,
                ff::ChannelLayout::MONO,
            );
            resampler.flush(&mut frame)?;
            let mut samples = &frame.plane::<f32>(0)[..frame.samples()];
            if samples.is_empty() {
                break;
            }
            if let Some(stop) = stop_us {
                let remaining = (stop.saturating_sub(cursor).max(0) as u64)
                    .saturating_mul(u64::from(rate))
                    / 1_000_000;
                samples = &samples[..samples.len().min(remaining as usize)];
            }
            if samples.is_empty() {
                break;
            }
            emit(cursor, samples)?;
            cursor = cursor.saturating_add(samples.len() as i64 * 1_000_000 / i64::from(rate));
        }
        if let Some(stop) = stop_us {
            let mut gap = (stop.saturating_sub(cursor).max(0) as u64)
                .saturating_mul(u64::from(rate))
                / 1_000_000;
            let zero = [0f32; 2048];
            while gap > 0 && !cancelled.load(Ordering::Acquire) {
                let count = gap.min(zero.len() as u64) as usize;
                emit(cursor, &zero[..count])?;
                cursor = cursor.saturating_add(count as i64 * 1_000_000 / i64::from(rate));
                gap -= count as u64;
            }
        }
    }
    producer.finish()?;
    Ok(())
}

fn decode_video(
    path: &std::path::Path,
    target_ms: u64,
    generation: u64,
    frames: &Sender<VideoMessage>,
    cancelled: &AtomicBool,
) -> Result<()> {
    ff::init()?;
    let mut input = ff::format::input(path)?;
    let origin = unsafe { (*input.as_ptr()).start_time };
    let origin = if origin == ff::ffi::AV_NOPTS_VALUE {
        0
    } else {
        origin
    };
    let stream = input
        .streams()
        .best(ff::media::Type::Video)
        .context("媒体没有视频轨道")?;
    let index = stream.index();
    let tb = stream.time_base();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let target_us = target_ms.saturating_mul(1000).min(i64::MAX as u64) as i64;
    if target_us > 0 {
        let seek = origin.saturating_add(target_us);
        input.seek(seek, ..seek).context("无法定位视频")?;
        decoder.flush();
    }
    let mut scaler: Option<ff::software::scaling::Context> = None;
    let mut layout = None;
    let mut preceding: Option<PlaybackVideoFrame> = None;
    let mut emitted_after_target = false;
    let mut previous_pts = None;
    let mut consume = |frame: &ff::frame::Video| -> Result<()> {
        let pts = frame.timestamp().context("视频帧缺少媒体时间戳")?;
        let pts_us = ff::Rescale::rescale(&pts, tb, (1, 1_000_000))
            .saturating_sub(origin)
            .max(0);
        if previous_pts.is_some_and(|previous| pts_us < previous) {
            return Ok(());
        }
        previous_pts = Some(pts_us);
        let (width, height) = (frame.width(), frame.height());
        ensure!(width > 0 && height > 0, "视频尺寸无效");
        let scale = (960. / f64::from(width))
            .min(540. / f64::from(height))
            .min(1.);
        let out_width = (f64::from(width) * scale).round().max(1.) as u32;
        let out_height = (f64::from(height) * scale).round().max(1.) as u32;
        let current_layout = (frame.format(), width, height, out_width, out_height);
        if layout != Some(current_layout) {
            scaler = Some(ff::software::scaling::Context::get(
                frame.format(),
                width,
                height,
                ff::format::Pixel::BGRA,
                out_width,
                out_height,
                ff::software::scaling::Flags::BILINEAR,
            )?);
            layout = Some(current_layout);
        }
        let mut image = ff::frame::Video::new(ff::format::Pixel::BGRA, out_width, out_height);
        scaler
            .as_mut()
            .context("视频缩放器不可用")?
            .run(frame, &mut image)?;
        let row_bytes = out_width as usize * 4;
        let stride = image.stride(0);
        ensure!(
            stride >= row_bytes
                && image.data(0).len() >= stride.saturating_mul(out_height as usize),
            "视频帧数据不完整"
        );
        let mut bgra = vec![0; row_bytes * out_height as usize];
        for y in 0..out_height as usize {
            bgra[y * row_bytes..(y + 1) * row_bytes]
                .copy_from_slice(&image.data(0)[y * stride..y * stride + row_bytes]);
        }
        let image = PlaybackVideoFrame {
            generation,
            pts_ms: pts_us as u64 / 1000,
            width: out_width,
            height: out_height,
            bgra,
        };
        if pts_us <= target_us {
            preceding = Some(image);
        } else {
            if !emitted_after_target {
                if let Some(prior) = preceding.take() {
                    send_video_message(frames, VideoMessage::Frame(prior), cancelled)?;
                }
                emitted_after_target = true;
            }
            send_video_message(frames, VideoMessage::Frame(image), cancelled)?;
        }
        Ok(())
    };
    loop {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        let mut packet = ff::Packet::empty();
        let mut eof = false;
        match packet.read(&mut input) {
            Ok(()) if packet.stream() == index => decoder.send_packet(&packet)?,
            Ok(()) => continue,
            Err(ff::Error::Eof) => {
                decoder.send_eof()?;
                eof = true;
            }
            Err(error) => return Err(error.into()),
        }
        loop {
            let mut frame = ff::frame::Video::empty();
            match decoder.receive_frame(&mut frame) {
                Ok(()) => consume(&frame)?,
                Err(ff::Error::Eof)
                | Err(ff::Error::Other {
                    errno: ff::error::EAGAIN,
                }) => break,
                Err(error) => return Err(error.into()),
            }
            if cancelled.load(Ordering::Acquire) {
                break;
            }
        }
        if eof {
            break;
        }
    }
    if let Some(prior) = preceding {
        send_video_message(frames, VideoMessage::Frame(prior), cancelled)?;
    }
    Ok(())
}
