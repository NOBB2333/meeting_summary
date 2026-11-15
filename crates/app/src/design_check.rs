//! Isolated rendering smoke test for the redesigned workspaces.
use super::*;

pub fn run(main: WindowHandle<gpui::base::Root>, recorder: Entity<Recorder>, cx: &mut App) {
    let Some(path) =
        smoke::isolated_dir().filter(|_| std::env::args().any(|a| a == "--ui-design-check"))
    else {
        return;
    };
    cx.spawn(async move |cx| {
        let result: anyhow::Result<()> = async {
            std::fs::create_dir_all(&path)?;
            // Measure fresh synthetic frames even when the test window is occluded.
            macro_rules! redraw {
                () => {{
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                    cx.update_window(main.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                }};
            }
            if std::env::args().any(|a| a == "--ui-design-require-meetings") {
                recorder.update(cx, |r, cx| {
                    r.transcript.segments = vec![opencap_asr::Segment {
                        id: 1,
                        revision: 1,
                        start_ms: 1_000,
                        end_ms: 3_000,
                        text: "这是一段仅用于界面检查的虚构发言。".into(),
                        finalized: true,
                        correcting: false,
                        review: None,
                        extra: Default::default(),
                    }];
                    r.transcript_dirty = true;
                    r.transcript_new_document = true;
                    cx.notify();
                });
            }
            let mut rendered = Vec::new();
            for (width, height) in [(1440., 900.), (620., 700.)] {
                main.update(cx, |_, window, _| {
                    window.resize(size(px(width), px(height)))
                })?;
                for page in [
                    "record",
                    "record-swapped",
                    "target-picker",
                    "library-month",
                    "library-week",
                    "library-time",
                    "transcript",
                    "transcript-swapped",
                    "minutes",
                    "notes",
                    "ai",
                    "recognition",
                    "templates",
                    "recording-settings",
                ] {
                    recorder.update(cx, |r, cx| {
                        r.settings = matches!(
                            page,
                            "ai" | "recognition" | "templates" | "recording-settings"
                        );
                        r.transcription_tab = page.starts_with("library-")
                            || page.starts_with("transcript")
                            || page == "minutes"
                            || page == "notes";
                        r.meeting_page = if page.starts_with("transcript") {
                            workspace::MeetingPage::Transcript
                        } else if page == "minutes" {
                            workspace::MeetingPage::Minutes
                        } else if page == "notes" {
                            workspace::MeetingPage::Notes
                        } else {
                            workspace::MeetingPage::Library
                        };
                        r.config.meeting_media_on_right = page.ends_with("-swapped");
                        if page.starts_with("library-") {
                            r.config.meeting_group_by_month = page == "library-month";
                            r.config.meeting_group_by_week = page == "library-week";
                        }
                        r.choosing_target = page == "target-picker";
                        r.settings_page = match page {
                            "recognition" => settings::SettingsPage::Recognition,
                            "templates" => settings::SettingsPage::Templates,
                            "recording-settings" => settings::SettingsPage::Recording,
                            _ => settings::SettingsPage::Ai,
                        };
                        r.page_scroll.set_offset(point(px(0.), px(0.)));
                        cx.notify();
                    });
                    redraw!();
                    futures_timer::Delay::new(Duration::from_millis(300)).await;
                    if page.starts_with("library-")
                        && std::env::args().any(|a| a == "--ui-design-require-meetings")
                    {
                        anyhow::ensure!(
                            recorder.read_with(cx, |r, _| !r.meetings.is_empty()),
                            "design-check fixture did not load"
                        );
                    }
                    if page.starts_with("transcript") {
                        let viewport_height = recorder.read_with(cx, |r, cx| {
                            r.transcript_view
                                .read(cx)
                                .state
                                .viewport_bounds()
                                .size
                                .height
                        });
                        anyhow::ensure!(
                            viewport_height > px(0.),
                            "transcript has no bounded viewport at {width}x{height}"
                        );
                    }
                    rendered.push(format!("{width}x{height}:{page}"));
                }
            }
            // Exercise the actual nested wheel handlers with synthetic text only.
            main.update(cx, |_, window, _| window.resize(size(px(1440.), px(600.))))?;
            recorder.update(cx, |r, cx| {
                r.settings = false;
                r.transcription_tab = false;
                r.config.sidebar_collapsed = false;
                r.config.recording_transcript_collapsed = false;
                r.config.meeting_media_on_right = false;
                r.config.audio_only = false;
                r.config.show_recording_preview = true;
                r.choosing_target = false;
                r.page_scroll.set_offset(point(px(0.), px(0.)));
                r.recording_capture_scroll.set_offset(point(px(0.), px(0.)));
                r.transcript.segments = (0..80).map(|id| opencap_asr::Segment {
                    id, revision: 1, start_ms: id * 1000, end_ms: id * 1000 + 800,
                    text: format!("模拟实时发言第 {id} 句，用于验证滚动。"),
                    finalized: true, correcting: false, review: None, extra: Default::default(),
                }).collect();
                r.transcript_dirty = true;
                r.transcript_new_document = true;
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            recorder.update(cx, |r, cx| {
                let max = r.recording_capture_scroll.max_offset().y;
                r.recording_capture_scroll.set_offset(point(px(0.), -max / 2.));
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(200)).await;
            let (capture_position, capture_before, capture_max, page_before, expanded_width) =
                recorder.read_with(cx, |r, _| (
                    r.recording_capture_scroll.bounds().center(), r.recording_capture_scroll.offset().y,
                    r.recording_capture_scroll.max_offset().y, r.page_scroll.offset().y,
                    r.recording_capture_scroll.bounds().size.width,
                ));
            anyhow::ensure!(capture_max > px(20.), "capture fixture is not scrollable");
            main.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: capture_position, delta: ScrollDelta::Pixels(point(px(0.), px(-30.))), ..Default::default()
                }), cx);
            })?;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            let (capture_after, page_after_capture) = recorder.read_with(cx, |r, _| (r.recording_capture_scroll.offset().y, r.page_scroll.offset().y));
            anyhow::ensure!(capture_after < capture_before && page_after_capture == page_before,
                "capture wheel did not stay in its pane: {capture_before:?} -> {capture_after:?}, page {page_before:?} -> {page_after_capture:?}");
            // The application picker has its own wheel region; its parents must
            // remain stationary, even at the start/end of the inner list.
            recorder.update(cx, |r, cx| {
                r.mode = 1;
                r.choosing_target = true;
                r.targets = (1..=30).map(|i| TargetInfo {
                    target: CaptureTarget::Window(i), name: format!("模拟窗口 {i}"), width: 1280, height: 720,
                }).collect();
                r.recording_capture_scroll.set_offset(point(px(0.), px(0.)));
                r.page_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(250)).await;
            recorder.update(cx, |r, cx| {
                let max = r.recording_capture_scroll.max_offset().y;
                r.recording_capture_scroll.set_offset(point(px(0.), -max));
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(200)).await;
            let (target_position, target_max, outer_before, capture_before_target) = recorder.read_with(cx, |r, _| (
                r.target_scroll.bounds().center(), r.target_scroll.max_offset().y,
                r.page_scroll.offset(), r.recording_capture_scroll.offset(),
            ));
            anyhow::ensure!(target_max > px(20.), "target list fixture is not scrollable: max={target_max:?}, bounds={:?}, choosing={}, mode={}, count={}, settings={}, meeting={}, audio_only={}, page={:?}, capture={:?}, capture_bounds={:?}, capture_max={:?}",
                recorder.read_with(cx, |r, _| r.target_scroll.bounds()),
                recorder.read_with(cx, |r, _| r.choosing_target),
                recorder.read_with(cx, |r, _| r.mode),
                recorder.read_with(cx, |r, _| r.targets.len()),
                recorder.read_with(cx, |r, _| r.settings),
                recorder.read_with(cx, |r, _| r.transcription_tab),
                recorder.read_with(cx, |r, _| r.config.audio_only),
                recorder.read_with(cx, |r, _| r.page_scroll.offset()),
                recorder.read_with(cx, |r, _| r.recording_capture_scroll.offset()),
                recorder.read_with(cx, |r, _| r.recording_capture_scroll.bounds()),
                recorder.read_with(cx, |r, _| r.recording_capture_scroll.max_offset()));
            main.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: target_position, delta: ScrollDelta::Pixels(point(px(0.), px(-90.))), ..Default::default()
                }), cx);
            })?;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            let (target_after, outer_after, capture_after_target) = recorder.read_with(cx, |r, _| (
                r.target_scroll.offset().y, r.page_scroll.offset(), r.recording_capture_scroll.offset(),
            ));
            anyhow::ensure!(target_after < px(0.) && outer_after == outer_before && capture_after_target == capture_before_target,
                "target picker wheel escaped into a parent");
            recorder.update(cx, |r, cx| {
                r.target_scroll.set_offset(point(px(0.), -target_max));
                cx.notify();
            });
            redraw!();
            main.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: target_position, delta: ScrollDelta::Pixels(point(px(0.), px(-90.))), ..Default::default()
                }), cx);
            })?;
            redraw!();
            anyhow::ensure!(recorder.read_with(cx, |r, _| r.page_scroll.offset() == outer_before && r.recording_capture_scroll.offset() == capture_before_target),
                "target picker wheel escaped at the list boundary");
            recorder.update(cx, |r, cx| { r.choosing_target = false; cx.notify(); });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            recorder.update(cx, |r, cx| {
                r.page_scroll.set_offset(point(px(0.), px(0.)));
                let max = r.recording_capture_scroll.max_offset().y;
                r.recording_capture_scroll.set_offset(point(px(0.), -max));
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            main.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: capture_position, delta: ScrollDelta::Pixels(point(px(0.), px(-180.))), ..Default::default()
                }), cx);
            })?;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            let page_after_edge = recorder.read_with(cx, |r, _| r.page_scroll.offset().y);
            anyhow::ensure!(page_after_edge < page_before, "page did not scroll at the capture pane's lower edge");
            recorder.update(cx, |r, cx| {
                r.page_scroll.set_offset(point(px(0.), px(0.)));
                r.config.recording_transcript_collapsed = true;
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(200)).await;
            let collapsed_width = recorder.read_with(cx, |r, _| r.recording_capture_scroll.bounds().size.width);
            anyhow::ensure!(collapsed_width > expanded_width + px(100.), "closing live panel did not expand capture preview");
            recorder.update(cx, |r, cx| {
                r.config.sidebar_collapsed = true;
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(200)).await;
            let narrow_sidebar_width = recorder.read_with(cx, |r, _| r.recording_capture_scroll.bounds().size.width);
            anyhow::ensure!(narrow_sidebar_width > collapsed_width + px(100.), "collapsing navigation did not give space to the preview");
            recorder.update(cx, |r, cx| {
                r.config.sidebar_collapsed = false;
                r.config.recording_transcript_collapsed = false;
                r.page_scroll.set_offset(point(px(0.), px(-770.)));
                cx.notify();
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(250)).await;
            recorder.update(cx, |r, cx| {
                r.home_detail_transcript.update(cx, |view, cx| {
                    view.state.scroll_to(ListOffset { item_ix: 40, offset_in_item: px(0.) });
                    cx.notify();
                });
            });
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(150)).await;
            let (detail_position, detail_before, detail_page_before) = recorder.read_with(cx, |r, cx| (
                r.home_detail_transcript.read(cx).state.viewport_bounds().center(),
                r.home_detail_transcript.read(cx).state.logical_scroll_top().item_ix,
                r.page_scroll.offset().y,
            ));
            anyhow::ensure!(detail_before > 0, "second screen transcript is not scrollable");
            main.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: detail_position, delta: ScrollDelta::Pixels(point(px(0.), px(180.))), ..Default::default()
                }), cx);
            })?;
            redraw!();
            futures_timer::Delay::new(Duration::from_millis(120)).await;
            let (detail_after, detail_page_after) = recorder.read_with(cx, |r, cx| (
                r.home_detail_transcript.read(cx).state.logical_scroll_top().item_ix, r.page_scroll.offset().y,
            ));
            anyhow::ensure!(detail_after < detail_before && detail_page_after == detail_page_before,
                "second screen transcript wheel did not stay in its pane");
            // Load a separate synthetic meeting while the current transcript grows.
            let readonly_base = std::fs::canonicalize(&path)?.join("readonly-fixture");
            let old = opencap_asr::Transcript {
                complete: true,
                segments: vec![opencap_asr::Segment {
                    id: 10_000, revision: 1, start_ms: 0, end_ms: 800,
                    text: "历史会议中的虚构文字。".into(), finalized: true,
                    correcting: false, review: None, extra: Default::default(),
                }],
                ..Default::default()
            };
            old.checkpoint(&readonly_base)?;
            recorder.update(cx, |r, cx| {
                r.config.output_dir = path.clone();
                r.refresh_meetings();
                cx.notify();
            });
            for _ in 0..50 {
                if recorder.read_with(cx, |r, _| r.meetings.iter().any(|meeting| meeting.base == readonly_base)) { break; }
                redraw!();
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
            recorder.update(cx, |r, cx| {
                let meeting = r.meetings.iter().find(|meeting| meeting.base == readonly_base).cloned()
                    .ok_or_else(|| anyhow::anyhow!("synthetic readonly meeting not found"))?;
                r.open_readonly_library_meeting(meeting, cx);
                r.transcript.segments.push(opencap_asr::Segment {
                    id: 20_000, revision: 1, start_ms: 80_000, end_ms: 81_000,
                    text: "当前录制仍在继续。".into(), finalized: true,
                    correcting: false, review: None, extra: Default::default(),
                });
                r.transcript_dirty = true;
                cx.notify();
                Ok::<(), anyhow::Error>(())
            })?;
            for _ in 0..50 {
                if recorder.read_with(cx, |r, _| r.readonly_meeting.as_ref().is_some_and(|meeting| !meeting.loading)) { break; }
                redraw!();
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
            let (readonly_ok, rows, current_intact) = recorder.read_with(cx, |r, cx| (
                r.readonly_meeting.as_ref().is_some_and(|meeting| !meeting.loading && meeting.error.is_none()),
                r.readonly_view.read(cx).smoke_row_count(),
                r.transcript.segments.len() == 81 && r.transcript.segments.last().is_some_and(|s| s.id == 20_000),
            ));
            anyhow::ensure!(readonly_ok && rows == 1 && current_intact, "readonly document changed the current transcript or did not load");
            std::fs::write(
                path.join("ui-design-check.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"passed": true, "rendered": rendered, "recording_scroll": {
                        "capture_scrolled": true, "target_picker_isolated": true,
                        "page_scrolls_at_edge": true, "collapse_expands_capture": true,
                        "sidebar_collapse_expands_capture": true, "second_screen_transcript_scrolled": true,
                        "readonly_meeting_keeps_current_transcript": true
                    }}),
                )?,
            )?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = std::fs::write(path.join("ui-design-check-error.txt"), format!("{error:#}"));
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}
