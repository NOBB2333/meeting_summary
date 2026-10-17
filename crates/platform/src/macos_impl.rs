use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Callback = unsafe extern "C" fn(*mut c_void, i32, *const c_void, usize, i32, i32, i32, i64);
unsafe extern "C" {
    fn oc_free(pointer: *mut c_void);
    fn oc_sources() -> *mut c_char;
    fn oc_devices() -> *mut c_char;
    fn oc_microphone_authorized() -> bool;
    fn oc_start(
        input: *const c_char,
        callback: Callback,
        user: *mut c_void,
        error: *mut *mut c_char,
    ) -> *mut c_void;
    fn oc_stop(handle: *mut c_void, microphone: bool);
    fn oc_window(view: isize, action: i32, value: bool) -> bool;
    fn oc_window_id(view: isize) -> isize;
    fn oc_window_region(view: isize) -> *mut c_char;
    fn oc_controls(
        input: *const c_char,
        callback: Callback,
        user: *mut c_void,
        error: *mut *mut c_char,
    ) -> *mut c_void;
    fn oc_controls_state(handle: *mut c_void, recording: bool);
    fn oc_controls_drop(handle: *mut c_void);
    fn oc_region(input: *const c_char, callback: Callback, user: *mut c_void) -> *mut c_void;
    fn oc_region_drop(handle: *mut c_void);
    fn oc_indicator(input: *const c_char, handle: *mut c_void) -> *mut c_void;
    fn oc_indicator_drop(handle: *mut c_void);
    fn oc_indicator_visible(handle: *mut c_void) -> bool;
    fn oc_preview(view: isize, source: u32) -> *mut c_void;
    fn oc_preview_place(handle: *mut c_void, input: *const c_char);
    fn oc_preview_drop(handle: *mut c_void);
    fn oc_canvas(width: i32, height: i32) -> *mut c_void;
    fn oc_canvas_rect(handle: *mut c_void, x: i32, y: i32, width: i32, height: i32, color: u32);
    fn oc_canvas_text(
        handle: *mut c_void,
        x: i32,
        y: i32,
        size: i32,
        color: u32,
        text: *const c_char,
    );
    fn oc_canvas_data(handle: *mut c_void) -> *const u8;
    fn oc_canvas_drop(handle: *mut c_void);
}
fn take_string(pointer: *mut c_char) -> Result<String> {
    anyhow::ensure!(!pointer.is_null(), "Apple API returned no result");
    let value = unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() };
    unsafe { oc_free(pointer.cast()) };
    Ok(value)
}
fn native_json(pointer: *mut c_char) -> Result<Value> {
    let result: Value = serde_json::from_str(&take_string(pointer)?)?;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        anyhow::bail!("{error}");
    }
    Ok(result)
}
fn cjson(value: Value) -> CString {
    CString::new(value.to_string()).unwrap()
}
pub fn targets() -> Result<Vec<TargetInfo>> {
    let sources = native_json(unsafe { oc_sources() })?;
    let mut result = Vec::new();
    for (key, monitor) in [("monitors", true), ("windows", false)] {
        for item in sources[key]
            .as_array()
            .context("Invalid Apple source list")?
        {
            let id = item["id"].as_u64().context("Missing source ID")?;
            result.push(TargetInfo {
                target: if monitor {
                    CaptureTarget::Monitor(id as usize)
                } else {
                    CaptureTarget::Window(id as isize)
                },
                name: item["name"].as_str().unwrap_or("Unknown").into(),
                width: item["width"].as_u64().unwrap_or(0) as u32,
                height: item["height"].as_u64().unwrap_or(0) as u32,
            });
        }
    }
    Ok(result)
}
pub fn audio_devices() -> Result<Vec<AudioDevice>> {
    let items = native_json(unsafe { oc_devices() })?;
    let mut devices = vec![AudioDevice {
        id: "system".into(),
        name: "系统声音（所有输出设备）".into(),
        kind: AudioKind::System,
    }];
    for item in items.as_array().context("Invalid microphone list")? {
        devices.push(AudioDevice {
            id: item["id"].as_str().context("Missing microphone ID")?.into(),
            name: item["name"].as_str().unwrap_or("麦克风").into(),
            kind: AudioKind::Microphone,
        });
    }
    Ok(devices)
}
pub fn microphone_permission_granted() -> bool {
    unsafe { oc_microphone_authorized() }
}
pub fn capture_vendor(_: &CaptureTarget) -> Option<u32> {
    None
}
pub fn capture_window_id(view: isize) -> isize {
    unsafe { oc_window_id(view) }
}
pub fn window_region(view: isize) -> Result<CaptureTarget> {
    let r = native_json(unsafe { oc_window_region(view) })?;
    let coordinate = |key: &str| -> Result<u32> {
        let n = r[key].as_i64().context("Invalid window bounds")?;
        anyhow::ensure!(
            n >= 0 && n <= u32::MAX as i64,
            "Window extends outside its display"
        );
        Ok(n as u32)
    };
    Ok(CaptureTarget::Region {
        monitor: r["monitor"].as_u64().context("Missing display")? as usize,
        rect: Rect {
            x: coordinate("x")?,
            y: coordinate("y")?,
            width: coordinate("width")?,
            height: coordinate("height")?,
        },
    })
}
pub const CAPTURE_PATH: &str = "ScreenCaptureKit -> CPU BGRA -> FFmpeg/VideoToolbox";

struct CaptureContext {
    clock: RecordingClock,
    video: Option<(Sender<VideoFrame>, Receiver<VideoFrame>, Arc<AtomicU64>)>,
    audio: Option<Sender<AudioChunk>>,
    error: Sender<String>,
    failed: Arc<AtomicBool>,
    dropped_audio: Arc<AtomicU64>,
    timeline: Mutex<AudioTimeline>,
}
unsafe extern "C" fn capture_callback(
    user: *mut c_void,
    kind: i32,
    data: *const c_void,
    count: usize,
    width: i32,
    height: i32,
    stride: i32,
    latency: i64,
) {
    // Apple invokes this synchronously with borrowed data. Copy before returning.
    // oc_stop drains callbacks before the boxed context is released.
    let state = unsafe { &*user.cast::<CaptureContext>() };
    if kind == -1 {
        state.failed.store(true, Ordering::Release);
        if !data.is_null() {
            let message = unsafe { CStr::from_ptr(data.cast()) }
                .to_string_lossy()
                .into_owned();
            let _ = state.error.try_send(message);
        }
        return;
    }
    if data.is_null() || state.clock.is_paused() {
        return;
    }
    let pts_us = (state.clock.now_us() - latency.clamp(0, 2_000_000)).max(0);
    if kind == 0 {
        let Some((tx, rx, dropped)) = &state.video else {
            return;
        };
        if width < 2
            || height < 2
            || stride < width * 4
            || count < stride as usize * height as usize
        {
            return;
        }
        let frame = VideoFrame {
            width: width as u32,
            height: height as u32,
            stride: stride as usize,
            pts_us,
            data: Arc::new(
                unsafe { std::slice::from_raw_parts(data.cast::<u8>(), count) }.to_vec(),
            ),
        };
        if let Err(crossbeam_channel::TrySendError::Full(frame)) = tx.try_send(frame) {
            let _ = rx.try_recv();
            dropped.fetch_add(1, Ordering::Relaxed);
            let _ = tx.try_send(frame);
        }
    } else if let Some(tx) = &state.audio {
        let Some(pts_us) = state
            .timeline
            .lock()
            .timestamp(&state.clock, latency, count / 2)
        else {
            return;
        };
        let chunk = AudioChunk {
            kind: if kind == 1 {
                AudioKind::System
            } else {
                AudioKind::Microphone
            },
            pts_us,
            samples: unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count) }.to_vec(),
        };
        if let Err(crossbeam_channel::TrySendError::Full(_)) = tx.try_send(chunk) {
            let n = state.dropped_audio.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                let _ = state
                    .error
                    .try_send("音频队列积压，部分采样丢失；采集会继续".into());
            }
        }
    }
}
struct NativeCapture {
    handle: *mut c_void,
    microphone: bool,
    _context: Box<CaptureContext>,
}
impl NativeCapture {
    fn start(options: Value, context: CaptureContext) -> Result<Self> {
        let mut context = Box::new(context);
        let mut error = std::ptr::null_mut();
        let input = cjson(options.clone());
        let handle = unsafe {
            oc_start(
                input.as_ptr(),
                capture_callback,
                (&mut *context as *mut CaptureContext).cast(),
                &mut error,
            )
        };
        if handle.is_null() {
            anyhow::bail!(
                "{}",
                take_string(error).unwrap_or_else(|_| "无法开始采集".into())
            );
        }
        Ok(Self {
            handle,
            microphone: options["microphone"].as_bool().unwrap_or(false),
            _context: context,
        })
    }
}
impl Drop for NativeCapture {
    fn drop(&mut self) {
        unsafe { oc_stop(self.handle, self.microphone) };
    }
}
// ScreenCaptureKit uses a private dispatch queue and its stop API is thread-safe.
unsafe impl Send for NativeCapture {}
pub struct MacCapture {
    native: Option<NativeCapture>,
    failed: Arc<AtomicBool>,
    warnings: Vec<String>,
}
impl MacCapture {
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn is_finished(&self) -> bool {
        self.native.is_none() || self.failed.load(Ordering::Acquire)
    }
}
impl CaptureBackend for MacCapture {
    fn stop(&mut self) -> Result<()> {
        self.native.take();
        Ok(())
    }
}
pub fn start_capture(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<VideoFrame>,
    rx: Receiver<VideoFrame>,
    errors: Sender<String>,
    dropped: Arc<AtomicU64>,
) -> Result<MacCapture> {
    let mut options =
        json!({"fps": config.max_fps, "cursor": config.cursor, "exclude_app": config.exclude_app});
    match config.target {
        CaptureTarget::Monitor(index) => options["monitor"] = json!(index),
        CaptureTarget::Window(id) => options["window"] = json!(id),
        CaptureTarget::Region { monitor, rect } => {
            options["monitor"] = json!(monitor);
            options["region"] = json!([rect.x, rect.y, rect.width, rect.height]);
        }
    }
    let failed = Arc::new(AtomicBool::new(false));
    let native = NativeCapture::start(
        options,
        CaptureContext {
            clock,
            video: Some((tx, rx, dropped)),
            audio: None,
            error: errors,
            failed: failed.clone(),
            dropped_audio: Arc::new(AtomicU64::new(0)),
            timeline: Mutex::new(AudioTimeline::default()),
        },
    )?;
    Ok(MacCapture {
        native: Some(native),
        failed,
        warnings: Vec::new(),
    })
}

struct AudioRoute {
    kind: AudioKind,
    device: Option<String>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for AudioRoute {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
pub struct MacAudio {
    routes: Vec<AudioRoute>,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
}
impl MacAudio {
    pub fn reconfigure(&mut self, routing: &AudioRouting) {
        self.routes
            .retain(|r| routing.enabled(r.kind) && routing.device(r.kind) == &r.device);
        for kind in [AudioKind::System, AudioKind::Microphone] {
            if !routing.enabled(kind) || self.routes.iter().any(|r| r.kind == kind) {
                continue;
            }
            let device = routing.device(kind).clone();
            let stop = Arc::new(AtomicBool::new(false));
            let (worker_stop, worker_device, clock, tx, warnings) = (
                stop.clone(),
                device.clone(),
                self.clock.clone(),
                self.tx.clone(),
                self.warnings.clone(),
            );
            let worker = thread::spawn(move || {
                let name = if kind == AudioKind::System {
                    "系统声音"
                } else {
                    "麦克风"
                };
                let options = json!({"audio_only": kind == AudioKind::System, "microphone": kind == AudioKind::Microphone, "device": worker_device});
                let failed = Arc::new(AtomicBool::new(false));
                match NativeCapture::start(
                    options,
                    CaptureContext {
                        clock,
                        video: None,
                        audio: Some(tx),
                        error: warnings.clone(),
                        failed: failed.clone(),
                        dropped_audio: Arc::new(AtomicU64::new(0)),
                        timeline: Mutex::new(AudioTimeline::default()),
                    },
                ) {
                    Ok(_native) => {
                        while !worker_stop.load(Ordering::Acquire)
                            && !failed.load(Ordering::Acquire)
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Err(e) => {
                        let _ = warnings.try_send(format!(
                            "{name}不可用，该路补静音；请检查权限或重新开启：{e:#}"
                        ));
                    }
                }
            });
            self.routes.push(AudioRoute {
                kind,
                device,
                stop,
                worker: Some(worker),
            });
        }
    }
}
impl AudioBackend for MacAudio {
    fn stop(&mut self) -> Result<()> {
        self.routes.clear();
        Ok(())
    }
}
impl Drop for MacAudio {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn start_audio(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<AudioChunk>,
    warnings: Sender<String>,
) -> MacAudio {
    let mut audio = MacAudio {
        routes: Vec::new(),
        clock,
        tx,
        warnings,
    };
    audio.reconfigure(&config.audio_routing());
    audio
}
pub struct AudioMonitor {
    audio: MacAudio,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    state: Arc<Mutex<[(f32, Instant); 2]>>,
    denoise: Arc<AtomicBool>,
    warnings: Receiver<String>,
}
impl AudioMonitor {
    pub fn set_routing(&mut self, routing: &AudioRouting, denoise: bool) {
        self.denoise.store(denoise, Ordering::Release);
        self.audio.reconfigure(routing);
        let mut state = self.state.lock();
        for (i, kind) in [AudioKind::System, AudioKind::Microphone]
            .into_iter()
            .enumerate()
        {
            if !routing.enabled(kind) {
                state[i].0 = 0.0;
            }
        }
    }
    pub fn peaks(&self) -> (f32, f32) {
        let s = self.state.lock();
        let peak = |i: usize| s[i].0 * (-s[i].1.elapsed().as_secs_f32() / 0.35).exp();
        (peak(0), peak(1))
    }
    pub fn take_warning(&self) -> Option<String> {
        self.warnings.try_recv().ok()
    }
}
impl Drop for AudioMonitor {
    fn drop(&mut self) {
        let _ = self.audio.stop();
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
pub fn start_audio_monitor(routing: &AudioRouting, denoise: bool) -> Result<AudioMonitor> {
    anyhow::ensure!(
        routing.system_audio || routing.microphone,
        "没有开启任何声音来源"
    );
    let (tx, rx) = bounded::<AudioChunk>(256);
    let (warnings, warning_rx) = bounded(16);
    let state = Arc::new(Mutex::new([(0.0, Instant::now()); 2]));
    let stop = Arc::new(AtomicBool::new(false));
    let (shared, stopping) = (state.clone(), stop.clone());
    let denoise = Arc::new(AtomicBool::new(denoise));
    let worker_denoise = denoise.clone();
    let worker = thread::spawn(move || {
        let mut mic = MicDenoiser::new(48_000);
        let mut enabled = worker_denoise.load(Ordering::Acquire);
        while !stopping.load(Ordering::Acquire) {
            if let Ok(mut chunk) = rx.recv_timeout(Duration::from_millis(50)) {
                let next = worker_denoise.load(Ordering::Acquire);
                if next != enabled {
                    enabled = next;
                    mic = MicDenoiser::new(48_000);
                }
                if enabled && chunk.kind == AudioKind::Microphone {
                    mic.process(&mut chunk.samples);
                }
                let index = usize::from(chunk.kind == AudioKind::Microphone);
                let peak = chunk
                    .samples
                    .iter()
                    .filter(|v| v.is_finite())
                    .fold(0.0f32, |p, v| p.max(v.abs().min(1.0)));
                let mut s = shared.lock();
                s[index].0 =
                    peak.max(s[index].0 * (-s[index].1.elapsed().as_secs_f32() / 0.35).exp());
                s[index].1 = Instant::now();
            }
        }
    });
    let config = RecordingConfig {
        system_audio: routing.system_audio,
        microphone: routing.microphone,
        system_device: routing.system_device.clone(),
        microphone_device: routing.microphone_device.clone(),
        ..Default::default()
    };
    let audio = start_audio(&config, RecordingClock::new(), tx, warnings);
    Ok(AudioMonitor {
        audio,
        stop,
        worker: Some(worker),
        state,
        denoise,
        warnings: warning_rx,
    })
}
fn audio_probe(kind: AudioKind, device: Option<String>, seconds: u64) -> Result<String> {
    let (tx, rx) = unbounded();
    let (warnings, errors) = unbounded();
    let failed = Arc::new(AtomicBool::new(false));
    let _native = NativeCapture::start(
        json!({"audio_only": kind == AudioKind::System, "microphone": kind == AudioKind::Microphone, "device": device}),
        CaptureContext {
            clock: RecordingClock::new(),
            video: None,
            audio: Some(tx),
            error: warnings,
            failed,
            dropped_audio: Arc::new(AtomicU64::new(0)),
            timeline: Mutex::new(AudioTimeline::default()),
        },
    )?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut packets, mut frames, mut nonfinite, mut peak, mut sum) =
        (0u64, 0u64, 0u64, 0.0f32, 0.0f64);
    while Instant::now() < deadline {
        if let Ok(error) = errors.try_recv() {
            anyhow::bail!("{error}");
        }
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            packets += 1;
            frames += chunk.samples.len() as u64 / 2;
            for v in chunk.samples {
                if v.is_finite() {
                    peak = peak.max(v.abs());
                    sum += f64::from(v);
                } else {
                    nonfinite += 1;
                }
            }
        }
    }
    Ok(format!(
        "packets={packets} frames={frames} peak={peak:.6} dc={:.6} nonfinite={nonfinite}",
        sum / (frames * 2).max(1) as f64
    ))
}
pub fn audio_probe_microphone(device: Option<String>, seconds: u64) -> Result<String> {
    audio_probe(AudioKind::Microphone, device, seconds)
}
pub fn audio_probe_loopback(device: Option<String>, seconds: u64) -> Result<String> {
    audio_probe(AudioKind::System, device, seconds)
}
pub fn audio_diagnostics() -> String {
    "ScreenCaptureKit system audio + CoreAudio microphone (48 kHz stereo)".into()
}

#[derive(Debug, Clone)]
pub enum ShellEvent {
    Show,
    Record,
    Pause,
    OpenFolder,
    Quit,
    Locked,
    Warning(String),
}
unsafe extern "C" fn shell_callback(
    user: *mut c_void,
    kind: i32,
    _: *const c_void,
    _: usize,
    _: i32,
    _: i32,
    _: i32,
    _: i64,
) {
    let tx = unsafe { &*user.cast::<Sender<ShellEvent>>() };
    let event = match kind {
        1 => ShellEvent::Record,
        2 => ShellEvent::Pause,
        3 => ShellEvent::OpenFolder,
        4 => ShellEvent::Quit,
        5 => ShellEvent::Locked,
        _ => ShellEvent::Show,
    };
    let _ = tx.try_send(event);
}
pub struct DesktopControls {
    handle: *mut c_void,
    pub events: Receiver<ShellEvent>,
    _sender: Box<Sender<ShellEvent>>,
}
impl DesktopControls {
    pub fn set_recording_state(&self, value: bool) {
        unsafe { oc_controls_state(self.handle, value) };
    }
}
impl Drop for DesktopControls {
    fn drop(&mut self) {
        unsafe { oc_controls_drop(self.handle) };
    }
}
pub fn desktop_controls(record: &str, pause: &str) -> Result<DesktopControls> {
    let (tx, events) = unbounded();
    let mut sender = Box::new(tx);
    let mut error = std::ptr::null_mut();
    let input = cjson(json!({"record": record, "pause": pause}));
    let handle = unsafe {
        oc_controls(
            input.as_ptr(),
            shell_callback,
            (&mut *sender as *mut Sender<ShellEvent>).cast(),
            &mut error,
        )
    };
    if handle.is_null() {
        anyhow::bail!("{}", take_string(error)?);
    }
    Ok(DesktopControls {
        handle,
        events,
        _sender: sender,
    })
}
pub fn set_app_icon(_: isize, _: bool) {}
pub fn set_capture_exclusion(view: isize, value: bool) -> Result<()> {
    unsafe {
        oc_window(view, 2, value);
    }
    Ok(())
}
pub fn show_native_window(view: isize, value: bool) {
    unsafe {
        oc_window(view, 0, value);
    }
}
pub fn show_controls_without_activation(view: isize, value: bool) {
    unsafe {
        oc_window(view, 1, value);
    }
}
pub fn native_window_visible(view: isize) -> bool {
    unsafe { oc_window(view, 4, false) }
}
pub fn configure_floating_controls(view: isize, _: isize) -> Result<()> {
    unsafe {
        oc_window(view, 3, false);
    }
    Ok(())
}
pub fn open_path(path: &Path) -> Result<()> {
    anyhow::ensure!(
        std::process::Command::new("open")
            .arg(path)
            .status()?
            .success(),
        "无法打开路径"
    );
    Ok(())
}
pub fn desktop_available() -> bool {
    true
} // Session locking is reported by native notifications.
pub struct ProcessMeter {
    previous: f64,
    time: Instant,
}
impl Default for ProcessMeter {
    fn default() -> Self {
        Self {
            previous: 0.0,
            time: Instant::now(),
        }
    }
}
impl ProcessMeter {
    pub fn sample(&mut self) -> (Option<f64>, Option<u64>) {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return (None, None);
        }
        let usage = unsafe { usage.assume_init() };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
        let cpu = seconds(usage.ru_utime) + seconds(usage.ru_stime);
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;
        let percent =
            (cpu - self.previous) / self.time.elapsed().as_secs_f64().max(0.001) / cores * 100.0;
        self.previous = cpu;
        self.time = Instant::now();
        (Some(percent), Some(usage.ru_maxrss as u64))
    }
}

#[derive(Debug)]
pub enum RegionEvent {
    Changed(Rect),
    Start(Rect),
    Closed,
}
unsafe extern "C" fn region_callback(
    user: *mut c_void,
    kind: i32,
    _: *const c_void,
    _: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i64,
) {
    let tx = unsafe { &*user.cast::<Sender<RegionEvent>>() };
    let rect = Rect {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
    };
    let _ = tx.try_send(match kind {
        1 => RegionEvent::Start(rect),
        2 => RegionEvent::Closed,
        _ => RegionEvent::Changed(rect),
    });
}
pub struct RegionSelection {
    handle: *mut c_void,
    pub events: Receiver<RegionEvent>,
    _sender: Box<Sender<RegionEvent>>,
}
impl Drop for RegionSelection {
    fn drop(&mut self) {
        unsafe { oc_region_drop(self.handle) };
    }
}
pub fn edit_region(monitor: usize, initial: Option<Rect>) -> Result<RegionSelection> {
    let (tx, events) = unbounded();
    let mut sender = Box::new(tx);
    let input = cjson(json!({"monitor": monitor, "initial": initial}));
    let handle = unsafe {
        oc_region(
            input.as_ptr(),
            region_callback,
            (&mut *sender as *mut Sender<RegionEvent>).cast(),
        )
    };
    anyhow::ensure!(!handle.is_null(), "无法打开区域选择器");
    Ok(RegionSelection {
        handle,
        events,
        _sender: sender,
    })
}
pub struct CaptureIndicator {
    handle: *mut c_void,
    visible: bool,
}
impl CaptureIndicator {
    pub fn new() -> Result<Self> {
        Ok(Self {
            handle: std::ptr::null_mut(),
            visible: false,
        })
    }
    pub fn update(&mut self, target: &CaptureTarget, paused: bool) -> Result<()> {
        let input = cjson(json!({"target": target, "paused": paused}));
        self.handle = unsafe { oc_indicator(input.as_ptr(), self.handle) };
        self.visible = !self.handle.is_null() && unsafe { oc_indicator_visible(self.handle) };
        Ok(())
    }
    pub fn hide(&mut self) {
        if !self.handle.is_null() {
            unsafe { oc_indicator_drop(self.handle) };
            self.handle = std::ptr::null_mut();
        }
        self.visible = false;
    }
    pub fn visible(&self) -> bool {
        self.visible
    }
}
impl Drop for CaptureIndicator {
    fn drop(&mut self) {
        self.hide();
    }
}
pub struct WindowPreview {
    handle: *mut c_void,
}
impl WindowPreview {
    pub fn new(view: isize, source: isize) -> Result<Self> {
        let handle = unsafe { oc_preview(view, source as u32) };
        anyhow::ensure!(!handle.is_null(), "窗口缩略图不可用");
        Ok(Self { handle })
    }
    pub fn minimized(&self) -> bool {
        false
    }
    pub fn hide(&self) {
        self.place([0; 4], [0; 4]).ok();
    }
    pub fn place(&self, area: [i32; 4], clip: [i32; 4]) -> Result<()> {
        let input = cjson(json!({"area": area, "clip": clip}));
        unsafe { oc_preview_place(self.handle, input.as_ptr()) };
        Ok(())
    }
}
impl Drop for WindowPreview {
    fn drop(&mut self) {
        unsafe { oc_preview_drop(self.handle) };
    }
}
pub struct FixtureCanvas {
    handle: *mut c_void,
    pub width: u32,
    pub height: u32,
}
impl FixtureCanvas {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let handle = unsafe { oc_canvas(width as i32, height as i32) };
        anyhow::ensure!(!handle.is_null(), "无法创建基准画布");
        Ok(Self {
            handle,
            width,
            height,
        })
    }
    pub fn rect(&self, x: i32, y: i32, width: i32, height: i32, color: u32) {
        unsafe { oc_canvas_rect(self.handle, x, y, width, height, color) };
    }
    pub fn text(&self, x: i32, y: i32, size: i32, color: u32, text: &str) {
        if let Ok(text) = CString::new(text) {
            unsafe { oc_canvas_text(self.handle, x, y, size, color, text.as_ptr()) };
        }
    }
    pub fn frame(&self, pts_us: i64) -> VideoFrame {
        let data = unsafe {
            std::slice::from_raw_parts(
                oc_canvas_data(self.handle),
                self.width as usize * self.height as usize * 4,
            )
        }
        .to_vec();
        VideoFrame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            pts_us,
            data: Arc::new(data),
        }
    }
}
impl Drop for FixtureCanvas {
    fn drop(&mut self) {
        unsafe { oc_canvas_drop(self.handle) };
    }
}
