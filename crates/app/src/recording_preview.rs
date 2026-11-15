//! A bounded, read-only preview before recording starts. No audio or media file is created.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub(super) struct IdlePreview {
    target: CaptureTarget,
    cursor: bool,
    exclude_app: bool,
    stop: Arc<AtomicBool>,
    frames: crossbeam_channel::Receiver<Result<opencap_media::RecordingPreview, String>>,
}

impl IdlePreview {
    fn start(config: &RecordingConfig) -> Self {
        let target = config.target.clone();
        let cursor = config.cursor;
        let exclude_app = config.exclude_app;
        let mut capture_config = config.clone();
        capture_config.max_fps = 2;
        let (output, frames) = crossbeam_channel::bounded(1);
        let stale_frames = frames.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        std::thread::spawn(move || {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            let (video_tx, video_rx) = crossbeam_channel::bounded(2);
            let (error_tx, error_rx) = crossbeam_channel::bounded(2);
            let result = opencap_platform::start_capture(
                &capture_config,
                RecordingClock::new(),
                video_tx,
                video_rx.clone(),
                error_tx,
                Arc::new(AtomicU64::new(0)),
                Arc::new(CaptureMetrics::default()),
            );
            let mut capture = match result {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = output.try_send(Err(error.to_string()));
                    return;
                }
            };
            while !cancelled.load(Ordering::Acquire) {
                if let Ok(error) = error_rx.try_recv() {
                    let _ = stale_frames.try_recv();
                    let _ = output.try_send(Err(error));
                    break;
                }
                if capture.is_finished() {
                    let _ = stale_frames.try_recv();
                    let _ = output.try_send(Err("画面采集已停止，请重新选择目标或重试。".into()));
                    break;
                }
                if let Ok(frame) = video_rx.recv_timeout(Duration::from_millis(200))
                    && output.is_empty()
                {
                    let _ = output.try_send(Ok(opencap_media::make_preview(&frame)));
                }
            }
            let _ = capture.stop();
        });
        Self {
            target,
            cursor,
            exclude_app,
            stop,
            frames,
        }
    }
    fn matches(&self, config: &RecordingConfig) -> bool {
        self.target == config.target
            && self.cursor == config.cursor
            && self.exclude_app == config.exclude_app
    }
}
impl Drop for IdlePreview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Recorder {
    pub(super) fn poll_idle_preview(&mut self, cx: &mut Context<Self>) -> bool {
        #[cfg(feature = "ui-smoke")]
        if std::env::args().any(|arg| {
            arg == "--ui-design-check"
                || arg == "--ui-scroll-bench"
                || arg == "--ui-correction-bench"
                || arg == "--ui-chat-bench"
        }) {
            self.idle_preview.take();
            return false;
        }
        let should_show = self.session.is_none()
            && !self.active()
            && !self.config.audio_only
            && self.config.show_recording_preview
            && !self.settings
            && !self.transcription_tab
            && !self.choosing_target
            && self.selecting.is_none()
            && opencap_platform::native_window_visible(self.hwnd)
            && !matches!(self.config.target, CaptureTarget::Window(0));
        if !should_show {
            if self.idle_preview.take().is_some() {
                self.idle_preview_error = None;
                if self.session.is_none()
                    && let Some(image) = self.preview_image.take()
                {
                    cx.drop_image(image, None);
                }
                return true;
            }
            return false;
        }
        if self
            .idle_preview
            .as_ref()
            .is_none_or(|preview| !preview.matches(&self.config))
        {
            self.idle_preview.take();
            self.idle_preview_error = None;
            if let Some(image) = self.preview_image.take() {
                cx.drop_image(image, None);
            }
            self.idle_preview = Some(IdlePreview::start(&self.config));
            return true;
        }
        let Some(preview) = &self.idle_preview else {
            return false;
        };
        let Ok(result) = preview.frames.try_recv() else {
            return false;
        };
        match result {
            Ok(frame) => {
                self.idle_preview_error = None;
                if let Some(buffer) =
                    image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
                {
                    let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                    if let Some(old) = self.preview_image.replace(image) {
                        cx.drop_image(old, None);
                    }
                }
            }
            Err(error) => {
                self.idle_preview_error = Some(error);
                if let Some(image) = self.preview_image.take() {
                    cx.drop_image(image, None);
                }
            }
        }
        true
    }
}
