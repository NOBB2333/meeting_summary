use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use opencap_core::*;
use parking_lot::Mutex;
use std::{
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
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
fn default_monitor() -> Result<String> {
    let info = pactl(&["info"])?;
    let sink = info
        .lines()
        .find_map(|l| l.strip_prefix("Default Sink:").map(str::trim))
        .context("找不到系统默认声音输出设备")?;
    Ok(format!("{sink}.monitor"))
}
struct AudioRoute {
    kind: AudioKind,
    device: Option<String>,
    child: Arc<Mutex<Child>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for AudioRoute {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            let mut child = self.child.lock();
            let _ = child.kill();
            let _ = child.wait();
        }
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
    let source = match &device {
        Some(id) => id.clone(),
        None if kind == AudioKind::System => default_monitor()?,
        None => "default".into(),
    };
    let mut child = Command::new(program("ffmpeg"))
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-f",
            "pulse",
            "-sample_rate",
            "48000",
            "-channels",
            "2",
            "-i",
        ])
        .arg(source)
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
    let child = Arc::new(Mutex::new(child));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("opencap-pulse".into())
        .spawn(move || {
            let mut bytes = vec![0u8; 960 * 2 * 4];
            let mut timeline = AudioTimeline::default();
            while !worker_stop.load(Ordering::Acquire) {
                if stdout.read_exact(&mut bytes).is_err() {
                    if !worker_stop.load(Ordering::Acquire) {
                        let _ = warnings.try_send(format!(
                            "{}音频采集已退出；请检查声音设备和 PulseAudio / PipeWire 服务",
                            if kind == AudioKind::System {
                                "系统"
                            } else {
                                "麦克风"
                            }
                        ));
                    }
                    break;
                }
                let samples: Vec<_> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                    .collect();
                if let Some(pts_us) = timeline.timestamp(&clock, 20_000, samples.len() / 2) {
                    if tx
                        .try_send(AudioChunk {
                            kind,
                            pts_us,
                            samples,
                        })
                        .is_err()
                    {
                        let _ = warnings.try_send("声音队列积压，部分采样未能保存".into());
                    }
                }
            }
        })
        .inspect_err(|_| {
            let mut child = child.lock();
            let _ = child.kill();
            let _ = child.wait();
        })?;
    Ok(AudioRoute {
        kind,
        device,
        child,
        stop,
        worker: Some(worker),
    })
}
pub struct LinuxAudio {
    routes: Vec<AudioRoute>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
}
impl LinuxAudio {
    pub fn reconfigure(&mut self, routing: &AudioRouting) {
        self.routes.retain(|r| {
            routing.enabled(r.kind)
                && routing.device(r.kind) == &r.device
                && r.child.lock().try_wait().is_ok_and(|s| s.is_none())
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
    state: Arc<Mutex<[(f32, Instant); 2]>>,
    denoise: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    warnings: Receiver<String>,
}
impl AudioMonitor {
    pub fn set_routing(&mut self, routing: &AudioRouting, denoise: bool) {
        self.denoise.store(denoise, Ordering::Relaxed);
        self.audio.reconfigure(routing);
        *self.state.lock() = [(0., Instant::now()); 2];
    }
    pub fn peaks(&self) -> (f32, f32) {
        let s = self.state.lock();
        let peak = |i: usize| {
            if s[i].1.elapsed() < Duration::from_millis(300) {
                s[i].0
            } else {
                0.
            }
        };
        (peak(0), peak(1))
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
    let state = Arc::new(Mutex::new([(0., Instant::now()); 2]));
    let worker_state = state.clone();
    let denoise = Arc::new(AtomicBool::new(denoise));
    let worker_denoise = denoise.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        let mut chain = MicDenoiser::new(48000);
        while !worker_stop.load(Ordering::Acquire) {
            if let Ok(mut chunk) = rx.recv_timeout(Duration::from_millis(100)) {
                if chunk.kind == AudioKind::Microphone && worker_denoise.load(Ordering::Relaxed) {
                    chain.process(&mut chunk.samples);
                }
                let peak = chunk
                    .samples
                    .iter()
                    .copied()
                    .filter(|s| s.is_finite())
                    .map(f32::abs)
                    .fold(0., f32::max);
                worker_state.lock()[usize::from(chunk.kind == AudioKind::Microphone)] =
                    (peak, Instant::now());
            }
        }
    });
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
