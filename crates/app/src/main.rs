#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
extern crate gpui_kit as gpui;
mod ai;
mod ai_chat;
mod live_notes;
#[cfg(feature = "online-speaker")]
mod online_speakers;
#[cfg(feature = "internal-player")]
mod player;
mod recording;
mod search;
mod semantic;
mod settings;
mod shell;
#[cfg(feature = "ui-smoke")]
mod smoke;
mod speakers;
mod task;
mod text_document;
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
use opencap_media::{AudioSubscription, RecordingCommand, RecordingSession};
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
    window_icon_recording: bool,
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
    #[allow(clippy::type_complexity)]
    audio_export_job: Option<(
        PathBuf,
        crossbeam_channel::Receiver<anyhow::Result<(PathBuf, bool)>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicU8>,
    )>,
    audio_export_stage_seen: u8,
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
    monitor_maintenance_at: std::time::Instant,
    monitor_levels: (AudioRouteLevels, AudioRouteLevels),
    transcription_tab: bool,
    asr: Option<opencap_asr::AsrSession>,
    asr_live: bool,
    revision_runtime: Option<opencap_asr::RevisionRuntimeState>,
    revision_failure: Option<String>,
    recording_without_live_due_to_task: bool,
    asr_processed_ms: Option<u64>,
    asr_models: opencap_asr::ModelAvailability,
    transcript: opencap_asr::Transcript,
    transcript_output: Option<PathBuf>,
    transcript_disk_baseline: Option<(PathBuf, String)>,
    transcript_load_generation: u64,
    transcript_load_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    transcript_load_workers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(feature = "online-speaker")]
    word_timing_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(feature = "online-speaker")]
    word_timing_status: String,
    transcription_status: String,
    transcription_progress: f64,
    transcription_clock: task::TaskClock,
    speaker_clock: task::TaskClock,
    ai_clock: task::TaskClock,
    speaker_threads: usize,
    speaker_compare_version: Option<String>,
    speaker_compare_page: usize,
    transcript_view: Entity<transcript_view::TranscriptView>,
    speakers_view: Entity<speakers::SpeakersView>,
    voice_busy: bool,
    transcript_dirty: bool,
    transcript_new_document: bool,
    task_display_second: u64,
    preparing_model: Option<crossbeam_channel::Receiver<transcription::PrepareEvent>>,
    prepare_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Source, original meeting, run ID and exact visible transcript at scheduling.
    pending_backfill: Option<(PathBuf, PathBuf, String, Vec<u8>)>,
    backfill_checking: bool,
    summary_task: Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::MinutesHistory>>>,
    summary_task_base: Option<PathBuf>,
    summary_text: String,
    live_notes: Option<live_notes::Controller>,
    live_notes_status: String,
    live_archive_preview: Option<(PathBuf, String)>,
    live_archive_loading: bool,
    live_notes_credentials_busy: bool,
    live_notes_preparing: bool,
    live_notes_prepare_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "online-speaker")]
    online_speakers: Option<online_speakers::Controller>,
    #[cfg(feature = "online-speaker")]
    online_speaker_status: String,
    #[cfg(feature = "online-speaker")]
    backfill_speaker_history: Option<Vec<opencap_asr::SpeakerAnalysisVersion>>,
    #[cfg(feature = "online-speaker")]
    pending_speaker_sync: bool,
    live_note_edit_input: Entity<InputState>,
    #[cfg(feature = "internal-player")]
    player: Option<opencap_media::PlaybackSession>,
    #[cfg(feature = "internal-player")]
    player_base: Option<PathBuf>,
    #[cfg(feature = "internal-player")]
    player_media: Option<PathBuf>,
    #[cfg(feature = "internal-player")]
    player_snapshot: Option<opencap_media::PlaybackSnapshot>,
    #[cfg(feature = "internal-player")]
    player_image: Option<std::sync::Arc<RenderImage>>,
    #[cfg(feature = "internal-player")]
    player_clear_image: bool,
    #[cfg(feature = "internal-player")]
    pending_player: Option<(PathBuf, PathBuf, player::PlayerRequest)>,
    #[cfg(feature = "internal-player")]
    player_start_pending: bool,
    #[cfg(feature = "internal-player")]
    player_selection_anchor: Option<u64>,
    #[cfg(feature = "internal-player")]
    player_selection: Option<(u64, u64)>,
    #[cfg(feature = "internal-player")]
    player_selection_snapshot: Vec<(u64, u64, u64, u64, String)>,
    #[cfg(feature = "internal-player")]
    player_selection_run: Option<Option<String>>,
    #[cfg(feature = "internal-player")]
    player_slider: Entity<gpui::component::slider::SliderState>,
    #[cfg(feature = "internal-player")]
    player_drag_preview: Option<u64>,
    #[cfg(feature = "internal-player")]
    player_keyboard_focus: FocusHandle,
    #[cfg(feature = "internal-player")]
    player_waveform: Option<opencap_asr::Waveform>,
    #[cfg(feature = "internal-player")]
    player_waveform_jobs: Vec<player::WaveformJob>,
    #[cfg(feature = "internal-player")]
    player_waveform_status: String,
    ai: ai::AiWorkspace,
    hotwords_input: Entity<InputState>,
    speaker_count: Entity<InputState>,
    speaker_threshold: Entity<InputState>,
    speaker_task: Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::Transcript>>>,
    transcript_sync: Option<crossbeam_channel::Receiver<opencap_asr::Transcript>>,
    historical_checking: bool,
    historical_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    text_document: Option<Entity<text_document::TextDocument>>,
    speaker_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    meeting_page: workspace::MeetingPage,
    meetings: Vec<workspace::MeetingFile>,
    meeting_list_page: usize,
    meeting_selected_base: Option<PathBuf>,
    meeting_rows_scroll: ScrollHandle,
    meeting_list_focus: FocusHandle,
    meeting_detail_scroll: ScrollHandle,
    meeting_scan: Option<crossbeam_channel::Receiver<workspace::MeetingScan>>,
    meeting_scan_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    meeting_scan_again: bool,
    meeting_scan_extra: std::collections::BTreeSet<PathBuf>,
    pending_legacy_imports: Vec<workspace::PendingImport>,
    pending_legacy_import_page: usize,
    meeting_search_input: Entity<InputState>,
    meeting_search_query: Option<String>,
    meeting_search_result: Option<search::SearchResult>,
    meeting_search_task: Option<crossbeam_channel::Receiver<anyhow::Result<search::SearchResult>>>,
    meeting_search_filters: search::SearchFilters,
    meeting_filter_open: bool,
    meeting_filter_recording_date: bool,
    meeting_filter_source: Option<String>,
    meeting_filter_status: Option<String>,
    meeting_filter_from: Entity<InputState>,
    meeting_filter_to: Entity<InputState>,
    meeting_filter_min: Entity<InputState>,
    meeting_filter_max: Entity<InputState>,
    meeting_filter_tag: Entity<InputState>,
    meeting_filter_speaker: Entity<InputState>,
    semantic_model_input: Entity<InputState>,
    semantic_enabled: bool,
    semantic_status: String,
    semantic_task: Option<crossbeam_channel::Receiver<semantic::SemanticEvent>>,
    semantic_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    semantic_search_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    semantic_task_model: Option<String>,
    meeting_tags_input: Entity<InputState>,
    meeting_tags_base: Option<PathBuf>,
    meeting_tags_baseline: Option<Vec<String>>,
    meeting_tags_identity: Option<Option<String>>,
    rename_base: Option<PathBuf>,
    rename_title_baseline: Option<Option<String>>,
    rename_identity: Option<Option<String>>,
    rename_files: bool,
    rename_migration: Option<bool>,
    rename_job: Option<workspace::EntityRename>,
    relink_busy: bool,
    relink_prompt_generation: u64,
    relink_cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rename_input: Entity<InputState>,
    meeting_title: String,
    speaker_panel_open: bool,
    transcript_media_tab: bool,
    transcript_tools_open: bool,
    transcript_history_page: usize,
    transcript_history_loaded_base: Option<PathBuf>,
    transcript_history_loading_base: Option<PathBuf>,
    transcript_history_files: Vec<PathBuf>,
    transcript_history_unavailable: std::collections::HashSet<PathBuf>,
    transcript_history_error: Option<String>,
    transcript_history_epoch: u64,
    notes: Entity<gpui::component::input::TextareaState>,
    notes_status: String,
    notes_base: Option<PathBuf>,
    notes_saved: String,
    notes_exists: bool,
    notes_load_failed: bool,
    notes_recovery: Option<String>,
    notes_recovery_busy: bool,
    notes_reload_busy: bool,
    notes_initial_load_pending: bool,
    notes_reload_result: Option<workspace::NotesReload>,
    notes_index_pending: Option<std::time::Instant>,
    recording_registered: bool,
}
#[cfg(target_os = "macos")]
const UI_FONT: &str = "PingFang SC";
#[cfg(windows)]
const UI_FONT: &str = "Microsoft YaHei UI";
#[cfg(target_os = "linux")]
const UI_FONT: &str = "Noto Sans CJK SC";
#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(windows)]
const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "linux")]
const MONO_FONT: &str = "DejaVu Sans Mono";
fn native_handle(window: &Window) -> isize {
    match HasWindowHandle::window_handle(window).map(|w| w.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        #[cfg(target_os = "macos")]
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as isize,
        #[cfg(target_os = "linux")]
        Ok(RawWindowHandle::Xlib(h)) => h.window as isize,
        #[cfg(target_os = "linux")]
        Ok(RawWindowHandle::Xcb(h)) => h.window.get() as isize,
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
        cx.set_reduce_motion(config.reduce_motion);
        let hwnd = native_handle(window);
        opencap_platform::set_app_icon(hwnd, false);
        if !std::env::args().any(|a| a == "--preview") {
            let _ = opencap_platform::set_capture_exclusion(hwnd, config.exclude_app);
        }
        let controls =
            opencap_platform::desktop_controls(&config.hotkey_record, &config.hotkey_pause)
                .map_err(|e| message = format!("托盘/快捷键不可用：{e}"))
                .ok();
        let tray_available = controls.as_ref().is_some_and(|c| c.has_tray());
        let close_entity = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            if tray_available {
                opencap_platform::show_native_window(hwnd, false);
            } else {
                let _ = close_entity.update(cx, |this, cx| {
                    this.quit_pending = true;
                    this.request_recording_stop();
                    cx.notify();
                });
            }
            false
        });
        cx.spawn(async move |entity, cx| {
            log::info!("OpenCap foreground polling started");
            loop {
                futures_timer::Delay::new(Duration::from_millis(200)).await;
                if entity.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |entity, cx| {
            loop {
                let Ok(delay) = entity.update(cx, |this, cx| {
                    this.poll_audio_levels(cx);
                    #[allow(unused_mut)]
                    let mut needs_fast = this.active() || this.monitor.is_some();
                    #[cfg(feature = "internal-player")]
                    {
                        if this.poll_player(cx) {
                            cx.notify();
                        }
                        needs_fast |= this.player.is_some();
                    }
                    if needs_fast { 50 } else { 250 }
                }) else {
                    break;
                };
                futures_timer::Delay::new(Duration::from_millis(delay)).await;
            }
        })
        .detach();
        // Reading/exporting saved meetings does not need screen capture access.
        // Enumerate targets only when choosing/starting video capture (or in the UI harness).
        let targets = if !config.audio_only
            && cfg!(feature = "ui-smoke")
            && !std::env::args().any(|a| a == "--ui-no-targets")
        {
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
        let mode = if config.audio_only {
            3
        } else {
            match config.target {
                CaptureTarget::Monitor(_) => 0,
                CaptureTarget::Window(_) => 1,
                CaptureTarget::Region { .. } => 2,
            }
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
        let notes = cx.new(|cx| {
            gpui::component::input::TextareaState::new(window, cx)
                .placeholder("记录自己的备注、补充信息或会后想法，内容自动保存到本机。")
                .auto_grow(10, 20)
        });
        let semantic_model = config.semantic_embedding_model.clone().unwrap_or_default();
        let mut recorder = Self {
            config,
            targets,
            devices,
            session: None,
            stats: RecordingStats::default(),
            controls,
            hwnd,
            window_icon_recording: false,
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
            audio_export_job: None,
            audio_export_stage_seen: 0,
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
            monitor_maintenance_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(5))
                .unwrap_or_else(std::time::Instant::now),
            monitor_levels: (AudioRouteLevels::default(), AudioRouteLevels::default()),
            transcription_tab: !cfg!(feature = "ui-smoke"),
            asr: None,
            asr_live: false,
            revision_runtime: None,
            revision_failure: None,
            recording_without_live_due_to_task: false,
            asr_processed_ms: None,
            asr_models: opencap_asr::ModelAvailability::inspect(&opencap_asr::model_root()),
            transcript: opencap_asr::Transcript::default(),
            transcript_output: None,
            transcript_disk_baseline: None,
            transcript_load_generation: 0,
            transcript_load_cancel: None,
            transcript_load_workers: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(feature = "online-speaker")]
            word_timing_cancel: None,
            #[cfg(feature = "online-speaker")]
            word_timing_status: String::new(),
            transcription_status: "等待录制或导入文件".into(),
            transcription_progress: 0.0,
            transcription_clock: task::TaskClock::default(),
            speaker_clock: task::TaskClock::default(),
            ai_clock: task::TaskClock::default(),
            speaker_threads: 2,
            speaker_compare_version: None,
            speaker_compare_page: 0,
            speakers_view: {
                let view = cx.new(speakers::SpeakersView::new);
                cx.subscribe(&view, |this, _, event, cx| this.edit_speaker(event, cx))
                    .detach();
                view
            },
            voice_busy: false,
            transcript_view: {
                let view = cx.new(|_| transcript_view::TranscriptView::new());
                cx.subscribe(&view, |this, _, event, cx| {
                    this.select_sentence_review(event, cx)
                })
                .detach();
                #[cfg(feature = "internal-player")]
                cx.subscribe(
                    &view,
                    |this, _, event: &transcript_view::PlaybackSeek, cx| {
                        this.seek_player_from_row(event, cx)
                    },
                )
                .detach();
                #[cfg(feature = "internal-player")]
                cx.subscribe(
                    &view,
                    |this, _, event: &transcript_view::PlaybackSelection, cx| {
                        this.select_player_segment_from_row(&event.segment, cx)
                    },
                )
                .detach();
                #[cfg(feature = "internal-player")]
                cx.subscribe(
                    &view,
                    |this, _, event: &transcript_view::PlaybackRangeSelection, cx| {
                        this.select_player_range_from_rows(event, cx)
                    },
                )
                .detach();
                #[cfg(feature = "internal-player")]
                cx.subscribe(
                    &view,
                    |this, _, event: &transcript_view::PlaybackTokenSeek, cx| {
                        this.seek_player_from_token(event, cx)
                    },
                )
                .detach();
                #[cfg(feature = "internal-player")]
                cx.subscribe(
                    &view,
                    |this, _, event: &transcript_view::PlaybackWordRange, cx| {
                        this.play_aligned_word(event, cx)
                    },
                )
                .detach();
                view
            },
            transcript_dirty: true,
            transcript_new_document: true,
            task_display_second: 0,
            preparing_model: None,
            prepare_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_backfill: None,
            backfill_checking: false,
            summary_task: None,
            summary_task_base: None,
            summary_text: String::new(),
            live_notes: None,
            live_notes_status: String::new(),
            live_archive_preview: None,
            live_archive_loading: false,
            live_notes_credentials_busy: false,
            live_notes_preparing: false,
            live_notes_prepare_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            #[cfg(feature = "online-speaker")]
            online_speakers: None,
            #[cfg(feature = "online-speaker")]
            online_speaker_status: String::new(),
            #[cfg(feature = "online-speaker")]
            backfill_speaker_history: None,
            #[cfg(feature = "online-speaker")]
            pending_speaker_sync: false,
            live_note_edit_input: cx
                .new(|cx| InputState::new(window, cx).placeholder("修改纪要条目")),
            #[cfg(feature = "internal-player")]
            player: None,
            #[cfg(feature = "internal-player")]
            player_base: None,
            #[cfg(feature = "internal-player")]
            player_media: None,
            #[cfg(feature = "internal-player")]
            player_snapshot: None,
            #[cfg(feature = "internal-player")]
            player_image: None,
            #[cfg(feature = "internal-player")]
            player_clear_image: false,
            #[cfg(feature = "internal-player")]
            pending_player: None,
            #[cfg(feature = "internal-player")]
            player_start_pending: false,
            #[cfg(feature = "internal-player")]
            player_selection_anchor: None,
            #[cfg(feature = "internal-player")]
            player_selection: None,
            #[cfg(feature = "internal-player")]
            player_selection_snapshot: Vec::new(),
            #[cfg(feature = "internal-player")]
            player_selection_run: None,
            #[cfg(feature = "internal-player")]
            player_slider: {
                let slider = cx.new(|_| gpui::component::slider::SliderState::new().step(0.01));
                cx.subscribe(&slider, |this, _, event, cx| {
                    this.player_slider_event(event, cx)
                })
                .detach();
                slider
            },
            #[cfg(feature = "internal-player")]
            player_drag_preview: None,
            #[cfg(feature = "internal-player")]
            player_keyboard_focus: cx.focus_handle(),
            #[cfg(feature = "internal-player")]
            player_waveform: None,
            #[cfg(feature = "internal-player")]
            player_waveform_jobs: Vec::new(),
            #[cfg(feature = "internal-player")]
            player_waveform_status: String::new(),
            ai,
            hotwords_input,
            speaker_count,
            speaker_threshold,
            speaker_task: None,
            transcript_sync: None,
            historical_checking: false,
            historical_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            text_document: None,
            speaker_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            meeting_page: workspace::MeetingPage::Library,
            meetings: Vec::new(),
            meeting_list_page: 0,
            meeting_selected_base: None,
            meeting_rows_scroll: ScrollHandle::new(),
            meeting_list_focus: cx.focus_handle().tab_stop(true),
            meeting_detail_scroll: ScrollHandle::new(),
            meeting_scan: None,
            meeting_scan_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            meeting_scan_again: false,
            meeting_scan_extra: Default::default(),
            pending_legacy_imports: Vec::new(),
            pending_legacy_import_page: 0,
            meeting_search_input: {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder("搜索标题、逐字稿、纪要和笔记")
                });
                cx.subscribe(&input, |this, input, event, cx| {
                    if matches!(event, gpui::component::input::InputEvent::PressEnter { .. }) {
                        let query = input.read(cx).value().to_string();
                        this.start_meeting_search(query);
                        cx.notify();
                    }
                })
                .detach();
                input
            },
            meeting_search_query: None,
            meeting_search_result: None,
            meeting_search_task: None,
            meeting_search_filters: Default::default(),
            meeting_filter_open: false,
            meeting_filter_recording_date: false,
            meeting_filter_source: None,
            meeting_filter_status: None,
            meeting_filter_from: cx
                .new(|cx| InputState::new(window, cx).placeholder("起始日期 YYYY-MM-DD")),
            meeting_filter_to: cx
                .new(|cx| InputState::new(window, cx).placeholder("结束日期 YYYY-MM-DD")),
            meeting_filter_min: cx
                .new(|cx| InputState::new(window, cx).placeholder("最短时长（分钟）")),
            meeting_filter_max: cx
                .new(|cx| InputState::new(window, cx).placeholder("最长时长（分钟）")),
            meeting_filter_tag: cx
                .new(|cx| InputState::new(window, cx).placeholder("标签（完整匹配）")),
            meeting_filter_speaker: cx
                .new(|cx| InputState::new(window, cx).placeholder("说话人姓名")),
            semantic_model_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("已安装的本地嵌入模型名称")
                    .default_value(semantic_model)
            }),
            semantic_enabled: false,
            semantic_status: String::new(),
            semantic_task: None,
            semantic_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            semantic_search_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            semantic_task_model: None,
            meeting_tags_input: cx
                .new(|cx| InputState::new(window, cx).placeholder("用逗号分隔标签")),
            meeting_tags_base: None,
            meeting_tags_baseline: None,
            meeting_tags_identity: None,
            rename_base: None,
            rename_title_baseline: None,
            rename_identity: None,
            rename_files: false,
            rename_migration: None,
            rename_job: None,
            relink_busy: false,
            relink_prompt_generation: 0,
            relink_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rename_input: cx.new(|cx| InputState::new(window, cx).placeholder("会议名称")),
            meeting_title: String::new(),
            speaker_panel_open: false,
            transcript_media_tab: false,
            transcript_tools_open: false,
            transcript_history_page: 0,
            transcript_history_loaded_base: None,
            transcript_history_loading_base: None,
            transcript_history_files: Vec::new(),
            transcript_history_unavailable: std::collections::HashSet::new(),
            transcript_history_error: None,
            transcript_history_epoch: 0,
            notes,
            notes_status: String::new(),
            notes_base: None,
            notes_saved: String::new(),
            notes_exists: false,
            notes_load_failed: false,
            notes_recovery: None,
            notes_recovery_busy: false,
            notes_reload_busy: false,
            notes_initial_load_pending: false,
            notes_reload_result: None,
            notes_index_pending: None,
            recording_registered: false,
        };
        recorder.refresh_meetings();
        recorder
    }
    fn active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.finished())
    }
    fn request_recording_stop(&mut self) {
        if self.active() && self.stats.state != RecordingState::Stopping {
            if let Some(session) = &self.session {
                session.command(RecordingCommand::Stop);
            }
            self.stats.state = RecordingState::Stopping;
        }
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
        if self.quit_pending {
            self.message = "正在退出，请等待当前任务收尾。".into();
            cx.notify();
            return;
        }
        if self.active() {
            self.request_recording_stop();
        } else {
            #[cfg(feature = "internal-player")]
            if self.player_blocks_recording() {
                self.message = "请先暂停或关闭内置播放，再开始录制，以免回放声音被录入。".into();
                cx.notify();
                return;
            }
            #[cfg(feature = "internal-player")]
            self.cancel_waveform_jobs();
            if self.asr.as_ref().is_some_and(|asr| asr.finished()) {
                self.poll_transcription(cx, false);
                #[cfg(feature = "online-speaker")]
                self.poll_online_speakers();
                self.poll_live_notes();
            }
            if !self.config.audio_only && self.targets.is_empty() {
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
            if self.config.audio_only && !self.config.system_audio && !self.config.microphone {
                self.message = "仅录音需要先开启系统声音或麦克风".into();
                cx.notify();
                return;
            }
            #[cfg(feature = "online-speaker")]
            if let Some(cancelled) = &self.word_timing_cancel {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                self.word_timing_status = "开始录制，正在停止词时间任务；录制不会等待。".into();
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
            if self.semantic_task.is_some() {
                self.semantic_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.semantic_status =
                    "录制优先：已请求停止语义索引，当前本地模型请求返回后结束。".into();
            }
            self.semantic_search_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if self.semantic_enabled {
                self.semantic_enabled = false;
                self.semantic_status =
                    "录制期间已切换为关键词搜索；结束后可重新启用混合搜索。".into();
                if let Some(query) = self.meeting_search_query.clone() {
                    self.start_meeting_search(query);
                }
            }
            let unsaved_transcript = self.transcript.primary_save_pending;
            let unsaved_minutes_draft = self.persist_minutes_draft(cx);
            self.persist_notes(cx);
            let unsaved_notes = self.notes_have_unsaved_work(cx);
            let unsaved_live_notes = self
                .live_notes
                .as_ref()
                .is_some_and(|notes| notes.has_unsaved_work());
            let unsaved_tags = self.meeting_tags_base.is_some();
            let unsaved_rename = self.rename_base.is_some();
            // Only another transcript writer (or an unfinished previous
            // document) can conflict with the new live transcript. Model
            // downloads, AI minutes requests, scans and previews for another
            // meeting stay independent; recording never waits for them.
            let busy = self.transcript_write_busy()
                || self.model_prepare_busy()
                || unsaved_transcript
                || unsaved_minutes_draft
                || unsaved_notes
                || unsaved_live_notes
                || unsaved_tags
                || unsaved_rename;
            self.recording_without_live_due_to_task = busy;
            let observer = if busy {
                // Preserve the other meeting's in-flight state; recording has
                // priority, but this version cannot share a single ASR slot.
                if self.asr_live {
                    if let Some(asr) = &mut self.asr {
                        asr.finish_at((self.stats.duration_seconds * 1000.0) as u64);
                    }
                    self.asr_live = false;
                }
                self.message = if unsaved_transcript {
                    "上一场逐字稿尚未保存；本次录制继续保存，实时转录暂不可用。"
                } else if unsaved_minutes_draft {
                    "上一场纪要草稿尚未保存；本次录制继续保存，实时转录暂不可用。"
                } else if unsaved_notes {
                    "上一场笔记尚未保存；本次录制继续保存，实时转录暂不可用。"
                } else if unsaved_live_notes {
                    "上一场增量纪要尚未保存；本次录制继续保存，实时转录暂不可用。"
                } else if unsaved_tags {
                    "上一场标签编辑尚未结束；本次录制继续保存，实时转录暂不可用。"
                } else if unsaved_rename {
                    "上一场名称编辑尚未结束；本次录制继续保存，实时转录暂不可用。"
                } else {
                    "识别、模型准备或另一项逐字稿写入仍在运行；本次录制继续保存，实时转录暂不可用，可在结束后补转。"
                }.into();
                None
            } else {
                self.start_live_asr()
            };
            #[cfg(feature = "online-speaker")]
            let (observers, speaker_started) = {
                let mut observers: Vec<_> = observer
                    .into_iter()
                    .map(|sender| AudioSubscription {
                        source: AudioSource::MixedRecording,
                        sender,
                        consumer: opencap_media::AudioConsumer::Asr,
                    })
                    .collect();
                let mut speaker_started = false;
                if !observers.is_empty()
                    && let Some(sender) = self.start_online_speakers()
                {
                    observers.push(AudioSubscription {
                        source: AudioSource::MixedRecording,
                        sender,
                        consumer: opencap_media::AudioConsumer::OnlineSpeaker,
                    });
                    speaker_started = true;
                } else if self.config.online_speakers && observers.is_empty() {
                    self.online_speaker_status = "本场没有实时转录，暂不启动在线说话人。".into();
                }
                (observers, speaker_started)
            };
            #[cfg(not(feature = "online-speaker"))]
            let observers: Vec<_> = observer
                .into_iter()
                .map(|sender| AudioSubscription {
                    source: AudioSource::MixedRecording,
                    sender,
                    consumer: opencap_media::AudioConsumer::Asr,
                })
                .collect();
            match RecordingSession::start_observed_sources(recording, observers) {
                Ok(session) => {
                    session.enable_preview(
                        !self.config.audio_only && self.config.show_recording_preview,
                    );
                    self.stats = session.stats();
                    self.session = Some(session);
                    self.recording_registered = false;
                }
                Err(e) => {
                    self.message = format!("无法开始录制：{e:#}");
                    #[cfg(feature = "online-speaker")]
                    if speaker_started && let Some(controller) = &mut self.online_speakers {
                        controller.cancel("录制未开始，在线说话人已停止。");
                    }
                    if !busy && let Some(asr) = &mut self.asr {
                        asr.cancel();
                    }
                    if !busy {
                        self.asr_live = false;
                    }
                }
            }
        }
        cx.notify();
    }
    fn toggle_pause(&mut self) {
        if self.stats.state == RecordingState::Stopping {
            return;
        }
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
        let task_second = if self.summary_task.is_some() || self.ai.correction_task.is_some() {
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
        if self
            .notes_index_pending
            .is_some_and(|time| time.elapsed() >= Duration::from_secs(1))
        {
            self.notes_index_pending = None;
            self.refresh_meetings();
        }
        needs_render |= self.persist_minutes_draft(cx);
        needs_render |= self.poll_transcription(cx, true);
        #[cfg(feature = "online-speaker")]
        {
            needs_render |= self.poll_online_speakers();
        }
        needs_render |= self.poll_minutes_document();
        needs_render |= self.poll_live_notes();
        #[cfg(feature = "internal-player")]
        {
            needs_render |= self.poll_player(cx);
        }
        needs_render |= self.poll_entity_rename(cx);
        needs_render |= self.poll_meeting_scan();
        needs_render |= self.poll_meeting_search();
        needs_render |= self.poll_semantic_index();
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
                    self.request_recording_stop();
                }
                ShellEvent::Locked => {
                    if self.active() {
                        self.request_recording_stop();
                        self.message = "桌面已锁定，正在结束录制".into();
                    }
                }
                ShellEvent::Warning(w) => self.message = w,
            }
        }
        if let Some(session) = &self.session {
            session.enable_preview(
                !self.config.audio_only
                    && self.config.show_recording_preview
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
            let mut next_stats = session.stats();
            if self.stats.state == RecordingState::Stopping
                && !session.finished()
                && matches!(
                    next_stats.state,
                    RecordingState::Starting | RecordingState::Recording | RecordingState::Paused
                )
            {
                next_stats.state = RecordingState::Stopping;
            }
            let stats_changed = next_stats.state != self.stats.state
                || (next_stats.duration_seconds - self.stats.duration_seconds).abs() >= 0.2
                || next_stats.bytes != self.stats.bytes
                || next_stats.error != self.stats.error
                || next_stats.audio_routing != self.stats.audio_routing
                || next_stats.audio_routes != self.stats.audio_routes
                || next_stats.system_peak != self.stats.system_peak
                || next_stats.microphone_peak != self.stats.microphone_peak
                || next_stats.system_rms != self.stats.system_rms
                || next_stats.microphone_rms != self.stats.microphone_rms
                || next_stats.system_input_rms != self.stats.system_input_rms
                || next_stats.microphone_input_rms != self.stats.microphone_input_rms
                || next_stats.warnings.len() != self.stats.warnings.len();
            self.stats = next_stats;
            if self.asr_live
                && let Some(asr) = &mut self.asr
            {
                if let Some(path) = &self.stats.output {
                    let media = transcription::final_recording_path(path);
                    let base = meeting_base(&media);
                    if self.transcript_output.as_ref() != Some(&base) {
                        match asr.set_output(media) {
                            Ok(()) => {
                                #[cfg(feature = "online-speaker")]
                                if let Some(controller) = &mut self.online_speakers
                                    && controller.base.is_none()
                                {
                                    controller.base = Some(base.clone());
                                }
                                self.transcript_output = Some(base);
                            }
                            Err(error) => {
                                asr.cancel();
                                self.asr_live = false;
                                self.transcription_status =
                                    format!("无法锁定转录保存位置，录制继续：{error:#}");
                            }
                        }
                    }
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
        // The taskbar's window icons and notification-area icon share one state.
        // Starting/paused/finalizing still belong to an active recording session.
        let recording = matches!(
            self.stats.state,
            RecordingState::Starting
                | RecordingState::Recording
                | RecordingState::Paused
                | RecordingState::Stopping
        );
        if recording != self.window_icon_recording {
            opencap_platform::set_app_icon(self.hwnd, recording);
            self.window_icon_recording = recording;
        }
        if let Some(controls) = &self.controls {
            controls.set_recording_state(recording);
        }
        let wants_monitor = self.config.monitor_levels
            && !self.settings
            && !self.transcription_tab
            && !self.active()
            && !self.meeting_task_busy()
            && (self.config.system_audio || self.config.microphone);
        if !wants_monitor {
            if self.monitor.take().is_some() {
                self.monitor_levels = (AudioRouteLevels::default(), AudioRouteLevels::default());
                needs_render = true;
            }
        } else {
            if self.monitor.is_some()
                && self.monitor_maintenance_at.elapsed() >= Duration::from_secs(5)
            {
                self.monitor_maintenance_at = std::time::Instant::now();
                let routing = self.config.audio_routing();
                let denoise = self.config.mic_denoise;
                if self
                    .monitor
                    .as_mut()
                    .is_some_and(|monitor| !monitor.maintain(&routing, denoise))
                {
                    self.monitor = None;
                    self.monitor_levels =
                        (AudioRouteLevels::default(), AudioRouteLevels::default());
                    self.message = "电平监测线程已退出，将自动重试。".into();
                    needs_render = true;
                }
            }
            if self.monitor.is_none()
                && self.monitor_maintenance_at.elapsed() >= Duration::from_secs(5)
            {
                self.monitor_maintenance_at = std::time::Instant::now();
                match opencap_platform::start_audio_monitor(
                    &self.config.audio_routing(),
                    self.config.mic_denoise,
                ) {
                    Ok(monitor) => {
                        self.monitor_maintenance_at = std::time::Instant::now();
                        self.monitor = Some(monitor);
                    }
                    Err(e) => {
                        self.message = format!("实时电平检测暂不可用，将自动重试：{e}");
                        needs_render = true;
                    }
                }
            }
            if let Some(monitor) = &self.monitor
                && let Some(warning) = monitor.take_warning()
            {
                self.message = warning;
                needs_render = true;
            }
        }
        let recovery_result = self.recovering.as_ref().and_then(|r| match r.try_recv() {
            Ok(result) => Some(result),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("恢复任务异常结束")))
            }
        });
        if let Some(result) = recovery_result {
            needs_render = true;
            self.recovering = None;
            if result.is_err() && self.quit_pending {
                self.quit_pending = false;
                opencap_platform::show_native_window(self.hwnd, true);
            }
            self.message = match result {
                Ok(p) => format!("已恢复到 {}；原临时文件已保留", p.display()),
                Err(e) => format!("恢复失败，原文件已保留：{e:#}"),
            };
        }
        if let Some((_, _, _, stage)) = &self.audio_export_job {
            let current = stage.load(std::sync::atomic::Ordering::Acquire);
            if current != self.audio_export_stage_seen {
                self.audio_export_stage_seen = current;
                needs_render = true;
            }
        }
        let audio_export_result =
            self.audio_export_job
                .as_ref()
                .and_then(|(base, rx, _, _)| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(crossbeam_channel::TryRecvError::Empty) => None,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        Some(Err(anyhow::anyhow!(
                            "音频导出线程异常结束；请检查目标 {} 及会议文件夹中的暂存文件",
                            base.with_extension("m4a").display()
                        )))
                    }
                });
        if let Some(result) = audio_export_result {
            if result.is_err() && self.quit_pending {
                self.quit_pending = false;
                opencap_platform::show_native_window(self.hwnd, true);
            }
            if let Some((base, _, _, _)) = self.audio_export_job.take() {
                self.meeting_scan_extra.insert(base);
                self.refresh_meetings();
            }
            self.message = match result {
                Ok((path, verified)) => format!(
                    "独立音频已导出：{}{}",
                    path.display(),
                    if verified {
                        ""
                    } else {
                        "；原录屏缺少历史指纹，内容身份无法核实。"
                    }
                ),
                Err(error) => format!("独立音频导出未完成：{error:#}"),
            };
            needs_render = true;
        }
        if self.quit_pending && !self.active() {
            if let Some((_, _, cancel, _)) = &self.audio_export_job {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            self.cancel_migration_for_quit();
            if self.live_notes_preparing {
                self.live_notes_prepare_cancel
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            if self.meeting_scan.is_some() {
                self.meeting_scan_cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.meeting_scan_again = false;
            }
            if self.semantic_task.is_some() {
                self.semantic_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.semantic_search_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if self.relink_busy {
                self.relink_cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if self.preparing_model.is_some() {
                self.prepare_cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(cancel) = self.transcript_load_cancel.take() {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
            }
            let unsaved_notes = self
                .live_notes
                .as_ref()
                .is_some_and(|notes| notes.has_unsaved_work());
            let notes_settling = self
                .live_notes
                .as_ref()
                .is_some_and(|notes| notes.settlement_pending());
            let notes_need_attention = self
                .live_notes
                .as_ref()
                .is_some_and(|notes| notes.settlement_needs_attention());
            self.persist_notes(cx);
            let unsaved_regular_notes =
                self.notes_have_unsaved_work(cx) && !self.notes_recovery_busy;
            let unsaved_minutes_draft = self.persist_minutes_draft(cx);
            // A worker can enqueue its terminal event after this frame's
            // poll and exit before this check. Drain that event next frame
            // before deciding whether any result still needs saving.
            let finishing_tasks = self
                .asr
                .as_ref()
                .is_some_and(|asr| !asr.finished() || !asr.events.is_empty())
                || self.preparing_model.is_some()
                || self.summary_task.is_some()
                || self.ai.document_task.is_some()
                || self.ai.correction_task.is_some()
                || self.speaker_task.is_some()
                || self.voice_busy
                || self.transcript_sync.is_some()
                || self.historical_checking
                || self.live_archive_loading
                || self.live_notes_preparing
                || self.relink_busy
                || self.rename_job.is_some()
                || self.backfill_checking
                || self.ai.draft_recovery_busy
                || self.notes_recovery_busy;
            let finishing_tasks = finishing_tasks
                || self.semantic_task.is_some()
                || self.meeting_scan.is_some()
                || self.recovering.is_some()
                || self.audio_export_job.is_some();
            let unsaved_transcript = self.transcript.primary_save_pending && !finishing_tasks;
            let unsaved_tags = self.meeting_tags_base.is_some();
            let unsaved_rename = self.rename_base.is_some();
            let unsaved_speaker_drafts =
                !finishing_tasks && self.speakers_view.read(cx).has_unsaved_drafts(cx);
            #[cfg(feature = "online-speaker")]
            let unsaved_speakers = self
                .online_speakers
                .as_ref()
                .is_some_and(|speaker| speaker.has_unsaved_work());
            #[cfg(feature = "online-speaker")]
            let speaker_awaiting_identity = self
                .online_speakers
                .as_ref()
                .is_some_and(|speaker| speaker.awaiting_identity());
            #[cfg(feature = "online-speaker")]
            let speaker_needs_attention = self
                .online_speakers
                .as_ref()
                .is_some_and(|speaker| speaker.settlement_needs_attention());
            #[cfg(not(feature = "online-speaker"))]
            let unsaved_speakers = false;
            #[cfg(not(feature = "online-speaker"))]
            let speaker_awaiting_identity = false;
            #[cfg(not(feature = "online-speaker"))]
            let speaker_needs_attention = false;
            if unsaved_notes
                || speaker_needs_attention
                || unsaved_regular_notes
                || unsaved_minutes_draft
                || notes_need_attention
                || unsaved_transcript
                || unsaved_speaker_drafts
                || unsaved_tags
                || unsaved_rename
            {
                self.quit_pending = false;
                self.message = if speaker_needs_attention && speaker_awaiting_identity {
                    "未绑定身份的说话人结果尚未保存到恢复目录；请检查状态并等待重试。"
                } else if speaker_needs_attention {
                    "在线说话人结果尚未保存；请检查状态并等待自动重试后再退出。"
                } else if unsaved_regular_notes || unsaved_minutes_draft {
                    "笔记或纪要草稿尚未保存；请检查会议目录并重试。"
                } else if unsaved_tags {
                    "标签编辑尚未结束；请返回对应会议保存或取消后再退出。"
                } else if unsaved_rename {
                    "会议名称编辑尚未结束；请返回对应会议保存或取消后再退出。"
                } else if unsaved_transcript {
                    "逐字稿主文件尚未保存；请在逐字稿页面重试同步后退出。"
                } else if unsaved_speaker_drafts {
                    self.settings = false;
                    self.transcription_tab = true;
                    self.meeting_page = workspace::MeetingPage::Transcript;
                    self.transcript_media_tab = false;
                    self.speaker_panel_open = true;
                    self.text_document = None;
                    "说话人面板有仅存在内存的姓名草稿；请保存、复制并移除后再退出。"
                } else if notes_need_attention {
                    "增量纪要收尾需要处理；请检查状态、重试或停止更新后再退出。"
                } else {
                    "增量纪要有未保存的编辑或结果；请先保存、重试或取消编辑。"
                }
                .into();
                opencap_platform::show_native_window(self.hwnd, true);
                needs_render = true;
            } else if finishing_tasks || notes_settling || unsaved_speakers {
                let message = if speaker_awaiting_identity {
                    "正在等待说话人收尾；未绑定的结果将保存到恢复目录后退出。"
                } else if self.preparing_model.is_some() {
                    "正在取消模型下载，等待当前网络读取和临时文件清理后退出。"
                } else if self.rename_job.is_some() {
                    "正在等待会议文件操作结束；如有迁移，已请求取消。"
                } else if self.semantic_task.is_some() {
                    "正在停止语义索引；等待当前本地模型请求结束后退出。"
                } else if self.meeting_scan.is_some() {
                    "正在停止会议扫描，等待当前文件读取或索引事务结束后退出。"
                } else if self.recovering.is_some() {
                    "正在等待录制文件恢复完成后退出。"
                } else if self.audio_export_job.is_some() {
                    "正在等待独立音频导出完成后退出。"
                } else {
                    "正在等待转写、说话人和纪要任务收尾；完成后将退出。可在任务页面取消不需要的工作。"
                };
                if self.message != message {
                    self.message = message.into();
                    opencap_platform::show_native_window(self.hwnd, true);
                    needs_render = true;
                }
            } else {
                cx.quit();
            }
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
    fn poll_audio_levels(&mut self, cx: &mut Context<Self>) {
        let next = if self.active() {
            self.session.as_ref().map(RecordingSession::audio_levels)
        } else {
            self.monitor.as_ref().map(|monitor| monitor.levels())
        };
        let Some(next) = next else { return };
        let previous = if self.active() {
            (
                AudioRouteLevels {
                    input: AudioLevel {
                        rms: self.stats.system_input_rms,
                        peak: self.stats.system_input_peak,
                    },
                    processed: AudioLevel {
                        rms: self.stats.system_rms,
                        peak: self.stats.system_peak,
                    },
                },
                AudioRouteLevels {
                    input: AudioLevel {
                        rms: self.stats.microphone_input_rms,
                        peak: self.stats.microphone_input_peak,
                    },
                    processed: AudioLevel {
                        rms: self.stats.microphone_rms,
                        peak: self.stats.microphone_peak,
                    },
                },
            )
        } else {
            self.monitor_levels
        };
        if self.active() {
            self.stats.system_rms = next.0.processed.rms;
            self.stats.system_peak = next.0.processed.peak;
            self.stats.system_input_rms = next.0.input.rms;
            self.stats.system_input_peak = next.0.input.peak;
            self.stats.microphone_rms = next.1.processed.rms;
            self.stats.microphone_peak = next.1.processed.peak;
            self.stats.microphone_input_rms = next.1.input.rms;
            self.stats.microphone_input_peak = next.1.input.peak;
        } else {
            self.monitor_levels = next;
        }
        let changed = |a: AudioLevel, b: AudioLevel| {
            (level_bar(a.rms) - level_bar(b.rms)).abs() > 0.002
                || (level_bar(a.peak) - level_bar(b.peak)).abs() > 0.002
                || (level_bar(a.rms) == 0. && level_bar(b.rms) != 0.)
                || (level_bar(a.peak) == 0. && level_bar(b.peak) != 0.)
        };
        if changed(previous.0.input, next.0.input)
            || changed(previous.0.processed, next.0.processed)
            || changed(previous.1.input, next.1.input)
            || changed(previous.1.processed, next.1.processed)
        {
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
        self.config.audio_only = mode == 3;
        if self.config.audio_only {
            self.message.clear();
            self.region_preview = false;
            self.save();
            return;
        }
        self.region_preview = false;
        if self.targets.is_empty() {
            self.refresh_targets();
            self.close_targets();
        }
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
        if self.stats.state == RecordingState::Stopping && self.active() {
            self.monitor = None;
            self.message = "录制正在停止，音频设置将用于下一次录制。".into();
            self.save();
            return;
        }
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
        if self.config.audio_only
            || self.selecting.is_some()
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
        match id {
            None => "系统默认设备（实际设备未确认）".into(),
            Some(id) => self
                .devices
                .iter()
                .find(|d| d.kind == kind && &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| format!("已选设备不可用：{id}")),
        }
    }
    fn recover(&mut self) {
        if self.active() || self.quit_pending || self.recovering.is_some() {
            return;
        }
        let Some(input) = self.recoverables.first().cloned() else {
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.recovering = Some(rx);
        self.message = "正在恢复录制…".into();
        if let Err(error) = std::thread::Builder::new()
            .name("opencap-recording-recovery".into())
            .spawn(move || {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let extension = input.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
                let output = input.with_file_name(format!("Recovered-{stamp}.{extension}"));
                let result = opencap_media::recover_file(&input, &output).map(|_| output);
                let _ = tx.send(result);
            })
        {
            self.recovering = None;
            self.message = format!("无法启动恢复任务，原临时文件保留，可重试：{error}");
        }
    }
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        if self.active() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择会议保存根目录".into()),
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
    opencap_core::recoverable_recordings(dir)
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
                            r.request_recording_stop();
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
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--is-production-build"))
    {
        std::process::exit(if is_dev_profile() { 43 } else { 42 });
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-identity")) {
        println!("{}", app_name());
        return;
    }
    #[cfg(windows)]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() == Some(std::ffi::OsStr::new("--open-path-helper")) {
            let result = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing document path"))
                .and_then(|path| {
                    opencap_platform::open_path_in_helper(std::path::Path::new(&path))
                });
            if let Err(error) = &result {
                eprintln!("OpenCap file handler: {error:#}");
            }
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
    }
    #[cfg(windows)]
    if is_dev_profile() {
        let result = unsafe { opencap_platform::set_developer_app_identity() };
        if let Err(error) = result {
            eprintln!("无法设置开发版 Windows 身份：{error}");
        }
    }
    struct StderrLogger;
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
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
    let level = match std::env::var("OPENCAP_LOG").as_deref() {
        Ok("info") => log::LevelFilter::Info,
        Ok("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Warn,
    };
    log::set_max_level(level);
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
                        title: Some(app_name().into()),
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let _ = opencap_platform::configure_floating_controls(toolbar, owner);
            opencap_platform::show_native_window(r.hwnd, true);
        });
        cx.activate(true);
        if log::log_enabled!(log::Level::Info) {
            let _ = main_window.update(cx, |_, window, _| {
                log::info!(
                    "OpenCap window initialized: viewport={:?}, visibility={:?}",
                    window.viewport_size(),
                    window.visibility()
                );
            });
        }
        #[cfg(feature = "ui-smoke")]
        if let Some(source) = smoke::preview_source() {
            entity.update(cx, |r, cx| {
                r.start_file_transcription(source, false, false);
                let mut args = std::env::args();
                if args.any(|arg| arg == "--ui-page") {
                    r.meeting_page = match args.next().as_deref() {
                        Some("minutes") => workspace::MeetingPage::Minutes,
                        Some("notes") => workspace::MeetingPage::Notes,
                        Some("library") => workspace::MeetingPage::Library,
                        _ => workspace::MeetingPage::Transcript,
                    };
                }
                let mut args = std::env::args();
                if args.any(|arg| arg == "--ui-search")
                    && let Some(query) = args.next()
                {
                    r.transcript_view
                        .update(cx, |view, _| view.set_query(query));
                }
                cx.notify();
            });
        }
        #[cfg(feature = "ui-smoke")]
        smoke::scroll_benchmark(main_window, entity.clone(), cx);
        #[cfg(feature = "ui-smoke")]
        ai::chat_benchmark(main_window, entity.clone(), cx);
        #[cfg(feature = "ui-smoke")]
        if let Some(path) = smoke::output_dir() {
            smoke::run(main_window, entity, path, cx);
        }
        #[cfg(not(feature = "ui-smoke"))]
        let _ = main_window;
    });
}
