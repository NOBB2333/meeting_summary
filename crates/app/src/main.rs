#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
extern crate gpui_kit as gpui;
mod ai;
mod recording;
mod settings;
mod shell;
#[cfg(feature = "ui-smoke")]
mod smoke;
mod task;
mod transcript_view;
mod transcription;
mod ui;
mod widgets;
mod workspace;
use gpui::component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    input::InputState,
};
use gpui::{prelude::*, *};
use opencap_core::*;
use opencap_media::{RecordingCommand, RecordingSession};
use opencap_platform::{DesktopControls, ShellEvent};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{collections::HashMap, path::PathBuf, rc::Rc, time::Duration};

use ui::{ACCENT, BG, BORDER, MUTED, RED, SOFT, SURFACE, TEXT};

struct Recorder {
    config: RecordingConfig,
    targets: Vec<TargetInfo>,
    devices: Vec<AudioDevice>,
    session: Option<RecordingSession>,
    stats: RecordingStats,
    controls: Option<DesktopControls>,
    hwnd: isize,
    floating: Option<WindowHandle<gpui::base::Root>>,
    floating_hwnd: isize,
    mode: usize,
    selecting: Option<opencap_platform::RegionSelection>,
    message: String,
    message_expanded: bool,
    settings: bool,
    settings_page: settings::SettingsPage,
    choosing_target: bool,
    quit_pending: bool,
    recoverables: Vec<PathBuf>,
    recovering: Option<crossbeam_channel::Receiver<anyhow::Result<PathBuf>>>,
    previews: HashMap<isize, Rc<opencap_platform::WindowPreview>>,
    indicator: Option<opencap_platform::CaptureIndicator>,
    indicator_failed: bool,
    region_preview: bool,
    page_scroll: ScrollHandle,
    target_scroll: ScrollHandle,
    audio_picker: Option<AudioKind>,
    audio_scroll: ScrollHandle,
    targets_expanded: bool,
    boundary_started: Option<std::time::Instant>,
    preview_image: Option<std::sync::Arc<RenderImage>>,
    preview_pts: i64,
    monitor: Option<opencap_platform::AudioMonitor>,
    monitor_peaks: (f32, f32),
    transcription_tab: bool,
    asr: Option<opencap_asr::AsrSession>,
    asr_live: bool,
    transcript: opencap_asr::Transcript,
    transcript_output: Option<PathBuf>,
    transcription_status: String,
    transcription_progress: f64,
    transcription_clock: task::TaskClock,
    speaker_clock: task::TaskClock,
    ai_clock: task::TaskClock,
    speaker_threads: usize,
    transcript_view: Entity<transcript_view::TranscriptView>,
    transcript_dirty: bool,
    transcript_new_document: bool,
    task_display_second: u64,
    preparing_model: Option<crossbeam_channel::Receiver<transcription::PrepareEvent>>,
    pending_backfill: Option<PathBuf>,
    summary_task: Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::MinutesHistory>>>,
    summary_text: String,
    ai: ai::AiWorkspace,
    hotwords_input: Entity<InputState>,
    speaker_count: Entity<InputState>,
    speaker_threshold: Entity<InputState>,
    speaker_task: Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::Transcript>>>,
    speaker_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    meeting_page: workspace::MeetingPage,
    meetings: Vec<workspace::MeetingFile>,
    notes: Entity<gpui::component::input::TextareaState>,
    notes_status: String,
    notes_base: Option<PathBuf>,
    notes_saved: String,
    minutes_style: usize,
    recording_registered: bool,
}
#[cfg(target_os = "macos")]
const UI_FONT: &str = "PingFang SC";
#[cfg(not(target_os = "macos"))]
const UI_FONT: &str = "Microsoft YaHei UI";
#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const MONO_FONT: &str = "Consolas";
fn native_handle(window: &Window) -> isize {
    match HasWindowHandle::window_handle(window).map(|w| w.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        #[cfg(target_os = "macos")]
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as isize,
        _ => 0,
    }
}
fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, active: bool) -> Button {
    ui::button(id, label, active)
}
fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    ui::primary_button(id, label)
}
fn label(text: impl Into<SharedString>) -> Div {
    ui::label(text)
}
/// Map a linear peak onto a bar width with a -60 dBFS floor, so a quiet
/// noise floor reads as empty instead of a permanently half-full bar.
fn level_bar(peak: f32) -> f32 {
    let peak = peak.clamp(1e-6, 1.0);
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}
fn time_text(seconds: f64) -> String {
    let s = seconds as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
impl Recorder {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut message = String::new();
        let mut config = if config_path().exists() {
            RecordingConfig::load(&config_path()).unwrap_or_else(|e| {
                message = format!("设置读取失败，使用默认值：{e}");
                RecordingConfig::default()
            })
        } else {
            RecordingConfig::default()
        };
        // An HWND can be recycled by Windows between launches. Require a fresh window choice.
        if matches!(config.target, CaptureTarget::Window(_)) {
            config.target = CaptureTarget::Window(0);
        }
        #[cfg(feature = "ui-smoke")]
        if let Some(path) = smoke::isolated_dir() {
            config = RecordingConfig {
                output_dir: path.join("recording"),
                encoder: Some("libx264".into()),
                codec: Codec::H264,
                system_audio: false,
                microphone: false,
                exclude_app: false,
                realtime_transcription: false,
                ..Default::default()
            };
        }
        let hwnd = native_handle(window);
        opencap_platform::set_app_icon(hwnd);
        if !std::env::args().any(|a| a == "--preview") {
            let _ = opencap_platform::set_capture_exclusion(hwnd, config.exclude_app);
        }
        let controls =
            opencap_platform::desktop_controls(&config.hotkey_record, &config.hotkey_pause)
                .map_err(|e| message = format!("托盘/快捷键不可用：{e}"))
                .ok();
        let tray_available = controls.is_some();
        window.on_window_should_close(cx, move |_, cx| {
            if tray_available {
                opencap_platform::show_native_window(hwnd, false);
            } else {
                cx.quit();
            }
            false
        });
        cx.spawn(async move |entity, cx| {
            loop {
                futures_timer::Delay::new(Duration::from_millis(200)).await;
                if entity.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Reading/exporting saved meetings does not need screen capture access.
        // Enumerate targets when entering recording (or during the UI harness).
        let targets = if cfg!(feature = "ui-smoke") {
            opencap_platform::targets().unwrap_or_else(|e| {
                message = format!("无法枚举录制目标：{e}");
                Vec::new()
            })
        } else {
            Vec::new()
        };
        let devices = opencap_platform::audio_devices().unwrap_or_else(|e| {
            message = format!("无法枚举音频设备：{e}");
            Vec::new()
        });
        let recoverables = find_recoverables(&config.output_dir);
        let mode = match config.target {
            CaptureTarget::Monitor(_) => 0,
            CaptureTarget::Window(_) => 1,
            CaptureTarget::Region { .. } => 2,
        };
        let ai = ai::AiWorkspace::new(window, cx);
        let hotwords_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("中文术语，用逗号分隔")
                .default_value(config.transcription_hotwords.clone())
        });
        let speaker_count = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("人数：0 自动，或填写已知人数")
                .default_value("0")
        });
        let speaker_threshold = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("阈值")
                .default_value("0.9")
        });
        let meetings = workspace::discover(&config.output_dir);
        let notes = cx.new(|cx| {
            gpui::component::input::TextareaState::new(window, cx)
                .placeholder("记录自己的备注、补充信息或会后想法，内容自动保存到本机。")
                .auto_grow(10, 20)
        });
        Self {
            config,
            targets,
            devices,
            session: None,
            stats: RecordingStats::default(),
            controls,
            hwnd,
            floating: None,
            floating_hwnd: 0,
            mode,
            selecting: None,
            message,
            message_expanded: false,
            settings: false,
            settings_page: settings::SettingsPage::Ai,
            choosing_target: false,
            quit_pending: false,
            recoverables,
            recovering: None,
            previews: HashMap::new(),
            indicator: None,
            indicator_failed: false,
            region_preview: false,
            page_scroll: ScrollHandle::new(),
            target_scroll: ScrollHandle::new(),
            audio_picker: None,
            audio_scroll: ScrollHandle::new(),
            targets_expanded: true,
            boundary_started: None,
            preview_image: None,
            preview_pts: 0,
            monitor: None,
            monitor_peaks: (0.0, 0.0),
            transcription_tab: !cfg!(feature = "ui-smoke"),
            asr: None,
            asr_live: false,
            transcript: opencap_asr::Transcript::default(),
            transcript_output: None,
            transcription_status: "等待录制或导入文件".into(),
            transcription_progress: 0.0,
            transcription_clock: task::TaskClock::default(),
            speaker_clock: task::TaskClock::default(),
            ai_clock: task::TaskClock::default(),
            speaker_threads: 2,
            transcript_view: cx.new(|_| transcript_view::TranscriptView::new()),
            transcript_dirty: true,
            transcript_new_document: true,
            task_display_second: 0,
            preparing_model: None,
            pending_backfill: None,
            summary_task: None,
            summary_text: String::new(),
            ai,
            hotwords_input,
            speaker_count,
            speaker_threshold,
            speaker_task: None,
            speaker_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            meeting_page: workspace::MeetingPage::Library,
            meetings,
            notes,
            notes_status: String::new(),
            notes_base: None,
            notes_saved: String::new(),
            minutes_style: 0,
            recording_registered: false,
        }
    }
    fn active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.finished())
    }
    fn save(&mut self) {
        #[cfg(feature = "ui-smoke")]
        if smoke::isolated_dir().is_some() {
            return;
        }
        if let Err(e) = self.config.save(&config_path()) {
            self.message = format!("无法保存设置：{e}");
        }
    }
    fn toggle_record(&mut self, cx: &mut Context<Self>) {
        if self.recovering.is_some() {
            return;
        }
        if self.active() {
            if let Some(s) = &self.session {
                s.command(RecordingCommand::Stop);
            }
            self.stats.state = RecordingState::Stopping;
        } else {
            if self.targets.is_empty() {
                self.refresh_targets();
                if self.targets.is_empty() {
                    // Do not clear the last successful transcript or launch ASR
                    // when recording is known to be unavailable (e.g. TCC).
                    cx.notify();
                    return;
                }
            }
            if self.mode == 2 && !matches!(self.config.target, CaptureTarget::Region { .. }) {
                self.pick_region();
                return;
            }
            if self.mode == 1
                && !matches!(self.config.target, CaptureTarget::Window(hwnd) if hwnd != 0)
            {
                self.message = "请先选择一个窗口".into();
                self.refresh_targets();
                cx.notify();
                return;
            }
            self.selecting = None;
            self.close_targets();
            self.audio_picker = None;
            self.save();
            self.message.clear();
            self.indicator_failed = false;
            self.region_preview = false;
            self.boundary_started = None;
            if let Some(image) = self.preview_image.take() {
                cx.drop_image(image, None);
            }
            let mut recording = self.config.clone();
            recording.reserve_audio_track = true;
            if self.transcription_busy() {
                self.message = "请先完成或取消当前转录/纪要任务，再开始录制".into();
                cx.notify();
                return;
            }
            let observer = self.start_live_asr();
            match RecordingSession::start_observed(recording, observer) {
                Ok(session) => {
                    session.enable_preview(self.config.show_recording_preview);
                    self.stats = session.stats();
                    self.session = Some(session);
                    self.recording_registered = false;
                }
                Err(e) => {
                    self.message = format!("无法开始录制：{e:#}");
                    if let Some(asr) = &mut self.asr {
                        asr.cancel();
                    }
                    self.asr_live = false;
                }
            }
        }
        cx.notify();
    }
    fn toggle_pause(&mut self) {
        if let Some(s) = &self.session {
            match s.stats().state {
                RecordingState::Recording => s.command(RecordingCommand::Pause),
                RecordingState::Paused => s.command(RecordingCommand::Resume),
                _ => (),
            }
        }
    }
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut needs_render = false;
        let task_second = if self.summary_task.is_some() {
            self.ai_clock.elapsed().as_secs()
        } else if self.speaker_task.is_some() {
            self.speaker_clock.elapsed().as_secs()
        } else {
            self.transcription_clock.elapsed().as_secs()
        };
        if self.transcription_busy() && task_second != self.task_display_second {
            self.task_display_second = task_second;
            needs_render = true;
        }
        self.persist_notes(cx);
        needs_render |= self.poll_transcription();
        let events: Vec<_> = self
            .controls
            .as_ref()
            .map(|c| c.events.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            needs_render = true;
            match event {
                ShellEvent::Show => opencap_platform::show_native_window(self.hwnd, true),
                ShellEvent::Record => self.toggle_record(cx),
                ShellEvent::Pause => self.toggle_pause(),
                ShellEvent::OpenFolder => {
                    let _ = opencap_platform::open_path(&self.config.output_dir);
                }
                ShellEvent::Quit => {
                    self.quit_pending = true;
                    if let Some(s) = &self.session {
                        s.command(RecordingCommand::Stop);
                    }
                }
                ShellEvent::Locked => {
                    if let Some(s) = &self.session {
                        s.command(RecordingCommand::Stop);
                        self.message = "桌面已锁定，正在结束录制".into();
                    }
                }
                ShellEvent::Warning(w) => self.message = w,
            }
        }
        if let Some(session) = &self.session {
            session.enable_preview(
                self.config.show_recording_preview
                    && !self.settings
                    && !self.transcription_tab
                    && opencap_platform::native_window_visible(self.hwnd),
            );
            if let Some(preview) = session.take_preview() {
                self.preview_pts = preview.pts_us;
                if let Some(buffer) =
                    image::RgbaImage::from_raw(preview.width, preview.height, preview.bgra)
                {
                    let next =
                        std::sync::Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                    if let Some(old) = self.preview_image.replace(next) {
                        cx.drop_image(old, None);
                    }
                    needs_render = true;
                }
            }
            let previous = self.stats.state;
            let next_stats = session.stats();
            let stats_changed = next_stats.state != self.stats.state
                || (next_stats.duration_seconds - self.stats.duration_seconds).abs() >= 0.2
                || next_stats.bytes != self.stats.bytes
                || next_stats.error != self.stats.error
                || next_stats.audio_routing != self.stats.audio_routing
                || next_stats.system_peak != self.stats.system_peak
                || next_stats.microphone_peak != self.stats.microphone_peak
                || next_stats.warnings.len() != self.stats.warnings.len();
            self.stats = next_stats;
            if self.asr_live
                && let Some(asr) = &mut self.asr
            {
                if let Some(path) = &self.stats.output {
                    let base = transcription::final_recording_path(path);
                    asr.set_output(base.clone());
                    self.transcript_output = Some(base);
                }
                if session.finished() {
                    asr.finish_at((self.stats.duration_seconds * 1000.0) as u64);
                    self.asr_live = false;
                }
            }
            needs_render |= stats_changed;
            if previous != self.stats.state {
                if self.stats.state == RecordingState::Recording
                    && previous == RecordingState::Starting
                {
                    self.boundary_started = Some(std::time::Instant::now());
                }
                let visible = self.config.show_floating_controls
                    && matches!(
                        self.stats.state,
                        RecordingState::Recording
                            | RecordingState::Paused
                            | RecordingState::Stopping
                    );
                opencap_platform::show_controls_without_activation(self.floating_hwnd, visible);
                if matches!(
                    self.stats.state,
                    RecordingState::Finished | RecordingState::Failed
                ) {
                    opencap_platform::show_native_window(self.hwnd, true);
                    self.recoverables = find_recoverables(&self.config.output_dir);
                    self.refresh_meetings();
                    if self.stats.state == RecordingState::Finished {
                        self.transcription_tab = true;
                        self.meeting_page = workspace::MeetingPage::Library;
                        self.page_scroll.set_offset(point(px(0.), px(0.)));
                    }
                }
            }
        }
        if !self.recording_registered
            && self.stats.state == RecordingState::Finished
            && self.session.as_ref().is_some_and(|s| s.finished())
        {
            self.recording_registered = true;
            self.refresh_meetings();
            needs_render = true;
        }
        let region_events: Vec<_> = self
            .selecting
            .as_ref()
            .map(|s| s.events.try_iter().collect())
            .unwrap_or_default();
        for event in region_events {
            needs_render = true;
            match event {
                opencap_platform::RegionEvent::Changed(rect) => {
                    self.config.target = CaptureTarget::Region {
                        monitor: self.monitor_index(),
                        rect,
                    };
                    self.region_preview = true;
                    self.save();
                }
                opencap_platform::RegionEvent::Start(rect) => {
                    self.config.target = CaptureTarget::Region {
                        monitor: self.monitor_index(),
                        rect,
                    };
                    self.region_preview = true;
                    self.save();
                    // Keep the editor above the desktop until its explicit start action.
                    // Restoring the GPUI window here would cover the handles and toolbar.
                    self.selecting = None;
                    self.toggle_record(cx);
                }
                opencap_platform::RegionEvent::Closed => {
                    self.selecting = None;
                    self.region_preview = false;
                    opencap_platform::show_native_window(self.hwnd, true);
                }
            }
        }
        if let Some(controls) = &self.controls {
            controls.set_recording_state(matches!(
                self.stats.state,
                RecordingState::Recording | RecordingState::Paused | RecordingState::Stopping
            ));
        }
        let wants_monitor = self.config.monitor_levels
            && !self.settings
            && !self.transcription_tab
            && !self.active()
            && !self.transcription_busy();
        if !wants_monitor {
            if self.monitor.take().is_some() {
                self.monitor_peaks = (0.0, 0.0);
                needs_render = true;
            }
        } else {
            if self.monitor.is_none() {
                match opencap_platform::start_audio_monitor(
                    &self.config.audio_routing(),
                    self.config.mic_denoise,
                ) {
                    Ok(monitor) => self.monitor = Some(monitor),
                    Err(e) => {
                        self.config.monitor_levels = false;
                        self.message = format!("实时电平检测不可用：{e}");
                        self.save();
                    }
                }
            }
            if let Some(monitor) = &self.monitor {
                if let Some(warning) = monitor.take_warning() {
                    self.message = warning;
                    needs_render = true;
                }
                let peaks = monitor.peaks();
                if (peaks.0 - self.monitor_peaks.0).abs() > 0.004
                    || (peaks.1 - self.monitor_peaks.1).abs() > 0.004
                {
                    self.monitor_peaks = peaks;
                    needs_render = true;
                }
            }
        }
        if let Some(result) = self.recovering.as_ref().and_then(|r| r.try_recv().ok()) {
            needs_render = true;
            self.recovering = None;
            self.message = match result {
                Ok(p) => format!("已恢复到 {}；原临时文件已保留", p.display()),
                Err(e) => format!("恢复失败，原文件已保留：{e:#}"),
            };
        }
        if self.quit_pending && !self.active() {
            cx.quit();
        }
        self.sync_indicator();
        if needs_render
            && self.config.show_floating_controls
            && matches!(
                self.stats.state,
                RecordingState::Recording | RecordingState::Paused | RecordingState::Stopping
            )
            && let Some(window) = self.floating
        {
            let _ = window.update(cx, |_, _, cx| cx.notify());
        }
        if needs_render {
            cx.notify();
        }
    }
    fn monitor_index(&self) -> usize {
        match self.config.target {
            CaptureTarget::Monitor(i) | CaptureTarget::Region { monitor: i, .. } => i,
            _ => 0,
        }
    }
    fn change_mode(&mut self, mode: usize) {
        if self.active() {
            return;
        }
        self.selecting = None;
        self.close_targets();
        self.mode = mode;
        self.region_preview = false;
        let target = self.targets.iter().find(|t| {
            if mode == 1 {
                matches!(t.target, CaptureTarget::Window(_))
            } else {
                matches!(t.target, CaptureTarget::Monitor(_))
            }
        });
        if let Some(t) = target {
            self.config.target = t.target.clone();
        }
        self.save();
        if mode == 1 {
            self.choose_target();
        }
    }
    fn close_targets(&mut self) {
        for preview in self.previews.values() {
            preview.hide();
        }
        self.previews.clear();
        self.choosing_target = false;
    }
    fn choose_target(&mut self) {
        if self.active() {
            return;
        }
        if self.choosing_target {
            self.close_targets();
            return;
        }
        self.refresh_targets();
        self.choosing_target = true;
    }
    fn refresh_targets(&mut self) {
        self.close_targets();
        match opencap_platform::targets() {
            Ok(targets) => {
                self.targets = targets;
                self.message.clear();
            }
            Err(e) => self.message = format!("无法刷新窗口：{e}"),
        }
        if self.mode == 1 {
            for target in &self.targets {
                if let CaptureTarget::Window(hwnd) = target.target
                    && let Ok(preview) = opencap_platform::WindowPreview::new(self.hwnd, hwnd)
                {
                    self.previews.insert(hwnd, Rc::new(preview));
                }
            }
        }
        self.target_scroll.set_offset(point(px(0.), px(0.)));
        self.choosing_target = true;
    }
    fn pick_region(&mut self) {
        if self.active() || self.selecting.is_some() {
            return;
        }
        let monitor = self.monitor_index();
        let initial = if let CaptureTarget::Region { rect, .. } = self.config.target {
            Some(rect)
        } else {
            None
        };
        self.indicator = None;
        self.close_targets();
        if initial.is_none() {
            opencap_platform::show_native_window(self.hwnd, false);
        }
        match opencap_platform::edit_region(monitor, initial) {
            Ok(editor) => self.selecting = Some(editor),
            Err(error) => {
                self.message = format!("无法选择区域：{error:#}");
                opencap_platform::show_native_window(self.hwnd, true);
            }
        }
    }
    fn choose_device(&mut self, kind: AudioKind, id: Option<String>) {
        let previous = self.config.clone();
        let slot = if kind == AudioKind::System {
            &mut self.config.system_device
        } else {
            &mut self.config.microphone_device
        };
        *slot = id;
        self.apply_audio(previous);
        self.audio_picker = None;
    }
    fn toggle_audio(&mut self, kind: AudioKind) {
        let previous = self.config.clone();
        if kind == AudioKind::System {
            self.config.system_audio = !self.config.system_audio;
        } else {
            self.config.microphone = !self.config.microphone;
        }
        self.apply_audio(previous);
    }
    fn apply_audio(&mut self, previous: RecordingConfig) {
        if self.active()
            && let Some(session) = &self.session
            && let Err(e) = session.set_audio(self.config.audio_routing())
        {
            self.config = previous;
            self.message = e.to_string();
            return;
        }
        // The idle meter watches specific devices, so reopen them on any routing change.
        self.monitor = None;
        self.save();
    }
    fn sync_indicator(&mut self) {
        let recording = matches!(
            self.stats.state,
            RecordingState::Recording | RecordingState::Paused
        );
        let selected_region = self.region_preview
            && !self.active()
            && matches!(self.config.target, CaptureTarget::Region { .. });
        if self.selecting.is_some()
            || !self.config.effective_border_mode().visible(
                self.boundary_started
                    .map(|v| v.elapsed())
                    .unwrap_or(Duration::MAX),
            )
            || (!recording && !selected_region)
        {
            self.indicator = None;
            return;
        }
        if self.indicator_failed {
            return;
        }
        if self.indicator.is_none() {
            match opencap_platform::CaptureIndicator::new() {
                Ok(indicator) => self.indicator = Some(indicator),
                Err(e) => {
                    self.message = format!("无法显示范围提示：{e}");
                    self.indicator_failed = true;
                    return;
                }
            }
        }
        if let Some(indicator) = &mut self.indicator
            && let Err(e) = indicator.update(
                &self.config.target,
                self.stats.state == RecordingState::Paused || !recording,
            )
        {
            indicator.hide();
            self.message = format!("范围提示不可用：{e}");
            self.indicator_failed = true;
        }
    }
    fn device_name(&self, kind: AudioKind) -> String {
        let id = if kind == AudioKind::System {
            &self.config.system_device
        } else {
            &self.config.microphone_device
        };
        id.as_ref()
            .and_then(|id| self.devices.iter().find(|d| &d.id == id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "系统默认设备".into())
    }
    fn recover(&mut self) {
        if self.active() || self.recovering.is_some() {
            return;
        }
        let Some(input) = self.recoverables.first().cloned() else {
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.recovering = Some(rx);
        self.message = "正在恢复录制…".into();
        std::thread::spawn(move || {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let output = input.with_file_name(format!("Recovered-{stamp}.mp4"));
            let result = opencap_media::recover_file(&input, &output).map(|_| output);
            let _ = tx.send(result);
        });
    }
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        if self.active() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择录制保存目录".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(path) = paths.first()
            {
                let path = path.clone();
                let _ = entity.update(cx, |this, cx| {
                    this.refresh_meetings();
                    this.config.output_dir = path;
                    this.save();
                    this.refresh_meetings();
                    cx.notify();
                });
            }
        })
        .detach();
    }
}
fn same_target(a: &CaptureTarget, b: &CaptureTarget) -> bool {
    match (a, b) {
        (CaptureTarget::Window(a), CaptureTarget::Window(b)) => a == b,
        (CaptureTarget::Monitor(a), CaptureTarget::Monitor(b)) => a == b,
        (CaptureTarget::Monitor(a), CaptureTarget::Region { monitor: b, .. }) => a == b,
        _ => false,
    }
}
fn find_recoverables(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".partial.mp4"))
        })
        .collect()
}

struct Floating {
    recorder: WeakEntity<Recorder>,
}
impl Render for Floating {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stats = self
            .recorder
            .upgrade()
            .map(|r| r.read(cx).stats.clone())
            .unwrap_or_default();
        let pause = self.recorder.clone();
        let stop = self.recorder.clone();
        let microphone = self.recorder.clone();
        let system = self.recorder.clone();
        let main = self.recorder.clone();
        let routing = self
            .recorder
            .upgrade()
            .map(|r| r.read(cx).config.audio_routing())
            .unwrap_or_default();
        div()
            .w(px(660.))
            .h(px(66.))
            .p_3()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family(UI_FONT)
            .flex()
            .gap_2()
            .items_center()
            .child(div().text_color(rgb(RED)).child("●"))
            .child(
                div()
                    .id("move-controls")
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .font_family(MONO_FONT)
                    .child(time_text(stats.duration_seconds)),
            )
            .child(
                button(
                    "float-pause",
                    if stats.state == RecordingState::Paused {
                        "继续"
                    } else {
                        "暂停"
                    },
                    false,
                )
                .on_click(move |_, _, cx| {
                    let _ = pause.update(cx, |r, _| r.toggle_pause());
                }),
            )
            .child(
                primary_button("float-stop", "停止")
                    .danger()
                    .on_click(move |_, _, cx| {
                        let _ = stop.update(cx, |r, _| {
                            if let Some(s) = &r.session {
                                s.command(RecordingCommand::Stop);
                            }
                        });
                    }),
            )
            .child(
                button(
                    "float-mic",
                    if routing.microphone {
                        "麦克风 开"
                    } else {
                        "麦克风 关"
                    },
                    routing.microphone,
                )
                .on_click(move |_, _, cx| {
                    let _ = microphone.update(cx, |r, cx| {
                        r.toggle_audio(AudioKind::Microphone);
                        cx.notify();
                    });
                }),
            )
            .child(
                button(
                    "float-system",
                    if routing.system_audio {
                        "系统声 开"
                    } else {
                        "系统声 关"
                    },
                    routing.system_audio,
                )
                .on_click(move |_, _, cx| {
                    let _ = system.update(cx, |r, cx| {
                        r.toggle_audio(AudioKind::System);
                        cx.notify();
                    });
                }),
            )
            .child(
                button("float-main", "主窗口", false).on_click(move |_, _, cx| {
                    let _ = main.update(cx, |r, _| {
                        opencap_platform::show_native_window(r.hwnd, true)
                    });
                }),
            )
    }
}
fn main() {
    struct StderrLogger;
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{}: {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: StderrLogger = StderrLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    let application = gpui::application().with_assets(gpui::assets::Assets);
    #[cfg(target_os = "macos")]
    let reopen_view = Rc::new(std::cell::Cell::new(0isize));
    #[cfg(target_os = "macos")]
    {
        let view = reopen_view.clone();
        application.on_reopen(move |_| {
            opencap_platform::show_native_window(view.get(), true);
        });
    }
    application.run(move |cx: &mut App| {
        gpui::init(cx);
        ui::init_theme(cx);
        let initial_size = size(px(1200.), px(820.));
        #[cfg(feature = "ui-smoke")]
        let initial_size = smoke::preview_size().unwrap_or(initial_size);
        let bounds = Bounds::centered(None, initial_size, cx);
        let mut main_entity = None;
        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("OpenCap".into()),
                        ..Default::default()
                    }),
                    is_resizable: true,
                    window_min_size: Some(size(px(620.), px(480.))),
                    ..Default::default()
                },
                |window, cx| {
                    let entity = cx.new(|cx| Recorder::new(window, cx));
                    main_entity = Some(entity.clone());
                    cx.new(|cx| gpui::base::Root::new(entity, window, cx))
                },
            )
            .expect("Create recorder window");
        let entity = main_entity.expect("recorder entity");
        #[cfg(target_os = "macos")]
        reopen_view.set(entity.read(cx).hwnd);
        let weak = entity.downgrade();
        let floating = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(660.), px(66.)),
                        cx,
                    ))),
                    titlebar: None,
                    kind: WindowKind::Floating,
                    show: false,
                    focus: false,
                    is_resizable: false,
                    ..Default::default()
                },
                move |window, cx| {
                    let hwnd = native_handle(window);
                    let exclude = weak.upgrade().is_none_or(|r| r.read(cx).config.exclude_app);
                    let _ = opencap_platform::set_capture_exclusion(hwnd, exclude);
                    window.on_window_should_close(cx, move |_, _| {
                        opencap_platform::show_native_window(hwnd, false);
                        false
                    });
                    let _ = weak.update(cx, |r, _| r.floating_hwnd = hwnd);
                    let floating = cx.new(|_| Floating { recorder: weak });
                    cx.new(|cx| gpui::base::Root::new(floating, window, cx))
                },
            )
            .expect("Create recording controls");
        entity.update(cx, |r, _| {
            r.floating = Some(floating);
            // Dispatch outside the App borrow: SetWindowPos synchronously invokes
            // GPUI's resize callback, which must be able to update its window bounds.
            let (toolbar, owner) = (r.floating_hwnd, r.hwnd);
            #[cfg(windows)]
            std::thread::spawn(move || {
                let _ = opencap_platform::configure_floating_controls(toolbar, owner);
            });
            #[cfg(target_os = "macos")]
            let _ = opencap_platform::configure_floating_controls(toolbar, owner);
            opencap_platform::show_native_window(r.hwnd, true);
        });
        cx.activate(true);
        #[cfg(feature = "ui-smoke")]
        if let Some(source) = smoke::preview_source() {
            entity.update(cx, |r, cx| {
                r.start_file_transcription(source, false, false);
                cx.notify();
            });
        }
        #[cfg(feature = "ui-smoke")]
        smoke::scroll_benchmark(main_window, entity.clone(), cx);
        #[cfg(feature = "ui-smoke")]
        if let Some(path) = smoke::output_dir() {
            smoke::run(main_window, entity, path, cx);
        }
        #[cfg(not(feature = "ui-smoke"))]
        let _ = main_window;
    });
}
