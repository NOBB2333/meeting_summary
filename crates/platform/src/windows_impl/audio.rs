use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use opencap_core::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use wasapi::*;

struct ComGuard;
impl ComGuard {
    fn new() -> Result<Self> {
        initialize_mta().ok()?;
        Ok(Self)
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        deinitialize();
    }
}
pub fn audio_devices() -> Result<Vec<AudioDevice>> {
    thread::spawn(|| -> Result<Vec<AudioDevice>> {
        let _com = ComGuard::new()?;
        let enumerator = DeviceEnumerator::new()?;
        let mut result = Vec::new();
        for (direction, kind) in [
            (Direction::Render, AudioKind::System),
            (Direction::Capture, AudioKind::Microphone),
        ] {
            for item in &enumerator.get_device_collection(&direction)? {
                let device = item?;
                result.push(AudioDevice {
                    id: device.get_id()?,
                    name: device.get_friendlyname()?,
                    kind,
                });
            }
        }
        Ok(result)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Audio device enumeration panicked"))?
}
/// Process-wide audio path counters for `opencap-bench` diagnostics. These are
/// cheap relaxed atomics; the GUI never reads them.
pub static AUDIO_DIAG: AudioDiag = AudioDiag::new();
pub struct AudioDiag {
    pub capture_packets: std::sync::atomic::AtomicU64,
    pub capture_nonzero: std::sync::atomic::AtomicU64,
    pub peak_millis: std::sync::atomic::AtomicU64,
    pub chunks_sent: std::sync::atomic::AtomicU64,
    pub chunks_dropped: std::sync::atomic::AtomicU64,
    pub init_errors: std::sync::atomic::AtomicU64,
    pub packets_system: std::sync::atomic::AtomicU64,
    pub packets_mic: std::sync::atomic::AtomicU64,
    pub last_error_millis: std::sync::atomic::AtomicU64,
    pub last_error_len: std::sync::atomic::AtomicU64,
}
impl AudioDiag {
    const fn new() -> Self {
        Self {
            capture_packets: std::sync::atomic::AtomicU64::new(0),
            capture_nonzero: std::sync::atomic::AtomicU64::new(0),
            peak_millis: std::sync::atomic::AtomicU64::new(0),
            chunks_sent: std::sync::atomic::AtomicU64::new(0),
            chunks_dropped: std::sync::atomic::AtomicU64::new(0),
            init_errors: std::sync::atomic::AtomicU64::new(0),
            packets_system: std::sync::atomic::AtomicU64::new(0),
            packets_mic: std::sync::atomic::AtomicU64::new(0),
            last_error_millis: std::sync::atomic::AtomicU64::new(0),
            last_error_len: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
pub fn audio_diagnostics() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    format!(
        "capture_packets={} packets_system={} packets_mic={} capture_nonzero={} peak={:.4} chunks_sent={} chunks_dropped={} init_errors={}",
        AUDIO_DIAG.capture_packets.load(Relaxed),
        AUDIO_DIAG.packets_system.load(Relaxed),
        AUDIO_DIAG.packets_mic.load(Relaxed),
        AUDIO_DIAG.capture_nonzero.load(Relaxed),
        AUDIO_DIAG.peak_millis.load(Relaxed) as f64 / 10_000.0,
        AUDIO_DIAG.chunks_sent.load(Relaxed),
        AUDIO_DIAG.chunks_dropped.load(Relaxed),
        AUDIO_DIAG.init_errors.load(Relaxed),
    )
}
pub struct WindowsAudio {
    routes: Vec<AudioRoute>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
}
struct AudioRoute {
    kind: AudioKind,
    device: Option<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for AudioRoute {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl AudioBackend for WindowsAudio {
    fn stop(&mut self) -> Result<()> {
        self.routes.clear();
        Ok(())
    }
}
impl Drop for WindowsAudio {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn start_audio(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
) -> WindowsAudio {
    let mut audio = WindowsAudio {
        routes: Vec::new(),
        clock,
        tx,
        warnings,
    };
    audio.reconfigure(&config.audio_routing());
    audio
}
impl WindowsAudio {
    /// Stop only changed routes. The unchanged route retains its stream and clock.
    pub fn reconfigure(&mut self, routing: &AudioRouting) {
        self.routes.retain(|route| {
            routing.enabled(route.kind) && routing.device(route.kind) == &route.device
        });
        for kind in [AudioKind::System, AudioKind::Microphone] {
            if !routing.enabled(kind) || self.routes.iter().any(|route| route.kind == kind) {
                continue;
            }
            let device = routing.device(kind).clone();
            let stop = Arc::new(AtomicBool::new(false));
            let (worker_stop, clock, tx, warnings, worker_device) = (
                stop.clone(),
                self.clock.clone(),
                self.tx.clone(),
                self.warnings.clone(),
                device.clone(),
            );
            let thread = thread::spawn(move || {
                if let Err(e) = audio_loop(
                    kind,
                    worker_device,
                    clock,
                    worker_stop,
                    tx,
                    warnings.clone(),
                ) {
                    let name = if kind == AudioKind::System {
                        "系统声音"
                    } else {
                        "麦克风"
                    };
                    let _ = warnings.try_send(format!(
                        "{name}不可用，该路补静音；可切换设备或重新开启：{e:#}"
                    ));
                }
            });
            self.routes.push(AudioRoute {
                kind,
                device,
                stop,
                thread: Some(thread),
            });
        }
    }
}

/// Metering-only capture for the idle screen. It opens the selected devices but
/// never stores samples, and it applies the same microphone chain as the
/// recorder so the bar shows what the recording would actually contain.
pub struct AudioMonitor {
    routes: Vec<AudioRoute>,
    state: Arc<Mutex<MonitorState>>,
    warning_tx: Sender<String>,
    warning_rx: Receiver<String>,
    denoise: bool,
}
struct MonitorState {
    value: [f32; 2],
    updated: [Instant; 2],
}
impl Default for MonitorState {
    fn default() -> Self {
        Self {
            value: [0.0; 2],
            updated: [Instant::now(); 2],
        }
    }
}
impl Drop for AudioMonitor {
    fn drop(&mut self) {
        self.routes.clear();
    }
}
impl AudioMonitor {
    pub fn set_routing(&mut self, routing: &AudioRouting, denoise: bool) {
        if self.denoise != denoise {
            self.routes
                .retain(|route| route.kind != AudioKind::Microphone);
            self.denoise = denoise;
        }
        self.routes.retain(|route| {
            routing.enabled(route.kind) && routing.device(route.kind) == &route.device
        });
        for kind in [AudioKind::System, AudioKind::Microphone] {
            if !routing.enabled(kind) || self.routes.iter().any(|route| route.kind == kind) {
                continue;
            }
            let device = routing.device(kind).clone();
            let stop = Arc::new(AtomicBool::new(false));
            let state = self.state.clone();
            let worker_device = device.clone();
            let worker_stop = stop.clone();
            let warnings = self.warning_tx.clone();
            let thread = thread::spawn(move || {
                if let Err(error) = monitor_loop(kind, worker_device, worker_stop, state, denoise) {
                    let _ = warnings.try_send(format!("{kind:?} 电平检测失败：{error:#}"));
                }
            });
            self.routes.push(AudioRoute {
                kind,
                device,
                stop,
                thread: Some(thread),
            });
        }
    }
    /// Latest peak level per route in the mixer's own order: system, microphone.
    pub fn peaks(&self) -> (f32, f32) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let peak =
            |i: usize| state.value[i] * (-state.updated[i].elapsed().as_secs_f32() / 0.35).exp();
        (peak(0), peak(1))
    }
    pub fn take_warning(&self) -> Option<String> {
        self.warning_rx.try_recv().ok()
    }
}
pub fn start_audio_monitor(routing: &AudioRouting, denoise: bool) -> Result<AudioMonitor> {
    let (warning_tx, warning_rx) = unbounded();
    let mut monitor = AudioMonitor {
        routes: Vec::new(),
        state: Arc::new(Mutex::new(MonitorState::default())),
        warning_tx,
        warning_rx,
        denoise,
    };
    if !routing.system_audio && !routing.microphone {
        anyhow::bail!("没有开启任何声音来源");
    }
    monitor.set_routing(routing, denoise);
    Ok(monitor)
}
fn monitor_loop(
    kind: AudioKind,
    device_id: Option<String>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<MonitorState>>,
    denoise: bool,
) -> Result<()> {
    let mut denoiser = (denoise && kind == AudioKind::Microphone).then(|| MicDenoiser::new(48_000));
    let index = usize::from(kind == AudioKind::Microphone);
    capture_loop(kind, device_id, stop, move |_latency, samples| {
        let mut filtered;
        let values: &[f32] = if let Some(denoiser) = denoiser.as_mut() {
            filtered = samples.to_vec();
            denoiser.process(&mut filtered);
            &filtered
        } else {
            samples
        };
        let peak = values
            .iter()
            .filter(|value| value.is_finite())
            .fold(0.0f32, |peak, value| peak.max(value.abs().min(1.0)));
        let now = Instant::now();
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = now
            .saturating_duration_since(state.updated[index])
            .as_secs_f32();
        state.value[index] = (state.value[index] * (-elapsed / 0.35).exp()).max(peak);
        state.updated[index] = now;
        Ok(())
    })
}
fn audio_loop(
    kind: AudioKind,
    device_id: Option<String>,
    clock: RecordingClock,
    stop: Arc<AtomicBool>,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
) -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let counter = if kind == AudioKind::System {
        &AUDIO_DIAG.packets_system
    } else {
        &AUDIO_DIAG.packets_mic
    };
    let mut timeline = AudioTimeline::default();
    let mut overrun_warned = false;
    let result = capture_loop(kind, device_id, stop, |latency, samples| {
        counter.fetch_add(1, Relaxed);
        let peak = samples
            .iter()
            .filter(|value| value.is_finite())
            .fold(0.0f32, |peak, value| peak.max(value.abs()));
        if peak > 0.0 {
            AUDIO_DIAG.capture_nonzero.fetch_add(1, Relaxed);
        }
        let millis = (peak.min(1.0) as f64 * 10_000.0) as u64;
        AUDIO_DIAG.peak_millis.fetch_max(millis, Relaxed);
        let Some(pts_us) = timeline.timestamp(&clock, latency, samples.len() / 2) else {
            return Ok(());
        };
        match tx.try_send(AudioChunk {
            kind,
            pts_us,
            samples: samples.to_vec(),
        }) {
            Ok(()) => {
                AUDIO_DIAG.chunks_sent.fetch_add(1, Relaxed);
            }
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                AUDIO_DIAG.chunks_dropped.fetch_add(1, Relaxed);
                if !overrun_warned {
                    let _ = warnings
                        .try_send(format!("{kind:?} 音频队列积压，部分采样丢失；采集会继续"));
                    overrun_warned = true;
                }
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                anyhow::bail!("Audio receiver closed")
            }
        }
        Ok(())
    });
    if result.is_err() {
        AUDIO_DIAG.init_errors.fetch_add(1, Relaxed);
    }
    result
}
/// Shared WASAPI capture. `sink` receives the device latency and one 48 kHz
/// stereo float buffer per packet, with silent packets expanded to zeroes so the
/// recording timeline stays continuous.
fn capture_loop<F>(
    kind: AudioKind,
    device_id: Option<String>,
    stop: Arc<AtomicBool>,
    mut sink: F,
) -> Result<()>
where
    F: FnMut(i64, &[f32]) -> Result<()>,
{
    let _com = ComGuard::new()?;
    let enumerator = DeviceEnumerator::new()?;
    let direction = if kind == AudioKind::System {
        Direction::Render
    } else {
        Direction::Capture
    };
    let device = if let Some(id) = device_id {
        enumerator.get_device(&id)?
    } else {
        enumerator.get_default_device(&direction)?
    };
    let mut client = device.get_iaudioclient()?;
    // The Windows shared audio engine performs device-rate conversion to 48 kHz stereo.
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
    client.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: 1_000_000,
        },
    )?;
    let capture = client.get_audiocaptureclient()?;
    let mut bytes = vec![0u8; client.get_buffer_size()? as usize * 8];
    client.start_stream()?;
    while !stop.load(Ordering::Acquire) {
        while !stop.load(Ordering::Acquire) && capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let (count, info) = capture.read_from_device(&mut bytes)?;
            AUDIO_DIAG
                .capture_packets
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let latency = if info.flags.timestamp_error {
                count as i64 * 1_000_000 / 48000
            } else {
                (super::qpc_us() - (info.timestamp / 10) as i64).clamp(0, 1_000_000)
            };
            let samples: Vec<f32> = if info.flags.silent {
                vec![0.0; count as usize * 2]
            } else {
                bytes[..count as usize * 8]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                    .collect()
            };
            sink(latency, &samples)?;
        }
        thread::sleep(Duration::from_millis(5));
    }
    client.stop_stream()?;
    Ok(())
}

/// Diagnostic: capture the selected microphone endpoint for `seconds` and report
/// raw-stream sanity metrics. A clean stream has a small DC offset, no non-finite
/// samples, and an adjacent-delta distribution that does not contain digital
/// steps, which is what separates a quiet microphone from a broken one.
pub fn audio_probe_microphone(device_id: Option<String>, seconds: u64) -> Result<String> {
    let _com = ComGuard::new()?;
    let enumerator = DeviceEnumerator::new()?;
    let device = match device_id {
        Some(id) => enumerator.get_device(&id)?,
        None => enumerator.get_default_device(&Direction::Capture)?,
    };
    let mut client = device.get_iaudioclient()?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
    client.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: 1_000_000,
        },
    )?;
    let capture = client.get_audiocaptureclient()?;
    let buffer_bytes = client.get_buffer_size()? as usize * 8;
    let mut bytes = vec![0u8; buffer_bytes];
    client.start_stream()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut packets, mut frames, mut peak, mut nonfinite, mut dc) =
        (0u64, 0u64, 0.0f32, 0u64, 0.0f64);
    let (mut max_delta, mut steps, mut silent_packets) = (0.0f32, 0u64, 0u64);
    let mut previous = 0.0f32;
    let mut have_previous = false;
    let mut max_packet = 0u32;
    while Instant::now() < deadline {
        let mut progressed = false;
        while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let (count, info) = capture.read_from_device(&mut bytes)?;
            packets += 1;
            max_packet = max_packet.max(count);
            frames += u64::from(count);
            if info.flags.silent {
                silent_packets += 1;
            }
            for chunk in bytes[..count as usize * 8].chunks_exact(4) {
                let value = f32::from_le_bytes(chunk.try_into().unwrap());
                if !value.is_finite() {
                    nonfinite += 1;
                    continue;
                }
                peak = peak.max(value.abs());
                dc += f64::from(value);
                if have_previous {
                    let delta = (value - previous).abs();
                    max_delta = max_delta.max(delta);
                    if delta > 0.5 {
                        steps += 1;
                    }
                }
                previous = value;
                have_previous = true;
            }
            progressed = true;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(5));
        }
    }
    client.stop_stream()?;
    let samples = (frames * 2).max(1);
    Ok(format!(
        "packets={packets} silent_packets={silent_packets} frames={frames} max_packet_frames={max_packet} peak={peak:.6} dc={:.6} max_delta={max_delta:.6} digital_steps={steps} nonfinite={nonfinite} buffer_bytes={buffer_bytes}",
        dc / samples as f64
    ))
}

/// Diagnostic: capture the selected system render endpoint in loopback mode for
/// `seconds`, reporting whether the endpoint actually produces non-silent data.
/// Used by `opencap-bench audio-probe` to tell a silent endpoint apart from a
/// broken capture path.
pub fn audio_probe_loopback(device_id: Option<String>, seconds: u64) -> Result<String> {
    let _com = ComGuard::new()?;
    let enumerator = DeviceEnumerator::new()?;
    let device = match device_id {
        Some(id) => enumerator.get_device(&id)?,
        None => enumerator.get_default_device(&Direction::Render)?,
    };
    let mut client = device.get_iaudioclient()?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48000, 2, None);
    client.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: 1_000_000,
        },
    )?;
    let capture = client.get_audiocaptureclient()?;
    let buffer_bytes = client.get_buffer_size()? as usize * 8;
    let mut bytes = vec![0u8; buffer_bytes];
    client.start_stream()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut packets, mut silent_packets, mut samples_total, mut peak, mut nonfinite) =
        (0u64, 0u64, 0u64, 0.0f32, 0u64);
    let mut max_count = 0u32;
    while Instant::now() < deadline {
        let mut progressed = false;
        while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            let (count, info) = capture.read_from_device(&mut bytes)?;
            packets += 1;
            max_count = max_count.max(count);
            samples_total += u64::from(count) * 2;
            if info.flags.silent {
                silent_packets += 1;
            } else {
                for chunk in bytes[..count as usize * 8].chunks_exact(4) {
                    let value = f32::from_le_bytes(chunk.try_into().unwrap());
                    if !value.is_finite() {
                        nonfinite += 1;
                    } else {
                        peak = peak.max(value.abs());
                    }
                }
            }
            progressed = true;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(5));
        }
    }
    client.stop_stream()?;
    Ok(format!(
        "packets={packets} silent_packets={silent_packets} samples={samples_total} peak={peak:.6} nonfinite={nonfinite} max_packet_frames={max_count} buffer_bytes={buffer_bytes}"
    ))
}
