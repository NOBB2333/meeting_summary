//! Bounded preview of frames submitted to the encoder; never starts another capture.
use opencap_core::VideoFrame;
use parking_lot::Mutex;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct RecordingPreview {
    pub width: u32,
    pub height: u32,
    /// Packed BGRA with opaque alpha, also used by GPUI RenderImage.
    pub bgra: Vec<u8>,
    pub pts_us: i64,
}
#[derive(Default)]
pub(crate) struct PreviewSlot {
    enabled: AtomicBool,
    frame: Mutex<Option<RecordingPreview>>,
}
impl PreviewSlot {
    pub fn enable(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.frame.lock().take();
        }
    }
    pub fn take(&self) -> Option<RecordingPreview> {
        self.frame.lock().take()
    }
    pub fn publish(&self, frame: &VideoFrame, last: &mut Option<Instant>) {
        if !self.enabled.load(Ordering::Relaxed)
            || last.is_some_and(|v| v.elapsed() < Duration::from_millis(200))
        {
            return;
        }
        let preview = make_preview(frame);
        *self.frame.lock() = Some(preview);
        *last = Some(Instant::now());
    }
}
fn make_preview(frame: &VideoFrame) -> RecordingPreview {
    let scale = (640.0 / frame.width as f64)
        .min(360.0 / frame.height as f64)
        .min(1.0);
    let width = (frame.width as f64 * scale).round().max(1.) as u32;
    let height = (frame.height as f64 * scale).round().max(1.) as u32;
    let mut bgra = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let src = (y as u64 * frame.height as u64 / height as u64) as usize * frame.stride
                + (x as u64 * frame.width as u64 / width as u64) as usize * 4;
            let dst = (y as usize * width as usize + x as usize) * 4;
            bgra[dst..dst + 3].copy_from_slice(&frame.data[src..src + 3]);
            bgra[dst + 3] = 255;
        }
    }
    RecordingPreview {
        width,
        height,
        bgra,
        pts_us: frame.pts_us,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn preview_is_bounded_preserves_bgra_and_can_be_disabled() {
        let frame = VideoFrame {
            width: 720,
            height: 1280,
            stride: 720 * 4,
            pts_us: 123,
            data: Arc::new([11, 22, 33, 0].repeat(720 * 1280)),
        };
        let slot = PreviewSlot::default();
        let mut last = None;
        slot.publish(&frame, &mut last);
        assert!(slot.take().is_none());
        slot.enable(true);
        slot.publish(&frame, &mut last);
        let p = slot.take().unwrap();
        assert_eq!((p.width, p.height), (203, 360));
        assert_eq!(&p.bgra[..4], &[11, 22, 33, 255]);
        assert_eq!(p.pts_us, 123);
        slot.publish(&frame, &mut last);
        assert!(slot.take().is_none());
        slot.publish(&frame, &mut None);
        slot.enable(false);
        assert!(slot.take().is_none());
    }
}
