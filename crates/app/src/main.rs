#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(feature = "ui-smoke")]
mod smoke;
mod widgets;
use gpui::{prelude::*, *};
use opencap_core::*;
use opencap_media::{RecordingCommand, RecordingSession};
use opencap_platform::{DesktopControls, ShellEvent};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{collections::HashMap, path::PathBuf, rc::Rc, time::Duration};

const BG: u32 = 0x121316;
const SURFACE: u32 = 0x202226;
const MUTED: u32 = 0x92979e;
const RED: u32 = 0xee5d5d;

struct Recorder {
    config: RecordingConfig,
    targets: Vec<TargetInfo>,
    devices: Vec<AudioDevice>,
    session: Option<RecordingSession>,
    stats: RecordingStats,
    controls: Option<DesktopControls>,
    hwnd: isize,
    floating: Option<WindowHandle<Floating>>,
    floating_hwnd: isize,
    mode: usize,
    selecting: Option<opencap_platform::RegionSelection>,
    message: String,
    settings: bool,
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
fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .bg(rgb(if active { 0x3e292b } else { SURFACE }))
        .text_color(rgb(if active { 0xff9999 } else { 0xe6e7e9 }))
        .hover(|s| s.bg(rgb(0x34373d)))
        .child(label.into())
}
fn label(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
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
        if let Some(path) = smoke::output_dir() {
            config = RecordingConfig {
                output_dir: path.join("recording"),
                encoder: Some("libx264".into()),
                codec: Codec::H264,
                system_audio: false,
                microphone: false,
                exclude_app: false,
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
        let targets = opencap_platform::targets().unwrap_or_else(|e| {
            message = format!("无法枚举录制目标：{e}");
            Vec::new()
        });
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
            settings: false,
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
        }
    }
    fn active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.finished())
    }
    fn save(&mut self) {
        #[cfg(feature = "ui-smoke")]
        if smoke::output_dir().is_some() {
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
            match RecordingSession::start(recording) {
                Ok(session) => {
                    session.enable_preview(self.config.show_recording_preview);
                    self.stats = session.stats();
                    self.session = Some(session);
                }
                Err(e) => self.message = format!("无法开始录制：{e:#}"),
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
                }
            }
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
        let wants_monitor = self.config.monitor_levels && !self.active();
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
                    this.config.output_dir = path;
                    this.save();
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

impl Render for Recorder {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content_width = (f32::from(window.viewport_size().width) - 58.).max(1.);
        let active = self.active();
        let status = match self.stats.state {
            RecordingState::Idle => "准备就绪",
            RecordingState::Starting => "正在准备编码器…",
            RecordingState::Recording => "正在录制",
            RecordingState::Paused => "已暂停",
            RecordingState::Stopping => "正在保存…",
            RecordingState::Finished => "录制已保存",
            RecordingState::Failed => "录制未完成",
        };
        let target = if let CaptureTarget::Region { rect, .. } = self.config.target {
            format!(
                "区域 {} × {} · ({}, {})",
                rect.width, rect.height, rect.x, rect.y
            )
        } else {
            self.targets
                .iter()
                .find(|t| same_target(&t.target, &self.config.target))
                .map(|t| format!("{}  ·  {} × {}", t.name, t.width, t.height))
                .unwrap_or_else(|| "点击选择录制目标".into())
        };
        let mut root = div()
            .w_full()
            .min_h_full()
            .flex_shrink_0()
            .bg(rgb(BG))
            .text_color(rgb(0xeceef1))
            .font_family(UI_FONT)
            .p_6()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child("OpenCap"),
                            )
                            .child(label("录制工作台")),
                    )
                    .child(
                        button("settings", "设置", self.settings).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.settings = !this.settings;
                                this.close_targets();
                                this.audio_picker = None;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label(status))
                    .child(label("离线 · 无水印 · 不限时")),
            );
        if self.settings {
            root = root.child(self.settings_view(cx));
        } else {
            root = root
                .child(
                    div().flex().gap_2().children(
                        ["全屏", "窗口", "区域"]
                            .into_iter()
                            .enumerate()
                            .map(|(i, text)| {
                                button(("mode", i), text, self.mode == i).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.change_mode(i);
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(label("录制目标 · 点击切换"))
                        .child(
                            button("target", target, false)
                                .min_w(px(0.))
                                .overflow_hidden()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.choose_target();
                                    cx.notify();
                                })),
                        )
                        .when(self.choosing_target, |d| {
                            d.child(self.target_picker(content_width, cx))
                        })
                        .when(self.mode == 2, |d| {
                            d.child(
                                button(
                                    "region",
                                    if matches!(self.config.target, CaptureTarget::Region { .. }) {
                                        "调整区域 · 移动 / 缩放"
                                    } else {
                                        "框选录制区域"
                                    },
                                    false,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.pick_region();
                                        cx.notify();
                                    },
                                )),
                            )
                        }),
                )
                .when(!self.choosing_target, |d| {
                    d.child(
                        div()
                            .flex()
                            .gap_5()
                            .when(content_width < 780., |d| d.flex_col())
                            .child(self.recording_preview(cx).flex_1().min_w(px(0.)))
                            .child(
                                self.recording_options(cx)
                                    .when(content_width >= 780., |d| d.w(px(300.)).flex_shrink_0()),
                            ),
                    )
                });
        }
        let mut controls = div()
            .w_full()
            .flex_shrink_0()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(BG))
            .text_color(rgb(0xeceef1))
            .font_family(UI_FONT)
            .border_t_1()
            .border_color(rgb(0x303338))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .child(
                        div()
                            .text_2xl()
                            .font_family(MONO_FONT)
                            .child(time_text(self.stats.duration_seconds)),
                    )
                    .child(label(format!(
                        "{:.1} FPS     {:.0} kbps     {:.1} MB",
                        self.stats.fps,
                        self.stats.bitrate_kbps,
                        self.stats.bytes as f64 / 1_000_000.0
                    ))),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(
                        button(
                            "record",
                            if active {
                                "■  停止录制"
                            } else {
                                "●  开始录制"
                            },
                            true,
                        )
                        .bg(rgb(RED))
                        .text_color(rgb(0xffffff))
                        .py_3()
                        .flex_1()
                        .text_center()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.mode == 2
                                && !matches!(this.config.target, CaptureTarget::Region { .. })
                            {
                                this.pick_region();
                            } else {
                                this.toggle_record(cx);
                            }
                        })),
                    )
                    .when(active, |d| {
                        d.child(
                            button(
                                "pause",
                                if self.stats.state == RecordingState::Paused {
                                    "继续"
                                } else {
                                    "暂停"
                                },
                                false,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_pause();
                                cx.notify();
                            })),
                        )
                    }),
            )
            .child(label(format!(
                "{}  开始/停止    {}  暂停/继续",
                self.config.hotkey_record, self.config.hotkey_pause
            )));
        let message = self
            .stats
            .error
            .clone()
            .or_else(|| self.stats.warnings.last().cloned())
            .unwrap_or_else(|| self.message.clone());
        if !message.is_empty() {
            #[cfg(target_os = "macos")]
            if message.contains("权限") || message.contains("ScreenCaptureKit") {
                let microphone =
                    message.contains("麦克风") && !message.contains("ScreenCaptureKit");
                controls = controls.child(
                    div().flex().gap_2()
                        .child(button("privacy-settings", "打开权限设置", false).on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.open_url(if microphone {
                                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
                                } else {
                                    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
                                });
                            }),
                        ))
                        .child(button("refresh-permissions", "刷新录制目标", false).on_click(
                            cx.listener(|this, _, _, cx| { this.refresh_targets(); cx.notify(); }),
                        )),
                );
            }
            controls = controls.child(div().text_xs().text_color(rgb(0xffb5a9)).child(message));
        }
        if !self.recoverables.is_empty() {
            root = root.child(
                button(
                    "recover",
                    format!("恢复未完成录制（{}）", self.recoverables.len()),
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.recover();
                    cx.notify();
                })),
            );
        }
        controls = controls.child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .mt_auto()
                .gap_2()
                .child(
                    button("folder", "打开录制目录", false).on_click(cx.listener(
                        |this, _, _, _| {
                            let _ = std::fs::create_dir_all(&this.config.output_dir);
                            if let Err(e) = opencap_platform::open_path(&this.config.output_dir) {
                                this.message = e.to_string();
                            }
                        },
                    )),
                )
                .when(self.stats.state == RecordingState::Finished, |d| {
                    d.child(button("play-last", "播放录制", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(path) = &this.stats.output
                                && let Err(e) = opencap_platform::open_path(path)
                            {
                                this.message = e.to_string();
                                cx.notify();
                            }
                        },
                    )))
                })
                .child(
                    button("quit", "退出", false).on_click(cx.listener(|this, _, _, cx| {
                        this.quit_pending = true;
                        if let Some(s) = &this.session {
                            s.command(RecordingCommand::Stop);
                        }
                        if !this.active() {
                            cx.quit();
                        }
                    })),
                ),
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .child(div().flex_1().min_h(px(0.)).child(widgets::scrollable(
                "page-scroll",
                root,
                &self.page_scroll,
            )))
            .child(controls)
    }
}
impl Recorder {
    fn target_picker(&self, width: f32, cx: &Context<Self>) -> Div {
        let targets: Vec<_> = self
            .targets
            .iter()
            .filter(|t| {
                if self.mode == 1 {
                    matches!(t.target, CaptureTarget::Window(_))
                } else {
                    matches!(t.target, CaptureTarget::Monitor(_))
                }
            })
            .collect();
        let mut content = div().w_full().flex().flex_col().gap_3();
        let columns = widgets::target_columns(width - if self.targets_expanded { 0. } else { 10. });
        for (row, pair) in targets.chunks(columns).enumerate() {
            let mut line = div().flex().gap_3().w_full().flex_shrink_0();
            for (column, t) in pair.iter().enumerate() {
                let target = t.target.clone();
                let selected = same_target(&target, &self.config.target);
                let preview = if let CaptureTarget::Window(hwnd) = target {
                    self.previews.get(&hwnd).cloned()
                } else {
                    None
                };
                let caption = if preview.as_ref().is_some_and(|p| p.minimized()) {
                    "已最小化 · 可能显示最后画面"
                } else if matches!(target, CaptureTarget::Window(_)) && preview.is_none() {
                    "此窗口暂不提供预览"
                } else if matches!(target, CaptureTarget::Window(_)) {
                    if cfg!(target_os = "macos") {
                        "窗口快照 · 刷新窗口可更新"
                    } else {
                        "实时窗口预览"
                    }
                } else {
                    "整个显示器"
                };
                line = line.child(
                    div()
                        .id(("target-card", row * columns + column))
                        .flex_1()
                        .min_w(px(0.))
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if selected { RED } else { 0x34373d }))
                        .bg(rgb(SURFACE))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(0x2b2e33)))
                        .child(
                            div()
                                .h(px(120.))
                                .w_full()
                                .overflow_hidden()
                                .bg(rgb(0x090a0c))
                                .flex()
                                .items_center()
                                .justify_center()
                                .when_some(preview, |d, p| d.child(widgets::preview_view(p)))
                                .when(matches!(target, CaptureTarget::Monitor(_)), |d| {
                                    d.child(
                                        div()
                                            .text_2xl()
                                            .child(format!("{} × {}", t.width, t.height)),
                                    )
                                }),
                        )
                        .child(div().mt_2().text_sm().truncate().child(format!(
                            "{}{}",
                            if selected { "✓ " } else { "" },
                            t.name
                        )))
                        .child(label(caption).truncate())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.active() {
                                this.config.target = target.clone();
                                this.close_targets();
                                this.save();
                            }
                            cx.notify();
                        })),
                );
            }
            for _ in pair.len()..columns {
                line = line.child(div().flex_1());
            }
            content = content.child(line);
        }
        if targets.is_empty() {
            content = content.child(label("没有可录制窗口，请打开应用后刷新。"));
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label(format!(
                        "{} 个目标 · 每行 {} 个",
                        targets.len(),
                        columns
                    )))
                    .child(
                        button(
                            "expand-targets",
                            if self.targets_expanded {
                                "收起列表"
                            } else {
                                "展开全部"
                            },
                            self.targets_expanded,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.targets_expanded = !this.targets_expanded;
                            this.target_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        })),
                    )
                    .child(
                        button("refresh-targets", "刷新窗口", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.refresh_targets();
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(if self.targets_expanded {
                content
            } else {
                div()
                    .h(px(if self.mode == 1 { 345. } else { 195. }))
                    .flex_shrink_0()
                    .child(widgets::scrollable(
                        "target-list",
                        content,
                        &self.target_scroll,
                    ))
            })
    }
    fn recording_options(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(label("声音来源 · 录制中可随时切换"))
                    .when(!self.active(), |d| {
                        d.child(
                            button(
                                "monitor-levels-inline",
                                if self.config.monitor_levels {
                                    "电平检测：开"
                                } else {
                                    "电平检测：关"
                                },
                                self.config.monitor_levels,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.config.monitor_levels = !this.config.monitor_levels;
                                this.monitor = None;
                                this.monitor_peaks = (0.0, 0.0);
                                this.save();
                                cx.notify();
                            })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(self.audio_row(AudioKind::System, cx))
                    .child(self.audio_row(AudioKind::Microphone, cx)),
            )
            .when(!self.active() && !self.config.monitor_levels, |d| {
                d.child(label(
                    "打开“电平检测”可在未录制时确认这两路是否真的有声音。",
                ))
            })
            .child(label("画质与体积"))
            .child(
                div().flex().gap_2().children(
                    [
                        (Preset::Space, "省空间"),
                        (Preset::Balanced, "平衡"),
                        (Preset::Quality, "高质量"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (p, t))| {
                        button(("preset", i), t, self.config.preset == p).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if !this.active() {
                                    this.config.apply_preset(p);
                                    this.save();
                                }
                                cx.notify();
                            },
                        ))
                    }),
                ),
            )
            .child(label("省空间适合文档和会议；画面越动态，文件越大。"))
    }
    fn recording_preview(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label(if self.stats.state == RecordingState::Paused {
                        "录制预览 · 已暂停，保留最后画面"
                    } else if self.active() {
                        "录制预览 · 正在写入的画面"
                    } else if self.preview_image.is_some() {
                        "录制预览 · 最后一帧"
                    } else {
                        "录制预览"
                    }))
                    .child(
                        button(
                            "toggle-preview",
                            if self.config.show_recording_preview {
                                "收起预览"
                            } else {
                                "显示预览"
                            },
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.show_recording_preview =
                                !this.config.show_recording_preview;
                            if !this.config.show_recording_preview
                                && let Some(image) = this.preview_image.take()
                            {
                                cx.drop_image(image, None);
                            }
                            this.save();
                            cx.notify();
                        })),
                    ),
            )
            .when(self.config.show_recording_preview, |d| {
                d.child(
                    div()
                        .w_full()
                        .h(px(240.))
                        .bg(rgb(0x090a0c))
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when_some(self.preview_image.clone(), |d, image| {
                            d.child(img(image).size_full().object_fit(ObjectFit::Contain))
                        })
                        .when(self.preview_image.is_none(), |d| {
                            d.child(label(if self.active() {
                                "等待首帧…"
                            } else {
                                "开始录制后显示实际捕获画面"
                            }))
                        }),
                )
            })
    }
    fn audio_row(&self, kind: AudioKind, cx: &Context<Self>) -> Div {
        let enabled = if kind == AudioKind::System {
            self.config.system_audio
        } else {
            self.config.microphone
        };
        let name = if kind == AudioKind::System {
            "系统声音"
        } else {
            "麦克风"
        };
        let index: usize = if kind == AudioKind::System { 0 } else { 1 };
        let row = div()
            .flex()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(name)
                    .child(
                        div()
                            .id(("device", index))
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .cursor_pointer()
                            .truncate()
                            .child(format!("{}  ▾", self.device_name(kind)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.audio_picker == Some(kind) {
                                    this.audio_picker = None;
                                } else {
                                    match opencap_platform::audio_devices() {
                                        Ok(devices) => this.devices = devices,
                                        Err(e) => this.message = e.to_string(),
                                    }
                                    this.audio_picker = Some(kind);
                                    this.audio_scroll.set_offset(point(px(0.), px(0.)));
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                button(
                    ("audio", index),
                    if enabled { "已开启" } else { "已关闭" },
                    enabled,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_audio(kind);
                    cx.notify();
                })),
            );
        // While recording the session reports levels; otherwise the optional idle
        // monitor does, so the bar means the same thing in both states.
        let peak = if !enabled {
            0.
        } else if self.active() {
            if kind == AudioKind::System {
                self.stats.system_peak
            } else {
                self.stats.microphone_peak
            }
        } else if self.config.monitor_levels {
            if kind == AudioKind::System {
                self.monitor_peaks.0
            } else {
                self.monitor_peaks.1
            }
        } else {
            0.
        };
        let mut result = div().flex().flex_col().gap_1().child(row).child(
            div().h(px(3.)).w_full().bg(rgb(SURFACE)).child(
                div()
                    .h_full()
                    .w(relative(level_bar(peak)))
                    .bg(rgb(if peak > 0.95 { RED } else { 0x6cc5a2 })),
            ),
        );
        if self.audio_picker == Some(kind) {
            let selected = self.config.audio_routing().device(kind).clone();
            let entries = std::iter::once((None, "系统默认设备".to_string())).chain(
                self.devices
                    .iter()
                    .filter(|d| d.kind == kind)
                    .map(|d| (Some(d.id.clone()), d.name.clone())),
            );
            let list = div()
                .flex()
                .flex_col()
                .gap_1()
                .children(entries.enumerate().map(|(i, (id, name))| {
                    button(
                        ("audio-device", i),
                        format!("{}{}", if selected == id { "✓ " } else { "" }, name),
                        selected == id,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_device(kind, id.clone());
                        cx.notify();
                    }))
                }));
            result = result.child(div().h(px(150.)).child(widgets::scrollable(
                "audio-devices",
                list,
                &self.audio_scroll,
            )));
        }
        result
    }
    fn settings_view(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(label("录制界面"))
            .child(
                button(
                    "floating-controls",
                    if self.config.show_floating_controls {
                        "悬浮控制条：开启"
                    } else {
                        "悬浮控制条：关闭"
                    },
                    self.config.show_floating_controls,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.config.show_floating_controls = !this.config.show_floating_controls;
                    let visible = this.config.show_floating_controls
                        && matches!(
                            this.stats.state,
                            RecordingState::Recording | RecordingState::Paused
                        );
                    opencap_platform::show_controls_without_activation(this.floating_hwnd, visible);
                    this.save();
                    cx.notify();
                })),
            )
            .child(
                button(
                    "capture-exclusion",
                    if self.config.exclude_app {
                        "录屏中隐藏 OpenCap：开启"
                    } else {
                        "录屏中隐藏 OpenCap：关闭"
                    },
                    self.config.exclude_app,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let enabled = !this.config.exclude_app;
                    match opencap_platform::set_capture_exclusion(this.hwnd, enabled).and_then(
                        |_| opencap_platform::set_capture_exclusion(this.floating_hwnd, enabled),
                    ) {
                        Ok(()) => {
                            this.config.exclude_app = enabled;
                            this.save();
                        }
                        Err(e) => this.message = format!("无法更新窗口捕获设置：{e}"),
                    }
                    cx.notify();
                })),
            )
            .child(
                button(
                    "mic-denoise",
                    if self.config.mic_denoise {
                        "麦克风降噪：开启"
                    } else {
                        "麦克风降噪：关闭"
                    },
                    self.config.mic_denoise,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.config.mic_denoise = !this.config.mic_denoise;
                    if let Some(session) = &this.session {
                        session.command(RecordingCommand::SetMicDenoise(this.config.mic_denoise));
                    }
                    this.monitor = None;
                    this.save();
                    cx.notify();
                })),
            )
            .child(
                button(
                    "monitor-levels",
                    if self.config.monitor_levels {
                        "实时电平检测：开启"
                    } else {
                        "实时电平检测：关闭"
                    },
                    self.config.monitor_levels,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.config.monitor_levels = !this.config.monitor_levels;
                    this.monitor = None;
                    this.monitor_peaks = (0.0, 0.0);
                    this.save();
                    cx.notify();
                })),
            )
            .child(label(
                "降噪包含高通、噪声门和轻度压缩；实时电平检测只在未录制时打开所选设备，仅显示电平，不写入文件。",
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("录制范围边框"))
                    .child(
                        div().flex().gap_2().children(
                            [
                                (BorderMode::Always, "常亮"),
                                (BorderMode::OnStart, "开始显示 3 秒"),
                                (BorderMode::Never, "关闭"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (mode, text))| {
                                button(
                                    ("border-mode", i),
                                    text,
                                    self.config.effective_border_mode() == mode,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.config.show_capture_border = mode != BorderMode::Never;
                                        this.config.border_mode = mode;
                                        this.boundary_started = Some(std::time::Instant::now());
                                        this.indicator_failed = false;
                                        this.sync_indicator();
                                        this.save();
                                        cx.notify();
                                    },
                                ))
                            }),
                        ),
                    ),
            )
            .child(label(
                "红框表示录制，黄框表示暂停；目标窗口最小化后隐藏，边框不录入视频。",
            ))
            .child(label(
                "声音开关和设备可在录制中修改，画面规格在下次录制生效。",
            ))
            .child(label("编码格式"))
            .child(
                div().flex().gap_2().children(
                    [
                        (Codec::Auto, "自动"),
                        (Codec::H264, "H.264"),
                        (Codec::Hevc, "HEVC"),
                        (Codec::Av1, "AV1"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (codec, t))| {
                        button(("codec", i), t, self.config.codec == codec).on_click(cx.listener(
                            move |this, _, _, cx| {
                                if !this.active() {
                                    this.config.codec = codec;
                                    this.config.encoder = None;
                                    this.save();
                                }
                                cx.notify();
                            },
                        ))
                    }),
                ),
            )
            .child(label("H.264 兼容性最好；HEVC / AV1 需要播放器支持。"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "quality-level",
                            format!("质量值 {}", self.config.quality),
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.active() {
                                this.config.quality = match this.config.quality {
                                    18 => 22,
                                    22 => 26,
                                    26 => 30,
                                    30 => 34,
                                    _ => 18,
                                };
                                this.config.preset = Preset::Custom;
                                this.save();
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "bitrate-cap",
                            format!("码率上限 {} kbps", self.config.max_bitrate_kbps),
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.active() {
                                this.config.max_bitrate_kbps = match this.config.max_bitrate_kbps {
                                    800 => 1600,
                                    1600 => 4000,
                                    4000 => 10000,
                                    _ => 800,
                                };
                                this.config.preset = Preset::Custom;
                                this.save();
                            }
                            cx.notify();
                        })),
                    ),
            )
            .child(label("质量值越低越清晰；部分编码器不支持硬码率上限。"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("fps", format!("最大 {} FPS", self.config.max_fps), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                if !this.active() {
                                    this.config.max_fps = match this.config.max_fps {
                                        15 => 30,
                                        30 => 60,
                                        _ => 15,
                                    };
                                    this.config.preset = Preset::Custom;
                                    this.save();
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button(
                            "cursor",
                            if self.config.cursor {
                                "鼠标：显示"
                            } else {
                                "鼠标：隐藏"
                            },
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.active() {
                                this.config.cursor = !this.config.cursor;
                                this.save();
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "adaptive",
                            if self.config.adaptive {
                                "自适应帧率"
                            } else {
                                "固定帧率"
                            },
                            self.config.adaptive,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.active() {
                                this.config.adaptive = !this.config.adaptive;
                                this.config.preset = Preset::Custom;
                                this.save();
                            }
                            cx.notify();
                        })),
                    ),
            )
            .child(
                button("save-folder", "选择保存目录", false)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
            )
            .child(
                button(
                    "audio-quality",
                    format!(
                        "声音 {} kbps · {} 声道",
                        self.config.audio_bitrate_kbps, self.config.audio_channels
                    ),
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.active() {
                        let (b, c) = match this.config.audio_bitrate_kbps {
                            48 => (96, 2),
                            96 => (128, 2),
                            _ => (48, 1),
                        };
                        this.config.audio_bitrate_kbps = b;
                        this.config.audio_channels = c;
                        this.config.preset = Preset::Custom;
                        this.save();
                    }
                    cx.notify();
                })),
            )
            .child(
                button(
                    "separate-audio",
                    if self.config.separate_audio {
                        "同时保存独立音频 M4A：开启"
                    } else {
                        "同时保存独立音频 M4A：关闭"
                    },
                    self.config.separate_audio,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.active() {
                        this.config.separate_audio = !this.config.separate_audio;
                        this.save();
                        cx.notify();
                    }
                })),
            )
            .child(label(
                "独立音频与视频同名，停止后保存；开关在下次录制生效。",
            ))
            .child(label(self.config.output_dir.to_string_lossy().into_owned()))
            .child(
                button("hotkeys", "切换快捷键组合", false).on_click(cx.listener(
                    |this, _, _, cx| {
                        if !this.active() {
                            let alt = this.config.hotkey_record == "Ctrl+Alt+F9";
                            this.config.hotkey_record =
                                if alt { "Ctrl+Shift+F9" } else { "Ctrl+Alt+F9" }.into();
                            this.config.hotkey_pause = if alt {
                                "Ctrl+Shift+F10"
                            } else {
                                "Ctrl+Alt+F10"
                            }
                            .into();
                            this.controls = None;
                            match opencap_platform::desktop_controls(
                                &this.config.hotkey_record,
                                &this.config.hotkey_pause,
                            ) {
                                Ok(c) => this.controls = Some(c),
                                Err(e) => this.message = e.to_string(),
                            };
                            this.save();
                        }
                        cx.notify();
                    },
                )),
            )
            .child(label("OpenCap 0.1.0 · GPL-3.0-or-later"))
    }
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
            .text_color(rgb(0xffffff))
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
                button("float-stop", "停止", true).on_click(move |_, _, cx| {
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
    let application = Application::new();
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
        let bounds = Bounds::centered(None, size(px(960.), px(850.)), cx);
        let main = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("OpenCap".into()),
                        ..Default::default()
                    }),
                    is_resizable: true,
                    window_min_size: Some(size(px(500.), px(420.))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Recorder::new(window, cx)),
            )
            .expect("Create recorder window");
        let entity = main.update(cx, |_, _, cx| cx.entity()).unwrap();
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
                    cx.new(|_| Floating { recorder: weak })
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
        if let Some(path) = smoke::output_dir() {
            smoke::run(main, entity, path, cx);
        }
    });
}
