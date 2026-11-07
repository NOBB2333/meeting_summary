use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaptureTarget {
    Monitor(usize),
    Window(isize),
    Region { monitor: usize, rect: Rect },
}
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub target: CaptureTarget,
    pub name: String,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub kind: AudioKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioKind {
    System,
    Microphone,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioRouting {
    pub system_audio: bool,
    pub microphone: bool,
    pub system_device: Option<String>,
    pub microphone_device: Option<String>,
}
impl AudioRouting {
    pub fn enabled(&self, kind: AudioKind) -> bool {
        match kind {
            AudioKind::System => self.system_audio,
            AudioKind::Microphone => self.microphone,
        }
    }
    pub fn device(&self, kind: AudioKind) -> &Option<String> {
        match kind {
            AudioKind::System => &self.system_device,
            AudioKind::Microphone => &self.microphone_device,
        }
    }
}
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub pts_us: i64,
    pub data: Arc<Vec<u8>>,
}
impl VideoFrame {
    /// Some libswscale paths can read up to 16 bytes past an image plane.
    /// Padding lets the encoder use these owned pixels without another frame copy.
    pub const PADDING: usize = 16;

    pub fn copy_pixels(pixels: &[u8]) -> Arc<Vec<u8>> {
        let mut data = Vec::with_capacity(pixels.len().saturating_add(Self::PADDING));
        data.extend_from_slice(pixels);
        data.resize(data.len() + Self::PADDING, 0);
        Arc::new(data)
    }

    pub fn validate(&self) -> Result<()> {
        let row_bytes = (self.width as usize).checked_mul(4);
        let image_bytes = self.stride.checked_mul(self.height as usize);
        anyhow::ensure!(
            self.width > 0
                && self.height > 0
                && row_bytes.is_some_and(|width| self.stride >= width),
            "Invalid BGRA frame geometry"
        );
        anyhow::ensure!(
            image_bytes.is_some_and(|size| self.data.len() >= size),
            "Truncated BGRA frame"
        );
        Ok(())
    }
}

/// Cheap, bounded counters. Platform callbacks never perform file IO to log a frame.
#[derive(Default)]
pub struct CaptureMetrics {
    pub callbacks: AtomicU64,
    pub rate_skips: AtomicU64,
    pub readbacks: AtomicU64,
    pub readback_bytes: AtomicU64,
    pub readback_max_us: AtomicU64,
    pub copy_submit_max_us: AtomicU64,
    pub map_call_max_us: AtomicU64,
    pub map_not_ready: AtomicU64,
    pub staging_full: AtomicU64,
    adapter_known: AtomicBool,
    adapter_luid: AtomicU64,
    adapter_vendor: AtomicU64,
    feature_level: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureMetricsSnapshot {
    pub callbacks: u64,
    pub rate_skips: u64,
    pub readbacks: u64,
    pub readback_bytes: u64,
    pub readback_max_us: u64,
    pub copy_submit_max_us: u64,
    pub map_call_max_us: u64,
    pub map_not_ready: u64,
    pub staging_full: u64,
    pub adapter_luid: Option<String>,
    pub adapter_vendor_id: Option<u64>,
    pub d3d_feature_level: Option<u64>,
}

impl CaptureMetrics {
    pub fn set_adapter(&self, vendor: u32, luid: u64, feature_level: u32) {
        self.adapter_vendor
            .store(u64::from(vendor), Ordering::Relaxed);
        self.adapter_luid.store(luid, Ordering::Relaxed);
        self.feature_level
            .store(u64::from(feature_level), Ordering::Relaxed);
        self.adapter_known.store(true, Ordering::Release);
    }
    pub fn record_readback(&self, bytes: usize, elapsed: Duration) {
        self.readbacks.fetch_add(1, Ordering::Relaxed);
        self.readback_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.readback_max_us
            .fetch_max(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    /// Start recording statistics after encoder probing, without clearing the
    /// device identity discovered during capture startup.
    pub fn reset_counters(&self) {
        for counter in [
            &self.callbacks,
            &self.rate_skips,
            &self.readbacks,
            &self.readback_bytes,
            &self.readback_max_us,
            &self.copy_submit_max_us,
            &self.map_call_max_us,
            &self.map_not_ready,
            &self.staging_full,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> CaptureMetricsSnapshot {
        let get = |value: &AtomicU64| value.load(Ordering::Relaxed);
        let known = self.adapter_known.load(Ordering::Acquire);
        CaptureMetricsSnapshot {
            callbacks: get(&self.callbacks),
            rate_skips: get(&self.rate_skips),
            readbacks: get(&self.readbacks),
            readback_bytes: get(&self.readback_bytes),
            readback_max_us: get(&self.readback_max_us),
            copy_submit_max_us: get(&self.copy_submit_max_us),
            map_call_max_us: get(&self.map_call_max_us),
            map_not_ready: get(&self.map_not_ready),
            staging_full: get(&self.staging_full),
            adapter_luid: known.then(|| format!("{:016x}", get(&self.adapter_luid))),
            adapter_vendor_id: known.then(|| get(&self.adapter_vendor)),
            d3d_feature_level: known.then(|| get(&self.feature_level)),
        }
    }
}
#[derive(Clone)]
pub struct AudioChunk {
    pub kind: AudioKind,
    pub pts_us: i64,
    pub samples: Vec<f32>, /* interleaved stereo 48 kHz */
}

/// Owned PCM on the recording media timeline. Observers cannot stall recording.
#[derive(Debug, Clone)]
pub struct PcmAudio {
    pub pts_us: i64,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

/// Future backends may expose owned GPU buffers; CPU frames are the portable v1 path.
pub trait CaptureBackend: Send {
    fn stop(&mut self) -> Result<()>;
}
pub trait AudioBackend: Send {
    fn stop(&mut self) -> Result<()>;
}
pub trait VideoEncoder {
    type Packet;
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<Self::Packet>>;
    fn flush(&mut self) -> Result<Vec<Self::Packet>>;
}
pub trait Muxer<P> {
    fn write_packet(&mut self, packet: &mut P) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RecordingState {
    #[default]
    Idle,
    Starting,
    Recording,
    Paused,
    Stopping,
    Finished,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordingStats {
    pub state: RecordingState,
    pub duration_seconds: f64,
    pub encoded_frames: u64,
    pub dropped_frames: u64,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub bytes: u64,
    pub encoder: String,
    pub cpu_percent: Option<f64>,
    pub gpu_percent: Option<f64>,
    pub peak_memory_bytes: Option<u64>,
    pub av_offset_ms: Option<f64>,
    pub output: Option<PathBuf>,
    pub audio_output: Option<PathBuf>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub audio_routing: AudioRouting,
    pub system_peak: f32,
    pub microphone_peak: f32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderCapabilities {
    pub name: String,
    pub codec: String,
    pub available: bool,
    pub hardware: bool,
    pub reason: Option<String>,
}
