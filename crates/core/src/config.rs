use crate::{CaptureTarget, Rect};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preset {
    #[default]
    Space,
    Balanced,
    Quality,
    Custom,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Codec {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BorderMode {
    Always,
    #[default]
    OnStart,
    Never,
}
impl BorderMode {
    pub fn visible(self, elapsed: std::time::Duration) -> bool {
        match self {
            Self::Always => true,
            Self::OnStart => elapsed < std::time::Duration::from_secs(3),
            Self::Never => false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    pub target: CaptureTarget,
    /// Capture audio only; never create a screen capture or video encoder.
    pub audio_only: bool,
    pub output_dir: PathBuf,
    pub preset: Preset,
    pub codec: Codec,
    pub encoder: Option<String>,
    pub system_audio: bool,
    pub microphone: bool,
    pub system_device: Option<String>,
    pub microphone_device: Option<String>,
    /// Reserve an AAC track even when both routes start muted, allowing live enabling.
    pub reserve_audio_track: bool,
    /// Also save the mixed recording audio to a same-name M4A without re-encoding.
    pub separate_audio: bool,
    /// Apply the built-in voice chain (high-pass, noise gate, compressor) to the microphone.
    pub mic_denoise: bool,
    /// Meter the selected devices while nothing is being recorded.
    pub monitor_levels: bool,
    pub realtime_transcription: bool,
    pub transcription_hotwords: String,
    pub transcription_context_revision: bool,
    pub show_floating_controls: bool,
    pub show_capture_border: bool,
    pub border_mode: BorderMode,
    pub show_recording_preview: bool,
    pub exclude_app: bool,
    pub cursor: bool,
    pub adaptive: bool,
    pub max_fps: u32,
    pub quality: u32,
    pub max_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub audio_channels: u16,
    pub gop_seconds: u32,
    pub encoder_options: BTreeMap<String, String>,
    pub hotkey_record: String,
    pub hotkey_pause: String,
}
impl Default for RecordingConfig {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|d| d.video_dir().map(|p| p.join("OpenCap")))
            .unwrap_or_else(|| PathBuf::from("recordings"));
        Self {
            target: CaptureTarget::Monitor(0),
            audio_only: false,
            output_dir,
            preset: Preset::Space,
            codec: Codec::Auto,
            encoder: None,
            system_audio: true,
            microphone: false,
            system_device: None,
            microphone_device: None,
            reserve_audio_track: false,
            separate_audio: true,
            mic_denoise: true,
            monitor_levels: false,
            realtime_transcription: true,
            transcription_hotwords: "录屏,语音转录,会议纪要,实时字幕".into(),
            transcription_context_revision: false,
            show_floating_controls: false,
            show_capture_border: !cfg!(target_os = "linux"),
            border_mode: BorderMode::OnStart,
            show_recording_preview: true,
            exclude_app: !cfg!(target_os = "linux"),
            cursor: true,
            adaptive: true,
            max_fps: 30,
            quality: 30,
            max_bitrate_kbps: 1600,
            audio_bitrate_kbps: 48,
            audio_channels: 1,
            gop_seconds: 5,
            encoder_options: BTreeMap::new(),
            hotkey_record: "Ctrl+Alt+F9".into(),
            hotkey_pause: "Ctrl+Alt+F10".into(),
        }
    }
}
impl RecordingConfig {
    /// Keep old saved configurations with the border switch off working.
    pub fn effective_border_mode(&self) -> BorderMode {
        if self.show_capture_border {
            self.border_mode
        } else {
            BorderMode::Never
        }
    }
    pub fn audio_routing(&self) -> crate::AudioRouting {
        crate::AudioRouting {
            system_audio: self.system_audio,
            microphone: self.microphone,
            system_device: self.system_device.clone(),
            microphone_device: self.microphone_device.clone(),
        }
    }
    pub fn apply_preset(&mut self, preset: Preset) {
        self.preset = preset;
        match preset {
            Preset::Space => {
                self.quality = 30;
                self.max_bitrate_kbps = 1600;
                self.audio_bitrate_kbps = 48;
                self.audio_channels = 1;
                self.adaptive = true;
                self.max_fps = 30;
            }
            Preset::Balanced => {
                self.quality = 25;
                self.max_bitrate_kbps = 4000;
                self.audio_bitrate_kbps = 96;
                self.audio_channels = 2;
                self.adaptive = true;
                self.max_fps = 30;
            }
            Preset::Quality => {
                self.quality = 20;
                self.max_bitrate_kbps = 10000;
                self.audio_bitrate_kbps = 128;
                self.audio_channels = 2;
                self.adaptive = false;
                self.max_fps = 30;
            }
            Preset::Custom => {}
        }
    }
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=60).contains(&self.max_fps),
            "FPS must be between 1 and 60"
        );
        anyhow::ensure!(
            (1..=51).contains(&self.quality),
            "Quality must be between 1 and 51"
        );
        anyhow::ensure!(
            (100..=100000).contains(&self.max_bitrate_kbps),
            "Invalid bitrate cap"
        );
        anyhow::ensure!(
            (16..=320).contains(&self.audio_bitrate_kbps) && [1, 2].contains(&self.audio_channels),
            "Invalid audio configuration"
        );
        anyhow::ensure!(
            (1..=30).contains(&self.gop_seconds),
            "GOP must be between 1 and 30 seconds"
        );
        if let CaptureTarget::Region {
            rect: Rect { width, height, .. },
            ..
        } = self.target
            && !self.audio_only
        {
            anyhow::ensure!(
                width >= 2 && height >= 2,
                "Region must be at least 2 x 2 pixels"
            );
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let s: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        s.validate()?;
        Ok(s)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path).context("Saving configuration")
    }
}
pub fn config_path() -> PathBuf {
    directories::ProjectDirs::from("org", "OpenCap", "OpenCap")
        .map(|d| d.config_local_dir().join("settings.json"))
        .unwrap_or_else(|| PathBuf::from("settings.json"))
}

#[cfg(test)]
mod border_tests {
    use super::*;
    #[test]
    fn legacy_switch_and_timed_border() {
        let old: RecordingConfig =
            serde_json::from_str(r#"{"show_capture_border":false}"#).unwrap();
        assert!(!old.audio_only);
        assert_eq!(old.effective_border_mode(), BorderMode::Never);
        assert!(
            !old.effective_border_mode()
                .visible(std::time::Duration::ZERO)
        );
        assert!(BorderMode::OnStart.visible(std::time::Duration::from_millis(2999)));
        assert!(!BorderMode::OnStart.visible(std::time::Duration::from_secs(3)));
        assert!(BorderMode::Always.visible(std::time::Duration::MAX));
    }
}
