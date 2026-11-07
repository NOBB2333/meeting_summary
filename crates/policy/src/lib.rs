use opencap_core::{Codec, RecordingConfig, VideoFrame};
use std::sync::{Arc, Weak};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    Static,
    Normal,
    High,
}
pub struct FramePolicy {
    previous: Vec<u8>,
    previous_frame: Weak<Vec<u8>>,
    previous_geometry: (u32, u32, usize),
    last_pts: Option<i64>,
    last_motion: i64,
    last_high: i64,
    pub motion: Motion,
}
impl Default for FramePolicy {
    fn default() -> Self {
        Self {
            previous: Vec::new(),
            previous_frame: Weak::new(),
            previous_geometry: (0, 0, 0),
            last_pts: None,
            last_motion: 0,
            last_high: 0,
            motion: Motion::Normal,
        }
    }
}
impl FramePolicy {
    pub fn accept(&mut self, frame: &VideoFrame, config: &RecordingConfig) -> bool {
        if !config.adaptive {
            self.previous.clear();
            self.previous_frame = Weak::new();
            return self.due(frame.pts_us, config.max_fps);
        }
        let geometry = (frame.width, frame.height, frame.stride);
        let unchanged_buffer = self.previous_geometry == geometry
            && self
                .previous_frame
                .upgrade()
                .is_some_and(|previous| Arc::ptr_eq(&previous, &frame.data));
        let changed = if unchanged_buffer {
            0.0
        } else {
            let mut thumb = Vec::with_capacity(80 * 45);
            for y in 0..45 {
                for x in 0..80 {
                    let i = (y * frame.height as usize / 45) * frame.stride
                        + (x * frame.width as usize / 80) * 4;
                    let d = &frame.data;
                    thumb.push(
                        ((d[i] as u16 * 29 + d[i + 1] as u16 * 150 + d[i + 2] as u16 * 77) >> 8)
                            as u8,
                    );
                }
            }
            let changed = if self.previous.len() == thumb.len() {
                thumb
                    .iter()
                    .zip(&self.previous)
                    .filter(|(a, b)| a.abs_diff(**b) > 8)
                    .count() as f64
                    / thumb.len() as f64
            } else {
                1.0
            };
            self.previous = thumb;
            self.previous_frame = Arc::downgrade(&frame.data);
            self.previous_geometry = geometry;
            changed
        };
        if changed > 0.12 {
            self.motion = Motion::High;
            self.last_motion = frame.pts_us;
            self.last_high = frame.pts_us;
        } else if changed > 0.001 {
            if self.motion != Motion::High || frame.pts_us - self.last_high > 500_000 {
                self.motion = Motion::Normal;
            }
            self.last_motion = frame.pts_us;
        } else if frame.pts_us - self.last_motion > 1_000_000 {
            self.motion = Motion::Static;
        }
        let fps = self.target_fps(config);
        self.due(frame.pts_us, fps)
    }
    fn target_fps(&self, config: &RecordingConfig) -> u32 {
        (match self.motion {
            Motion::Static => 1,
            Motion::Normal => 15,
            Motion::High => config.max_fps,
        })
        .min(config.max_fps)
    }
    /// The cached image still needs periodic output, including a static tail.
    /// New capture frames can wake the worker earlier and change the policy.
    pub fn next_due_us(&self, config: &RecordingConfig) -> Option<i64> {
        let fps = if config.adaptive {
            self.target_fps(config)
        } else {
            config.max_fps
        };
        self.last_pts.map(|pts| pts + 1_000_000 / fps as i64)
    }
    fn due(&mut self, pts_us: i64, fps: u32) -> bool {
        let due = self
            .last_pts
            .is_none_or(|p| pts_us - p >= 1_000_000 / fps as i64);
        // A change after a long static interval must be emitted immediately.
        if due {
            self.last_pts = Some(pts_us);
        }
        due
    }
}
pub fn candidates(config: &RecordingConfig) -> Vec<&str> {
    if let Some(name) = &config.encoder {
        return vec![name];
    }
    #[cfg(target_os = "macos")]
    return match config.codec {
        Codec::Auto | Codec::H264 => vec!["h264_videotoolbox", "libx264"],
        Codec::Hevc => vec!["hevc_videotoolbox", "libx265"],
        Codec::Av1 => vec!["libsvtav1"],
    };
    #[cfg(not(target_os = "macos"))]
    match config.codec {
        Codec::Auto if config.prefer_h264 => vec![
            "h264_qsv",
            "h264_nvenc",
            "h264_amf",
            "libx264",
            "hevc_qsv",
            "hevc_nvenc",
            "hevc_amf",
            "av1_qsv",
            "av1_nvenc",
            "av1_amf",
        ],
        Codec::Auto => vec![
            "av1_qsv",
            "av1_nvenc",
            "av1_amf",
            "hevc_qsv",
            "hevc_nvenc",
            "hevc_amf",
            "h264_qsv",
            "h264_nvenc",
            "h264_amf",
            "libx264",
        ],
        Codec::H264 => vec!["h264_qsv", "h264_nvenc", "h264_amf", "libx264"],
        Codec::Hevc => vec!["hevc_qsv", "hevc_nvenc", "hevc_amf", "libx265"],
        Codec::Av1 => vec!["av1_qsv", "av1_nvenc", "av1_amf", "libsvtav1"],
    }
}
pub fn prefer_adapter(mut candidates: Vec<&str>, vendor: Option<u32>) -> Vec<&str> {
    let preferred = match vendor {
        Some(0x8086) => "_qsv",
        Some(0x10de) => "_nvenc",
        Some(0x1002) => "_amf",
        _ => return candidates,
    };
    let family = |name: &str| {
        if name.starts_with("av1") || name == "libsvtav1" {
            0
        } else if name.starts_with("hevc") || name == "libx265" {
            1
        } else if name.starts_with("h264") || name == "libx264" {
            2
        } else {
            3
        }
    };
    let mut order = Vec::new();
    for name in &candidates {
        let codec = family(name);
        if !order.contains(&codec) {
            order.push(codec);
        }
    }
    candidates.sort_by_key(|name| {
        (
            order
                .iter()
                .position(|codec| *codec == family(name))
                .unwrap_or(order.len()),
            u8::from(!name.ends_with(preferred)),
        )
    });
    candidates
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn frame(pts: i64, value: u8) -> VideoFrame {
        VideoFrame {
            width: 80,
            height: 45,
            stride: 320,
            pts_us: pts,
            data: Arc::new(vec![value; 80 * 45 * 4]),
        }
    }
    #[test]
    fn static_reduces_work_and_motion_wakes_up() {
        let mut p = FramePolicy::default();
        let c = RecordingConfig::default();
        assert!(p.accept(&frame(0, 0), &c));
        assert!(p.accept(&frame(1_100_000, 0), &c));
        assert_eq!(p.motion, Motion::Static);
        assert!(!p.accept(&frame(1_150_000, 0), &c));
        assert!(p.accept(&frame(1_200_000, 255), &c));
        assert_eq!(p.motion, Motion::High);
    }
    #[test]
    fn reused_capture_frame_stays_static_and_new_pixels_wake_up() {
        let mut p = FramePolicy::default();
        let c = RecordingConfig::default();
        let mut cached = frame(0, 0);
        assert!(p.accept(&cached, &c));
        let thumbnail = p.previous.as_ptr();
        cached.pts_us = 1_100_000;
        assert!(p.accept(&cached, &c));
        assert_eq!(p.motion, Motion::Static);
        assert_eq!(
            p.previous.as_ptr(),
            thumbnail,
            "cached pixels were resampled"
        );
        cached.pts_us = 1_150_000;
        assert!(!p.accept(&cached, &c));
        assert!(p.accept(&frame(1_200_000, 255), &c));
        assert_eq!(p.motion, Motion::High);
    }
    #[test]
    fn fixed_fps_skips_motion_analysis() {
        let mut p = FramePolicy::default();
        let c = RecordingConfig {
            adaptive: false,
            ..Default::default()
        };
        assert!(p.accept(&frame(0, 0), &c));
        assert!(!p.accept(&frame(10_000, 255), &c));
        assert!(p.accept(&frame(40_000, 255), &c));
        assert!(p.previous.is_empty());
        assert_eq!(p.next_due_us(&c), Some(73_333));
    }
    #[test]
    fn explicit_encoder_is_never_silently_replaced() {
        let c = RecordingConfig {
            encoder: Some("libsvtav1".into()),
            ..Default::default()
        };
        assert_eq!(candidates(&c), vec!["libsvtav1"]);
    }
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn adapter_preference_keeps_codec_priority() {
        let c = RecordingConfig::default();
        let ordered = prefer_adapter(candidates(&c), Some(0x10de));
        assert_eq!(ordered[0], "av1_nvenc");
        assert_eq!(ordered[3], "hevc_nvenc");
        assert_eq!(ordered.last(), Some(&"libx264"));
    }
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn low_load_prioritizes_h264_but_preserves_explicit_choices() {
        let mut c = RecordingConfig {
            prefer_h264: true,
            ..Default::default()
        };
        let ordered = prefer_adapter(candidates(&c), Some(0x10de));
        assert_eq!(ordered[0], "h264_nvenc");
        assert!(
            ordered.iter().position(|name| *name == "libx264").unwrap()
                < ordered
                    .iter()
                    .position(|name| name.starts_with("hevc"))
                    .unwrap()
        );
        c.codec = Codec::Hevc;
        assert_eq!(
            prefer_adapter(candidates(&c), Some(0x10de))[0],
            "hevc_nvenc"
        );
        c.encoder = Some("libsvtav1".into());
        assert_eq!(candidates(&c), vec!["libsvtav1"]);
    }
}
