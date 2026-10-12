use crate::MediaWriter;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::*;
use opencap_policy::FramePolicy;
use parking_lot::Mutex;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub enum RecordingCommand {
    Pause,
    Resume,
    Stop,
    SetAudio(AudioRouting),
    SetMicDenoise(bool),
}
pub struct RecordingSession {
    commands: Sender<RecordingCommand>,
    stats: Arc<Mutex<RecordingStats>>,
    worker: Option<JoinHandle<()>>,
    preview: Arc<crate::preview::PreviewSlot>,
}
impl RecordingSession {
    pub fn start(config: RecordingConfig) -> Result<Self> {
        config.validate()?;
        std::fs::create_dir_all(&config.output_dir)?;
        let (commands, rx) = unbounded();
        let stats = Arc::new(Mutex::new(RecordingStats {
            state: RecordingState::Starting,
            ..Default::default()
        }));
        let shared = stats.clone();
        let preview = Arc::new(crate::preview::PreviewSlot::default());
        let worker_preview = preview.clone();
        let worker = thread::Builder::new()
            .name("opencap-recording".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(config, rx, shared.clone(), worker_preview)
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("录制线程异常退出，已写入的临时文件可尝试恢复".into()),
                };
                if let Some(error) = error {
                    let mut s = shared.lock();
                    s.state = RecordingState::Failed;
                    s.error = Some(error);
                }
            })?;
        Ok(Self {
            commands,
            stats,
            worker: Some(worker),
            preview,
        })
    }
    pub fn command(&self, command: RecordingCommand) {
        let _ = self.commands.try_send(command);
    }
    pub fn stats(&self) -> RecordingStats {
        self.stats.lock().clone()
    }
    pub fn enable_preview(&self, enabled: bool) {
        self.preview.enable(enabled);
    }
    pub fn take_preview(&self) -> Option<crate::RecordingPreview> {
        self.preview.take()
    }
    pub fn set_audio(&self, routing: AudioRouting) -> Result<()> {
        self.commands
            .try_send(RecordingCommand::SetAudio(routing))
            .context("音频设置尚未应用，请稍后重试")
    }
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.is_finished())
    }
    pub fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for RecordingSession {
    fn drop(&mut self) {
        self.command(RecordingCommand::Stop);
        self.join();
    }
}
fn unique_output(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("_{n:02}")
        };
        let output = dir.join(format!("OpenCap_{stamp}{suffix}.mp4"));
        let partial = output.with_extension("partial.mp4");
        if output.exists()
            || output.with_extension("m4a").exists()
            || output.with_extension("partial.m4a").exists()
            || output.with_extension("recording.json").exists()
        {
            continue;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
        {
            Ok(_) => return Ok((output, partial)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("Unable to allocate recording filename")
}
/// Microphone conditioning runs on the capture side so the level meter and the
/// recorded track always see the same signal.
struct MicChain {
    enabled: bool,
    denoiser: MicDenoiser,
    last_end_us: Option<i64>,
}
impl MicChain {
    fn new(config: &RecordingConfig) -> Self {
        Self {
            enabled: config.mic_denoise,
            denoiser: MicDenoiser::new(48_000),
            last_end_us: None,
        }
    }
    fn apply(&mut self, chunk: &mut AudioChunk) {
        if self.enabled && chunk.kind == AudioKind::Microphone {
            self.last_end_us = Some(chunk.pts_us + chunk.samples.len() as i64 / 2 * 1000 / 48);
            self.denoiser.process(&mut chunk.samples);
            chunk.pts_us -= MicDenoiser::LATENCY_FRAMES as i64 * 1000 / 48;
        }
    }
    fn flush(&mut self) -> Option<AudioChunk> {
        let end = self.last_end_us.take()?;
        let mut samples = vec![0.0; MicDenoiser::LATENCY_FRAMES * 2];
        self.denoiser.process(&mut samples);
        Some(AudioChunk {
            kind: AudioKind::Microphone,
            pts_us: end - MicDenoiser::LATENCY_FRAMES as i64 * 1000 / 48,
            samples,
        })
    }
    fn reset(&mut self) {
        self.denoiser = MicDenoiser::new(48_000);
        self.last_end_us = None;
    }
}
fn run(
    config: RecordingConfig,
    commands: Receiver<RecordingCommand>,
    stats: Arc<Mutex<RecordingStats>>,
    preview: Arc<crate::preview::PreviewSlot>,
) -> Result<()> {
    let clock = RecordingClock::new();
    let (video_tx, video_rx) = bounded(2);
    let (audio_tx, audio_rx) = bounded(256);
    let (errors_tx, errors_rx) = bounded(8);
    let (warn_tx, warn_rx) = bounded(16);
    let dropped = Arc::new(AtomicU64::new(0));
    let mut capture = opencap_platform::start_capture(
        &config,
        clock.clone(),
        video_tx,
        video_rx.clone(),
        errors_tx,
        dropped.clone(),
    )?;
    stats
        .lock()
        .warnings
        .extend(capture.warnings().iter().cloned());
    let first = video_rx
        .recv_timeout(Duration::from_secs(10))
        .context("No capture frames received within 10 seconds")?;
    let (output, partial) = unique_output(&config.output_dir)?;
    stats.lock().output = Some(partial.clone());
    let mut writer = MediaWriter::new(&partial, first.width, first.height, &config)?;
    // Encoder probing can take seconds; start the recording timeline only once ready.
    let video_epoch = clock.now_us();
    let mut audio = opencap_platform::start_audio(&config, clock.clone(), audio_tx, warn_tx);
    let mut routing = config.audio_routing();
    let mut mic = MicChain::new(&config);
    let mut audio_changes = Vec::new();
    let mut peaks = [0.0f32; 2];
    let mut latest = first;
    latest.pts_us = 0;
    while let Ok(mut frame) = video_rx.try_recv() {
        frame.pts_us = 0;
        latest = frame;
    }
    dropped.store(0, Ordering::Relaxed);
    writer.video(&latest)?;
    let mut preview_tick = None;
    preview.publish(&latest, &mut preview_tick);
    let mut policy = FramePolicy::default();
    let _ = policy.accept(&latest, &config);
    {
        let mut s = stats.lock();
        s.state = RecordingState::Recording;
        s.encoder = writer.video.name.clone();
        s.audio_routing = routing.clone();
    }
    let mut tick = Instant::now();
    let mut last_frames = 0;
    let mut last_bytes = 0;
    let mut meter = opencap_platform::ProcessMeter::default();
    let mut stop_reason = None;
    let mut last_capture = Instant::now();
    let mut idle_warned = false;
    let mut silent_audio_since = [Instant::now(); 2];
    let mut silent_audio_warned = [false; 2];
    loop {
        let mut stop = false;
        for cmd in commands.try_iter() {
            match cmd {
                RecordingCommand::Pause => {
                    clock.pause();
                    // Preserve the last captured syllable before resetting the
                    // delayed microphone filter on resume.
                    for mut chunk in audio_rx.try_iter() {
                        mic.apply(&mut chunk);
                        rebase_audio(&mut chunk, video_epoch);
                        writer.audio(chunk);
                    }
                    if let Some(mut tail) = mic.flush() {
                        rebase_audio(&mut tail, video_epoch);
                        writer.audio(tail);
                    }
                    mic.reset();
                    stats.lock().state = RecordingState::Paused;
                }
                RecordingCommand::Resume => {
                    mic.reset();
                    clock.resume();
                    stats.lock().state = RecordingState::Recording;
                }
                RecordingCommand::Stop => stop = true,
                RecordingCommand::SetMicDenoise(enabled) => {
                    if let Some(mut tail) = mic.flush() {
                        rebase_audio(&mut tail, video_epoch);
                        writer.audio(tail);
                    }
                    mic.enabled = enabled;
                    mic.reset();
                }
                RecordingCommand::SetAudio(next) => {
                    if writer.audio_cursor_us().is_none() {
                        stats
                            .lock()
                            .warnings
                            .push("本次录制未预留音轨，请停止后重新录制以启用声音".into());
                        continue;
                    }
                    let changed: Vec<_> = [AudioKind::System, AudioKind::Microphone]
                        .into_iter()
                        .filter(|&kind| {
                            routing.enabled(kind) != next.enabled(kind)
                                || routing.device(kind) != next.device(kind)
                        })
                        .collect();
                    audio.reconfigure(&next);
                    // Changed workers have stopped. Discard their queued data and clear
                    // their unencoded samples without discarding the other audio route.
                    for mut chunk in audio_rx.try_iter() {
                        if !changed.contains(&chunk.kind) {
                            mic.apply(&mut chunk);
                            rebase_audio(&mut chunk, video_epoch);
                            writer.audio(chunk);
                        }
                    }
                    for kind in changed {
                        writer.clear_audio_route(kind);
                        if kind == AudioKind::Microphone {
                            mic.reset();
                        }
                        let index = usize::from(kind == AudioKind::Microphone);
                        peaks[index] = 0.0;
                        silent_audio_since[index] = Instant::now();
                        silent_audio_warned[index] = false;
                    }
                    routing = next;
                    let mut s = stats.lock();
                    s.audio_routing = routing.clone();
                    s.system_peak = 0.0;
                    s.microphone_peak = 0.0;
                    audio_changes.push(serde_json::json!({"at_us": (clock.now_us() - video_epoch).max(0), "routing": routing}));
                }
            }
        }
        if stop {
            break;
        }
        if let Ok(reason) = errors_rx.try_recv() {
            stop_reason = Some(reason);
            break;
        }
        if capture.is_finished() {
            stop_reason = Some("屏幕采集线程已结束，正在保存已录制内容".into());
            break;
        }
        for warning in warn_rx.try_iter() {
            stats.lock().warnings.push(warning);
        }
        let now = (clock.now_us() - video_epoch).max(0);
        while let Ok(mut chunk) = audio_rx.try_recv() {
            mic.apply(&mut chunk);
            let index = usize::from(chunk.kind == AudioKind::Microphone);
            peaks[index] = chunk
                .samples
                .iter()
                .filter(|v| v.is_finite())
                .fold(peaks[index], |peak, value| peak.max(value.abs().min(1.0)));
            rebase_audio(&mut chunk, video_epoch);
            writer.audio(chunk);
        }
        if !clock.is_paused() {
            while let Ok(mut frame) = video_rx.try_recv() {
                frame.pts_us = (frame.pts_us - video_epoch).max(0);
                latest = frame;
                last_capture = Instant::now();
                idle_warned = false;
            }
            let mut frame = latest.clone();
            // Cached frames carry the current media time, including static end tails.
            frame.pts_us = now;
            if policy.accept(&frame, &config) {
                writer.video(&frame)?;
                preview.publish(&frame, &mut preview_tick);
            }
            writer.audio_until((now - 150_000).max(0), false)?;
        }
        if tick.elapsed() >= Duration::from_secs(1) {
            writer.flush_io()?;
            if !opencap_platform::desktop_available() {
                stop_reason = Some("桌面已锁定或无法访问，录制已安全结束".into());
                break;
            }
            let bytes = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
            let elapsed = tick.elapsed().as_secs_f64();
            let (cpu, memory) = meter.sample();
            let mut s = stats.lock();
            s.duration_seconds = now as f64 / 1_000_000.0;
            s.fps = (writer.frames - last_frames) as f64 / elapsed;
            s.encoded_frames = writer.frames;
            s.bitrate_kbps = bytes.saturating_sub(last_bytes) as f64 * 8.0 / 1000.0 / elapsed;
            s.bytes = bytes;
            s.dropped_frames = dropped.load(Ordering::Relaxed);
            s.cpu_percent = cpu;
            s.peak_memory_bytes = memory;
            for (i, kind) in [AudioKind::System, AudioKind::Microphone]
                .into_iter()
                .enumerate()
            {
                if !routing.enabled(kind) || clock.is_paused() || peaks[i] > 0.0005 {
                    silent_audio_since[i] = Instant::now();
                    silent_audio_warned[i] = false;
                } else if !silent_audio_warned[i]
                    && silent_audio_since[i].elapsed() > Duration::from_secs(12)
                {
                    let what = if kind == AudioKind::System {
                        "系统声音"
                    } else {
                        "麦克风"
                    };
                    s.warnings.push(format!(
                        "{what}连续 12 秒没有检测到信号；请确认设备和权限，或打开电平检测"
                    ));
                    silent_audio_warned[i] = true;
                }
            }
            s.system_peak = peaks[0];
            s.microphone_peak = peaks[1];
            peaks = [0.0; 2];
            if !idle_warned
                && !clock.is_paused()
                && matches!(config.target, CaptureTarget::Window(_))
                && last_capture.elapsed() > Duration::from_secs(5)
            {
                s.warnings
                    .push("窗口画面静止或不可见，当前保留最后画面".into());
                idle_warned = true;
            }
            last_frames = writer.frames;
            last_bytes = bytes;
            tick = Instant::now();
        }
        thread::sleep(Duration::from_millis(5));
    }
    stats.lock().state = RecordingState::Stopping;
    clock.pause();
    let stop_us = (clock.now_us() - video_epoch).max(1);
    if let Err(error) = capture.stop() {
        stats
            .lock()
            .warnings
            .push(format!("采集结束时发生错误：{error}"));
    }
    if let Err(error) = audio.stop() {
        stats
            .lock()
            .warnings
            .push(format!("音频采集结束时发生错误：{error}"));
    }
    stats.lock().warnings.extend(warn_rx.try_iter());
    for mut chunk in audio_rx.try_iter() {
        mic.apply(&mut chunk);
        rebase_audio(&mut chunk, video_epoch);
        writer.audio(chunk);
    }
    if let Some(mut tail) = mic.flush() {
        rebase_audio(&mut tail, video_epoch);
        writer.audio(tail);
    }
    let encoder_options = writer.video.options.clone();
    let encoder_details = writer.video.details();
    let frames = writer.frames;
    let has_audio = writer.audio_cursor_us().is_some();
    writer.finish(stop_us)?;
    crate::recover_file(&partial, &output)
        .context("Recording saved as partial MP4; finalization/recovery failed")?;
    std::fs::remove_file(&partial)?;
    if config.separate_audio && has_audio {
        let audio_output = output.with_extension("m4a");
        match crate::export_audio(&output, &audio_output) {
            Ok(()) => stats.lock().audio_output = Some(audio_output),
            Err(error) => stats
                .lock()
                .warnings
                .push(format!("视频已保存，独立音频导出失败：{error:#}")),
        }
    }
    let final_bytes = std::fs::metadata(&output)?.len();
    let snapshot = {
        let mut s = stats.lock();
        s.state = RecordingState::Finished;
        s.duration_seconds = stop_us as f64 / 1_000_000.0;
        s.encoded_frames = frames;
        s.dropped_frames = dropped.load(Ordering::Relaxed);
        s.fps = frames as f64 / s.duration_seconds;
        s.bytes = final_bytes;
        s.bitrate_kbps = final_bytes as f64 * 8.0 / 1000.0 / s.duration_seconds;
        s.av_offset_ms = if has_audio {
            container_av_offset(&output).ok()
        } else {
            None
        };
        s.output = Some(output.clone());
        if let Some(reason) = stop_reason {
            s.warnings.push(reason);
        }
        s.clone()
    };
    let report = serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "config": config, "stats": snapshot, "encoder_options": encoder_options, "encoder_details": encoder_details, "output_media":crate::inspect_output(&output)?,
        "pixel_format": "8-bit 4:2:0 BT.709 SDR", "time_base": "1/90000", "rate_control": if snapshot.encoder.ends_with("_qsv") { "ICQ (no hard VBV cap)" } else { "quality with encoder-specific rate control" },
        "audio_changes": audio_changes, "gpu_usage": "unavailable", "capture_path": opencap_platform::CAPTURE_PATH, "av_offset_definition": "audio stream end minus video stream end; does not measure perceptual synchronization" });
    std::fs::write(
        output.with_extension("recording.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
fn container_av_offset(path: &Path) -> Result<f64> {
    let context = ffmpeg_next::format::input(path)?;
    let end = |kind| -> Result<f64> {
        let s = context.streams().best(kind).context("Missing stream")?;
        Ok((s.start_time() + s.duration()) as f64 * f64::from(s.time_base()) * 1000.0)
    };
    Ok(end(ffmpeg_next::media::Type::Audio)? - end(ffmpeg_next::media::Type::Video)?)
}
