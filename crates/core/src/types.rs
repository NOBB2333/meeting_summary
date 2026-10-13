use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

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
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.width > 0 && self.height > 0 && self.stride >= self.width as usize * 4,
            "Invalid BGRA frame geometry"
        );
        anyhow::ensure!(
            self.data.len() >= self.stride * self.height as usize,
            "Truncated BGRA frame"
        );
        Ok(())
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
