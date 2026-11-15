use crate::{CaptureTarget, Rect};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Compile-time identity: the development application never reads production
/// settings merely because it was launched from Finder or a different shell.
pub const fn app_name() -> &'static str {
    if cfg!(feature = "dev-profile") {
        "OpenCap Dev"
    } else {
        "OpenCap"
    }
}

pub const fn is_dev_profile() -> bool {
    cfg!(feature = "dev-profile")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TranscriptionLanguage {
    #[default]
    Chinese,
    English,
    Auto,
}
impl TranscriptionLanguage {
    pub const ALL: [Self; 3] = [Self::Chinese, Self::English, Self::Auto];
    pub fn code(self) -> &'static str {
        match self {
            Self::Chinese => "zh",
            Self::English => "en",
            Self::Auto => "auto",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Chinese => "中文为主（含英文术语）",
            Self::English => "英文为主",
            Self::Auto => "自动（含中英日韩粤）",
        }
    }
}

/// Real-time recognition can prioritize low CPU or use a larger model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LiveAsrModel {
    #[default]
    Auto,
    Compact,
    Standard,
    Enhanced,
    Sentence,
}
impl LiveAsrModel {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Compact,
        Self::Standard,
        Self::Enhanced,
        Self::Sentence,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Compact => "轻量实时 · 14M",
            Self::Standard => "标准实时 · 中文",
            Self::Enhanced => "增强实时 · 中文",
            Self::Sentence => "逐句 · 使用文件模型",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OfflineAsrModel {
    #[default]
    SenseVoice,
    ParaformerSmall,
    ParaformerLarge,
}
impl OfflineAsrModel {
    pub const ALL: [Self; 3] = [
        Self::SenseVoice,
        Self::ParaformerSmall,
        Self::ParaformerLarge,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::SenseVoice => "SenseVoice · 多语言",
            Self::ParaformerSmall => "Paraformer · 轻量中英",
            Self::ParaformerLarge => "Paraformer · 标准中英",
        }
    }
}

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
    pub transcription_language: TranscriptionLanguage,
    pub transcription_context_revision: bool,
    /// Restore punctuation locally when a sentence or live hypothesis changes.
    pub transcription_punctuation: bool,
    pub live_asr_model: LiveAsrModel,
    pub offline_asr_model: OfflineAsrModel,
    /// Explicit opt-in; a trusted local NeMo library is never downloaded or
    /// loaded automatically. Model bytes are verified before initialization.
    pub online_speakers: bool,
    pub online_speaker_library: Option<PathBuf>,
    pub online_speaker_model: Option<PathBuf>,
    pub semantic_embedding_model: Option<String>,
    /// User-selected visual density in the meeting library.
    pub meeting_list_compact: bool,
    /// Group the meeting library by recording month instead of a flat list.
    pub meeting_group_by_month: bool,
    /// Group by ISO week instead; takes precedence if both flags are set.
    pub meeting_group_by_week: bool,
    pub meeting_media_on_right: bool,
    /// Collapse the main navigation to icons while retaining the current page.
    pub sidebar_collapsed: bool,
    /// Give the recording preview the full width when the configuration pane is hidden.
    pub recording_transcript_collapsed: bool,
    /// Explicitly reduce interface animation, independent of capture settings.
    pub reduce_motion: bool,
    pub show_floating_controls: bool,
    pub show_capture_border: bool,
    pub border_mode: BorderMode,
    pub show_recording_preview: bool,
    pub exclude_app: bool,
    pub cursor: bool,
    pub adaptive: bool,
    /// Trade software H.264/H.265 compression efficiency for lower CPU load.
    pub fast_software_encoding: bool,
    /// In automatic codec mode, prefer widely supported H.264 for lower load.
    pub prefer_h264: bool,
    /// Opt-in until the nonblocking WGC backend is tested on the target driver.
    pub experimental_async_wgc: bool,
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
            .and_then(|d| d.video_dir().map(|p| p.join(app_name())))
            .unwrap_or_else(|| {
                PathBuf::from(if is_dev_profile() {
                    "recordings-dev"
                } else {
                    "recordings"
                })
            });
        Self {
            target: CaptureTarget::Monitor(0),
            audio_only: false,
            output_dir,
            preset: Preset::Space,
            codec: Codec::Auto,
            encoder: None,
            system_audio: true,
            microphone: true,
            system_device: None,
            microphone_device: None,
            reserve_audio_track: false,
            separate_audio: true,
            mic_denoise: true,
            monitor_levels: false,
            realtime_transcription: true,
            transcription_hotwords: "录屏,语音转录,会议纪要,实时字幕".into(),
            transcription_language: TranscriptionLanguage::default(),
            // Live text stays responsive; the selected file model reviews each
            // finished sentence and replaces it. Missing review model simply
            // leaves the streaming text unchanged.
            transcription_context_revision: true,
            transcription_punctuation: true,
            live_asr_model: LiveAsrModel::Auto,
            offline_asr_model: OfflineAsrModel::SenseVoice,
            online_speakers: false,
            online_speaker_library: None,
            online_speaker_model: None,
            semantic_embedding_model: None,
            meeting_list_compact: false,
            meeting_group_by_month: true,
            meeting_group_by_week: false,
            meeting_media_on_right: false,
            sidebar_collapsed: false,
            recording_transcript_collapsed: false,
            reduce_motion: false,
            show_floating_controls: false,
            show_capture_border: !cfg!(target_os = "linux"),
            border_mode: BorderMode::OnStart,
            show_recording_preview: true,
            exclude_app: !cfg!(target_os = "linux"),
            cursor: true,
            adaptive: true,
            fast_software_encoding: false,
            prefer_h264: false,
            experimental_async_wgc: false,
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
    /// Explicit shortcut for older machines. Keep unrelated quality and audio
    /// choices intact; these changes are visible and can be adjusted again.
    pub fn reduce_recording_load(&mut self) {
        self.max_fps = self.max_fps.min(10);
        self.adaptive = true;
        self.fast_software_encoding = true;
        self.prefer_h264 = true;
        self.preset = Preset::Custom;
    }

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
        crate::meeting_rename::atomic_bytes(path, &serde_json::to_vec_pretty(self)?)
            .context("Saving configuration")
    }
}
pub fn config_path() -> PathBuf {
    directories::ProjectDirs::from("org", "OpenCap", app_name())
        .map(|d| d.config_local_dir().join("settings.json"))
        .unwrap_or_else(|| {
            PathBuf::from(if is_dev_profile() {
                "opencap-dev/settings.json"
            } else {
                "settings.json"
            })
        })
}

#[cfg(test)]
mod border_tests {
    use super::*;
    #[test]
    fn workspace_panels_default_open_and_remember_collapsed_state() {
        let mut config = RecordingConfig::default();
        assert!(!config.sidebar_collapsed && !config.recording_transcript_collapsed);
        config.sidebar_collapsed = true;
        config.recording_transcript_collapsed = true;
        let restored: RecordingConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(restored.sidebar_collapsed && restored.recording_transcript_collapsed);
    }
    #[test]
    fn fresh_config_enables_both_audio_sources_and_preview_without_overriding_saved_choices() {
        let defaults = RecordingConfig::default();
        assert!(defaults.microphone && defaults.system_audio && defaults.show_recording_preview);
        let mut saved = defaults;
        saved.microphone = false;
        saved.system_audio = false;
        saved.show_recording_preview = false;
        let restored: RecordingConfig =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(!restored.microphone && !restored.system_audio && !restored.show_recording_preview);
    }
    #[test]
    fn old_settings_enable_punctuation_and_explicit_disable_survives_reload() {
        let mut old = serde_json::to_value(RecordingConfig::default()).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("transcription_punctuation");
        let config: RecordingConfig = serde_json::from_value(old).unwrap();
        assert!(config.transcription_punctuation);
        let mut disabled = config;
        disabled.transcription_punctuation = false;
        let restored: RecordingConfig =
            serde_json::from_slice(&serde_json::to_vec(&disabled).unwrap()).unwrap();
        assert!(!restored.transcription_punctuation);
    }
    #[test]
    fn low_load_shortcut_preserves_quality_and_audio_and_never_raises_fps() {
        let mut config = RecordingConfig {
            max_fps: 30,
            quality: 20,
            codec: Codec::Hevc,
            microphone: true,
            ..Default::default()
        };
        config.reduce_recording_load();
        assert_eq!(config.max_fps, 10);
        assert!(config.adaptive);
        assert!(config.fast_software_encoding);
        assert!(config.prefer_h264);
        assert!(config.show_recording_preview);
        assert!(config.realtime_transcription);
        assert_eq!(
            (config.quality, config.codec, config.microphone),
            (20, Codec::Hevc, true)
        );
        assert_eq!(config.preset, Preset::Custom);
        let loaded: RecordingConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(loaded.max_fps, 10);
        assert!(loaded.realtime_transcription);
        config.max_fps = 5;
        config.reduce_recording_load();
        assert_eq!(config.max_fps, 5);
    }
    #[test]
    fn language_defaults_to_chinese_and_explicit_english_or_auto_survive_save() {
        let old: RecordingConfig = serde_json::from_str("{}").unwrap();
        assert!(!old.fast_software_encoding);
        assert!(!old.prefer_h264);
        assert!(!old.experimental_async_wgc);
        assert_eq!(old.live_asr_model, LiveAsrModel::Auto);
        assert!(old.transcription_context_revision);
        assert!(old.meeting_group_by_month);
        assert!(!old.meeting_group_by_week);
        assert_eq!(old.offline_asr_model, OfflineAsrModel::SenseVoice);
        assert_eq!(old.transcription_language, TranscriptionLanguage::Chinese);
        let weekly = RecordingConfig {
            meeting_group_by_month: false,
            meeting_group_by_week: true,
            ..Default::default()
        };
        let weekly: RecordingConfig =
            serde_json::from_str(&serde_json::to_string(&weekly).unwrap()).unwrap();
        assert!(weekly.meeting_group_by_week && !weekly.meeting_group_by_month);
        for language in TranscriptionLanguage::ALL {
            let config = RecordingConfig {
                transcription_language: language,
                ..Default::default()
            };
            let loaded: RecordingConfig =
                serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
            assert_eq!(loaded.transcription_language, language);
        }
        for live in LiveAsrModel::ALL {
            for offline in OfflineAsrModel::ALL {
                let config = RecordingConfig {
                    live_asr_model: live,
                    offline_asr_model: offline,
                    ..Default::default()
                };
                let loaded: RecordingConfig =
                    serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
                assert_eq!(
                    (loaded.live_asr_model, loaded.offline_asr_model),
                    (live, offline)
                );
            }
        }
    }
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
