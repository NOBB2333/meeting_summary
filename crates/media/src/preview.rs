//! Bounded preview of frames submitted to the encoder; never starts another capture.
use opencap_core::VideoFrame;
use parking_lot::Mutex;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const PREVIEW_INTERVAL: Duration = Duration::from_millis(333);

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
            || last.is_some_and(|v| v.elapsed() < PREVIEW_INTERVAL)
            || self.frame.lock().is_some()
        {
            return;
        }
        let preview = make_preview(frame);
        let mut slot = self.frame.lock();
        if self.enabled.load(Ordering::Relaxed) && slot.is_none() {
            *slot = Some(preview);
            *last = Some(Instant::now());
        }
    }
}
fn make_preview(frame: &VideoFrame) -> RecordingPreview {
    let scale = (640.0 / frame.width as f64)
        .min(360.0 / frame.height as f64)
        .min(1.0);
    let width = (frame.width as f64 * scale).round().max(1.) as u32;
    let height = (frame.height as f64 * scale).round().max(1.) as u32;
    let mut bgra = vec![0; width as usize * height as usize * 4];
    // Horizontal coordinates repeat for every row. Avoid two integer
    // divisions per output pixel on old CPUs.
    let columns: Vec<_> = (0..width)
        .map(|x| (x as u64 * frame.width as u64 / width as u64) as usize * 4)
        .collect();
    for y in 0..height {
        let source_row = (y as u64 * frame.height as u64 / height as u64) as usize * frame.stride;
        let output_row = y as usize * width as usize * 4;
        for (pixel, &column) in bgra[output_row..output_row + width as usize * 4]
            .chunks_exact_mut(4)
            .zip(&columns)
        {
            let src = source_row + column;
            pixel[..3].copy_from_slice(&frame.data[src..src + 3]);
            pixel[3] = 255;
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
        // A slow or hidden UI must not force repeated preview conversions.
        let mut another_tick = None;
        slot.publish(&frame, &mut another_tick);
        assert!(another_tick.is_none());
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

    #[test]
    fn preview_sampling_respects_padded_rows_and_source_coordinates() {
        let width = 1280usize;
        let height = 720usize;
        let stride = width * 4 + 16;
        let mut pixels = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                pixels[offset..offset + 4].copy_from_slice(&[x as u8, y as u8, (x + y) as u8, 0]);
            }
        }
        let frame = VideoFrame {
            width: width as u32,
            height: height as u32,
            stride,
            pts_us: 0,
            data: Arc::new(pixels),
        };
        let preview = make_preview(&frame);
        assert_eq!((preview.width, preview.height), (640, 360));
        for (x, y) in [(0usize, 0usize), (123, 234), (639, 359)] {
            let source = y * 2 * stride + x * 2 * 4;
            let dest = (y * 640 + x) * 4;
            assert_eq!(
                &preview.bgra[dest..dest + 3],
                &frame.data[source..source + 3]
            );
            assert_eq!(preview.bgra[dest + 3], 255);
        }
    }
}
