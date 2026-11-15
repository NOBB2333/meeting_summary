use crate::audio_state::RouteTelemetry;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use opencap_core::*;
use parking_lot::Mutex;
use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) fn program(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}
fn pactl(args: &[&str]) -> Result<String> {
    let result = Command::new(program("pactl"))
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .context("需要可用的 PulseAudio 服务和 pactl（PipeWire 可使用 pipewire-pulse）")?;
    anyhow::ensure!(
        result.status.success(),
        "无法连接 PulseAudio / PipeWire 音频服务"
    );
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}
pub fn audio_devices() -> Result<Vec<AudioDevice>> {
    let mut devices = Vec::new();
    for line in pactl(&["list", "short", "sources"])?.lines() {
        let mut fields = line.split('\t');
        fields.next();
        if let Some(id) = fields.next() {
            devices.push(AudioDevice {
                id: id.into(),
                name: id.replace('_', " "),
                kind: if id.ends_with(".monitor") {
                    AudioKind::System
                } else {
                    AudioKind::Microphone
                },
            });
        }
    }
    Ok(devices)
}
pub fn microphone_permission_granted() -> bool {
    true
} // PulseAudio authenticates the active user session at stream creation.
/// `pactl` runs away from the audio reader. Bound both its runtime and output
/// so a broken sound server cannot indefinitely block shutdown.
fn pactl_bounded(args: &[&str]) -> Result<String> {
    let mut child = Command::new(program("pactl"))
        .env("LC_ALL", "C")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().context("无法读取音频服务状态")?;
    let reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stdout.take(1_048_577).read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(anyhow::anyhow!("音频服务状态查询超时"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.into());
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("音频服务状态读取失败"))??;
    anyhow::ensure!(bytes.len() <= 1_048_576, "音频服务状态过大");
    anyhow::ensure!(status?.success(), "无法连接 PulseAudio / PipeWire 音频服务");
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn pulse_default(kind: AudioKind) -> Result<String> {
    let info = pactl_bounded(&["info"])?;
    let key = if kind == AudioKind::System {
        "Default Sink:"
    } else {
        "Default Source:"
    };
    let device = info
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
        .filter(|value| !value.is_empty())
        .context("找不到系统默认音频设备")?;
    Ok(if kind == AudioKind::System {
        format!("{device}.monitor")
    } else {
        device.to_owned()
    })
}

/// Query the source currently connected to our uniquely named Pulse stream,
/// rather than assuming a successful FFmpeg process used the requested source.
fn pulse_resolved_source(stream_name: &str) -> Result<Option<String>> {
    let outputs = pactl_bounded(&["list", "source-outputs"])?;
    let source_index = outputs.split("Source Output #").skip(1).find_map(|block| {
        let matches = block.lines().any(|line| {
            line.trim()
                .strip_prefix("media.name = ")
                .is_some_and(|name| name.trim_matches('"') == stream_name)
        });
        matches
            .then(|| {
                block
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("Source:").map(str::trim))
            })
            .flatten()
    });
    let Some(source_index) = source_index else {
        return Ok(None);
    };
    let sources = pactl_bounded(&["list", "short", "sources"])?;
    Ok(sources.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let index = fields.next()?;
        let name = fields.next()?;
        (index == source_index).then(|| name.to_owned())
    }))
}
struct AudioRoute {
    kind: AudioKind,
    device: Option<String>,
    telemetry: Arc<RouteTelemetry>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for AudioRoute {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn start_route(
    kind: AudioKind,
    device: Option<String>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
) -> Result<AudioRoute> {
    let telemetry = Arc::new(RouteTelemetry::new(kind, device.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_device = device.clone();
    let worker_stop = stop.clone();
    let worker_telemetry = telemetry.clone();
    let worker = thread::Builder::new()
        .name("opencap-pulse-supervisor".into())
        .spawn(move || {
            let mut previous_error = String::new();
            while !worker_stop.load(Ordering::Acquire) {
                worker_telemetry.starting();
                let began = Instant::now();
                let result = capture_route(
                    kind,
                    worker_device.clone(),
                    clock.clone(),
                    tx.clone(),
                    warnings.clone(),
                    worker_stop.clone(),
                    worker_telemetry.clone(),
                );
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                let error = result
                    .err()
                    .map(|error| format!("{error:#}"))
                    .unwrap_or_else(|| "采集流意外结束".into());
                worker_telemetry.waiting(error.clone());
                if error != previous_error || began.elapsed() >= Duration::from_secs(30) {
                    let name = if kind == AudioKind::System {
                        "系统声音"
                    } else {
                        "麦克风"
                    };
                    let _ =
                        warnings.try_send(format!("{name}暂不可用，该路补静音并自动重试：{error}"));
                    previous_error = error;
                }
                for _ in 0..20 {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })?;
    Ok(AudioRoute {
        kind,
        device,
        telemetry,
        stop,
        worker: Some(worker),
    })
}

fn capture_route(
    kind: AudioKind,
    device: Option<String>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
    stop: Arc<AtomicBool>,
    telemetry: Arc<RouteTelemetry>,
) -> Result<()> {
    let source = match &device {
        Some(id) => id.clone(),
        None => pulse_default(kind)?,
    };
    let stream_name = format!("OpenCap-{}", new_stable_id());
    let mut child = Command::new(program("ffmpeg"))
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-probesize",
            "32",
            "-analyzeduration",
            "1",
            "-f",
            "pulse",
            "-stream_name",
        ])
        .arg(&stream_name)
        .args([
            "-sample_rate",
            "48000",
            "-channels",
            "2",
            "-fragment_size",
            "3840",
            "-i",
        ])
        .arg(&source)
        .args([
            "-vn",
            "-c:a",
            "pcm_f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-flush_packets",
            "1",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动随包 FFmpeg 音频采集程序")?;
    let mut stdout = child.stdout.take().context("音频采集管道未创建")?;
    let allowed = Arc::new(AtomicBool::new(false));
    let reader_allowed = allowed.clone();
    let reader_stop = stop.clone();
    let reader_telemetry = telemetry.clone();
    let reader = thread::Builder::new()
        .name("opencap-pulse".into())
        .spawn(move || {
            let mut bytes = vec![0u8; 960 * 2 * 4];
            let mut timeline = AudioTimeline::default();
            let mut overrun_warned = false;
            while !reader_stop.load(Ordering::Acquire) {
                let (_, read_paused, read_generation) = clock.snapshot();
                if stdout.read_exact(&mut bytes).is_err() {
                    break;
                }
                if !reader_allowed.load(Ordering::Acquire) {
                    timeline = AudioTimeline::default();
                    continue;
                }
                let samples: Vec<_> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                    .collect();
                if let Some((pts_us, generation)) =
                    timeline.timestamp_with_generation(&clock, 20_000, samples.len() / 2)
                {
                    if read_paused || generation != read_generation {
                        continue;
                    }
                    reader_telemetry.sample(pts_us, generation);
                    if tx
                        .try_send(AudioChunk {
                            generation,
                            kind,
                            pts_us,
                            samples,
                        })
                        .is_err()
                        && !overrun_warned
                    {
                        let _ = warnings.try_send("声音队列积压，部分采样未能保存".into());
                        overrun_warned = true;
                    }
                }
            }
        })
        .inspect_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
    let started = Instant::now();
    let result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        if reader.is_finished() {
            break Err(anyhow::anyhow!("音频采集进程已退出"));
        }
        if device.is_none() {
            match pulse_default(kind) {
                Ok(current) if current == source => {}
                Ok(_) => break Err(anyhow::anyhow!("系统默认设备已更换")),
                Err(error) => {
                    allowed.store(false, Ordering::Release);
                    break Err(error);
                }
            }
        }
        match pulse_resolved_source(&stream_name) {
            Ok(Some(actual)) if actual == source => {
                telemetry.opened(actual.clone(), actual);
                allowed.store(true, Ordering::Release);
            }
            Ok(Some(_)) => {
                allowed.store(false, Ordering::Release);
                break Err(anyhow::anyhow!("音频服务将采集流切换到其他设备"));
            }
            Ok(None) if started.elapsed() < Duration::from_secs(5) => {}
            Ok(None) => {
                allowed.store(false, Ordering::Release);
                break Err(anyhow::anyhow!("无法确认实际音频来源"));
            }
            Err(error) => {
                allowed.store(false, Ordering::Release);
                break Err(error);
            }
        }
        for _ in 0..10 {
            if stop.load(Ordering::Acquire) || reader.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    };
    allowed.store(false, Ordering::Release);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}
pub struct LinuxAudio {
    routes: Vec<AudioRoute>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
}
impl LinuxAudio {
    pub fn clear_sample_times(&self) {
        for route in &self.routes {
            route.telemetry.clear_sample();
        }
    }
    pub fn route_status(&self) -> Vec<AudioRouteStatus> {
        let generation = self.clock.snapshot().2;
        self.routes
            .iter()
            .map(|route| {
                if route
                    .worker
                    .as_ref()
                    .is_none_or(|worker| worker.is_finished())
                {
                    route.telemetry.stopped_snapshot(generation)
                } else {
                    route.telemetry.snapshot(generation)
                }
            })
            .collect()
    }
    pub fn reconfigure(&mut self, routing: &AudioRouting) {
        self.routes.retain(|r| {
            routing.enabled(r.kind)
                && routing.device(r.kind) == &r.device
                && r.worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished())
        });
        for kind in [AudioKind::System, AudioKind::Microphone] {
            if !routing.enabled(kind) || self.routes.iter().any(|r| r.kind == kind) {
                continue;
            }
            match start_route(
                kind,
                routing.device(kind).clone(),
                self.clock.clone(),
                self.tx.clone(),
                self.warnings.clone(),
            ) {
                Ok(route) => self.routes.push(route),
                Err(e) => {
                    let _ = self.warnings.try_send(format!("无法开启声音采集：{e:#}"));
                }
            }
        }
    }
}
impl AudioBackend for LinuxAudio {
    fn stop(&mut self) -> Result<()> {
        self.routes.clear();
        Ok(())
    }
}
pub fn start_audio(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
) -> LinuxAudio {
    let mut audio = LinuxAudio {
        routes: vec![],
        clock,
        tx,
        warnings,
    };
    audio.reconfigure(&config.audio_routing());
    audio
}
pub struct AudioMonitor {
    audio: LinuxAudio,
    state: Arc<Mutex<[(AudioMeter, AudioMeter); 2]>>,
    denoise: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    warnings: Receiver<String>,
}
impl AudioMonitor {
    /// Returns false if the shared meter consumer exited and must be rebuilt.
    pub fn maintain(&mut self, routing: &AudioRouting, _denoise: bool) -> bool {
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
        {
            return false;
        }
        self.audio.reconfigure(routing);
        true
    }
    pub fn set_routing(&mut self, routing: &AudioRouting, denoise: bool) {
        self.denoise.store(denoise, Ordering::Relaxed);
        self.audio.reconfigure(routing);
        *self.state.lock() = [
            (AudioMeter::default(), AudioMeter::default()),
            (AudioMeter::default(), AudioMeter::default()),
        ];
    }
    pub fn levels(&self) -> (AudioRouteLevels, AudioRouteLevels) {
        let s = self.state.lock();
        let now = Instant::now();
        let route = |i: usize| AudioRouteLevels {
            input: s[i].0.level(now),
            processed: s[i].1.level(now),
        };
        (route(0), route(1))
    }
    pub fn take_warning(&self) -> Option<String> {
        self.warnings.try_recv().ok()
    }
}
impl Drop for AudioMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.audio.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn start_audio_monitor(routing: &AudioRouting, denoise: bool) -> Result<AudioMonitor> {
    let (tx, rx) = bounded::<AudioChunk>(16);
    let (warnings_tx, warnings) = bounded(8);
    let mut audio = LinuxAudio {
        routes: vec![],
        clock: RecordingClock::new(),
        tx,
        warnings: warnings_tx,
    };
    audio.reconfigure(routing);
    let state = Arc::new(Mutex::new([
        (AudioMeter::default(), AudioMeter::default()),
        (AudioMeter::default(), AudioMeter::default()),
    ]));
    let worker_state = state.clone();
    let denoise = Arc::new(AtomicBool::new(denoise));
    let worker_denoise = denoise.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("opencap-linux-meter".into())
        .spawn(move || {
            let mut chain = MicDenoiser::new(48000);
            while !worker_stop.load(Ordering::Acquire) {
                if let Ok(mut chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                    let input = AudioLevel::measure(&chunk.samples);
                    if chunk.kind == AudioKind::Microphone && worker_denoise.load(Ordering::Relaxed)
                    {
                        chain.process(&mut chunk.samples);
                    }
                    let now = Instant::now();
                    let mut meters = worker_state.lock();
                    let meter = &mut meters[usize::from(chunk.kind == AudioKind::Microphone)];
                    meter.0.observe(input, now);
                    meter.1.update(&chunk.samples, now);
                }
            }
        })?;
    Ok(AudioMonitor {
        audio,
        state,
        denoise,
        stop,
        worker: Some(worker),
        warnings,
    })
}
fn probe(kind: AudioKind, device: Option<String>, seconds: u64) -> Result<String> {
    let (tx, rx) = bounded(128);
    let (wt, wr) = bounded(8);
    let _route = start_route(kind, device, RecordingClock::new(), tx, wt)?;
    let began = Instant::now();
    let mut energy = 0.;
    let mut count = 0;
    while began.elapsed() < Duration::from_secs(seconds.clamp(1, 30)) {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            for sample in chunk.samples {
                energy += f64::from(sample).powi(2);
                count += 1;
            }
        }
        if let Ok(warning) = wr.try_recv() {
            anyhow::bail!("{warning}");
        }
    }
    anyhow::ensure!(count > 0, "音频服务未返回任何采样，请检查声音设备");
    Ok(format!(
        "48 kHz 立体声 · {count} 个采样 · RMS {:.5}",
        (energy / count as f64).sqrt()
    ))
}
pub fn audio_probe_microphone(device: Option<String>, seconds: u64) -> Result<String> {
    probe(AudioKind::Microphone, device, seconds)
}
pub fn audio_probe_loopback(device: Option<String>, seconds: u64) -> Result<String> {
    probe(AudioKind::System, device, seconds)
}
pub fn audio_diagnostics() -> String {
    pactl(&["info"]).unwrap_or_else(|e| format!("{e:#}"))
}
