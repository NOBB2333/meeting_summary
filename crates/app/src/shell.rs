use super::*;
use gpui::assets::IconName;

impl Recorder {
    fn task_banner(&self) -> Option<Div> {
        // The live workspace displays recording progress alongside the transcript.
        if self.settings || self.active() {
            return None;
        }
        if self.transcription_tab
            && !self.settings
            && matches!(
                self.meeting_page,
                workspace::MeetingPage::Library
                    | workspace::MeetingPage::Minutes
                    | workspace::MeetingPage::Transcript
            )
        {
            return None; // Bounded document panes show their own progress without shifting the reader.
        }
        let progress = if self.ai.correction_task.is_some() {
            ui::task_progress(
                "correction-task-progress",
                "正在校正转写全文",
                &self.ai.status,
                &self.ai_clock,
                None,
            )
        } else if self.speaker_task.is_some() {
            ui::task_progress(
                "speaker-task-progress",
                "正在区分说话人",
                &self.transcription_status,
                &self.speaker_clock,
                None,
            )
        } else if self.summary_task.is_some() {
            ui::task_progress(
                "ai-task-progress",
                "正在整理会议纪要",
                &self.ai.status,
                &self.ai_clock,
                None,
            )
        } else if self.preparing_model.is_some() || self.asr.as_ref().is_some_and(|a| !a.finished())
        {
            ui::task_progress(
                "asr-task-progress",
                if self.preparing_model.is_some() {
                    "正在下载识别模型"
                } else {
                    "正在转录逐字稿"
                },
                &self.transcription_status,
                &self.transcription_clock,
                if self.transcription_progress > 0. && self.transcription_progress < 1. {
                    Some(self.transcription_progress)
                } else {
                    None
                },
            )
        } else {
            return None;
        };
        Some(div().flex_shrink_0().px_6().py_3().child(progress))
    }
    fn navigate(&mut self, page: usize, cx: &mut Context<Self>) {
        self.asr_models = opencap_asr::ModelAvailability::inspect(&opencap_asr::model_root());
        self.close_targets();
        self.audio_picker = None;
        self.settings = page == 2;
        self.transcription_tab = page == 1;
        if page == 1 {
            self.meeting_page = workspace::MeetingPage::Library;
            self.refresh_meetings();
        }
        self.page_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn navigation(&self, compact: bool, collapsed: bool, cx: &Context<Self>) -> Div {
        let mut nav = div()
            .flex()
            .gap_2()
            .when(!compact || collapsed, |d| d.flex_col().w_full());
        for (i, (title, icon)) in [
            ("录制工作台", IconName::Monitor),
            ("会议文件", IconName::Folder),
            ("设置", IconName::Settings),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = if i == 2 {
                self.settings
            } else if i == 1 {
                !self.settings && self.transcription_tab
            } else {
                !self.settings && !self.transcription_tab
            };
            let item = if collapsed {
                ui::icon_button(("main-navigation", i), icon, title)
                    .ghost()
                    .size(px(40.))
                    .selected(selected)
                    .when(selected, |b| b.bg(rgb(SOFT)).text_color(rgb(ACCENT)))
            } else if compact {
                button(("main-navigation", i), title, selected)
                    .icon(icon)
                    .ghost()
                    .h(px(42.))
            } else {
                ui::nav_button(("main-navigation", i), title, icon, selected)
            };
            nav = nav.child(item.on_click(cx.listener(move |this, _, _, cx| this.navigate(i, cx))));
        }
        nav
    }

    fn sidebar(&self, cx: &Context<Self>) -> Div {
        let collapsed = self.config.sidebar_collapsed;
        div()
            .w(px(if collapsed { 64. } else { 212. }))
            .h_full()
            .flex_shrink_0()
            .px_3()
            .py_4()
            .bg(rgb(SURFACE))
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .when(collapsed, |d| d.flex_col())
                    .py_1()
                    .when(!collapsed, |d| {
                        d.child(
                            div()
                                .size(px(28.))
                                .rounded_lg()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(SURFACE))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .child("OC"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_base()
                                        .font_weight(FontWeight::BOLD)
                                        .child(app_name()),
                                )
                                .child(label("会议记录工作台")),
                        )
                    })
                    .child(
                        ui::icon_button(
                            "toggle-sidebar",
                            if collapsed {
                                IconName::PanelLeftOpen
                            } else {
                                IconName::PanelLeftClose
                            },
                            if collapsed {
                                "展开导航栏"
                            } else {
                                "收起导航栏"
                            },
                        )
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.sidebar_collapsed = !this.config.sidebar_collapsed;
                            this.save();
                            cx.notify();
                        })),
                    ),
            )
            .child(self.navigation(false, collapsed, cx))
            .child(div().flex_1())
            .when(!collapsed, |d| {
                d.child(
                    button("sidebar-folder", "打开保存根目录", false)
                        .icon(IconName::FolderOpen)
                        .ghost()
                        .w_full()
                        .justify_start()
                        .on_click(cx.listener(|this, _, _, cx| {
                            let _ = std::fs::create_dir_all(&this.config.output_dir);
                            if let Err(e) = opencap_platform::open_path(&this.config.output_dir) {
                                this.message = e.to_string();
                                cx.notify();
                            }
                        })),
                )
            })
            .when(collapsed, |d| {
                d.child(
                    ui::icon_button(
                        "sidebar-folder-icon",
                        IconName::FolderOpen,
                        "打开保存根目录",
                    )
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = std::fs::create_dir_all(&this.config.output_dir);
                        if let Err(e) = opencap_platform::open_path(&this.config.output_dir) {
                            this.message = e.to_string();
                        }
                        cx.notify();
                    })),
                )
            })
            .when(!collapsed, |d| {
                d.child(
                    button("quit", "退出应用", false)
                        .ghost()
                        .w_full()
                        .justify_start()
                        .on_click(cx.listener(|this, _, _, cx| this.request_quit(cx))),
                )
            })
            .when(collapsed, |d| {
                d.child(
                    ui::icon_button("quit-icon", IconName::LogOut, "退出应用")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.request_quit(cx))),
                )
            })
            .when(!collapsed, |d| {
                d.child(label(format!("{} {}", app_name(), env!("CARGO_PKG_VERSION"))).px_3())
            })
    }

    fn recording_status(&self) -> &'static str {
        match self.stats.state {
            RecordingState::Idle => "准备就绪",
            RecordingState::Starting => "正在准备…",
            RecordingState::Recording => "正在录制",
            RecordingState::Paused => "已暂停",
            RecordingState::Stopping => "正在保存…",
            RecordingState::Finished => "录制已保存",
            RecordingState::Failed => "录制未完成",
        }
    }

    fn topbar(&self, compact: bool, cx: &Context<Self>) -> Div {
        let model_ready = self
            .asr_models
            .live_plan_for(
                self.config.live_asr_model,
                self.config.offline_asr_model,
                self.config.transcription_context_revision,
            )
            .is_some();
        let title = if self.settings {
            "全局设置"
        } else if self.transcription_tab {
            "会议文件"
        } else {
            "录制工作台"
        };
        div()
            .flex_shrink_0()
            .px_6()
            .h(px(50.))
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .items_center()
            .when(!compact, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .w_full()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .text_base()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(title),
                                )
                                .child(ui::badge(
                                    if self.active() {
                                        self.recording_status()
                                    } else if model_ready {
                                        "本地模型就绪"
                                    } else {
                                        "本地模型未就绪"
                                    },
                                    if self.active() {
                                        RED
                                    } else if model_ready {
                                        ui::SUCCESS
                                    } else {
                                        MUTED
                                    },
                                    if self.active() {
                                        0xfdecee
                                    } else if model_ready {
                                        0xe6f4ef
                                    } else {
                                        0xeff2f7
                                    },
                                )),
                        )
                        .child(
                            label(format!("输出目录：{}", self.config.output_dir.display()))
                                .truncate(),
                        ),
                )
            })
            .when(compact, |d| d.child(self.navigation(true, false, cx)))
    }
    fn recording_bar(&self, compact: bool, cx: &Context<Self>) -> Div {
        let active = self.active();
        let transitioning = matches!(
            self.stats.state,
            RecordingState::Starting | RecordingState::Stopping
        );
        div()
            .flex_shrink_0()
            .px_6()
            .py_3()
            .bg(rgb(SURFACE))
            .border_t_1()
            .border_color(rgb(BORDER))
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(ui::badge(
                                self.recording_status(),
                                if active { RED } else { MUTED },
                                if active { 0xfdecee } else { 0xeff2f7 },
                            ))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_base()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(time_text(self.stats.duration_seconds)),
                            ),
                    )
                    .when(active, |d| {
                        d.child(label(if self.config.audio_only {
                            format!(
                                "录音 · {:.0} kbps · {:.1} MB",
                                self.stats.bitrate_kbps,
                                self.stats.bytes as f64 / 1_000_000.
                            )
                        } else {
                            format!(
                                "编码 {:.1} FPS · {:.0} kbps · {:.1} MB",
                                self.stats.fps,
                                self.stats.bitrate_kbps,
                                self.stats.bytes as f64 / 1_000_000.
                            )
                        }))
                    })
                    .when(active, |d| {
                        d.child(label(self.recording_token_usage.label()))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(!compact && !active, |d| {
                        d.child(label(format!("{} 开始 / 停止", self.config.hotkey_record)))
                    })
                    .child(
                        button(
                            "pause",
                            if self.stats.state == RecordingState::Paused {
                                "继续录制"
                            } else {
                                "暂停"
                            },
                            false,
                        )
                        .icon(if self.stats.state == RecordingState::Paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        })
                        .h(px(32.))
                        .disabled(transitioning || !active)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_pause();
                            cx.notify();
                        })),
                    )
                    .when(
                        !active && self.stats.state == RecordingState::Finished && !compact,
                        |d| {
                            d.child(
                                button("play-last", "播放录制", false)
                                    .icon(IconName::Play)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(path) = &this.stats.output
                                            && let Err(e) = opencap_platform::open_path(path)
                                        {
                                            this.message = e.to_string();
                                            cx.notify();
                                        }
                                    })),
                            )
                        },
                    )
                    .child(
                        primary_button(
                            "record",
                            if transitioning {
                                self.recording_status()
                            } else if active {
                                "停止并保存"
                            } else if self.config.audio_only {
                                "开始录音"
                            } else {
                                "开始录制"
                            },
                        )
                        .icon(if active {
                            IconName::Square
                        } else {
                            IconName::GalleryVerticalEnd
                        })
                        .danger()
                        .h(px(32.))
                        .min_w(px(112.))
                        .disabled(transitioning || (!active && self.recovering.is_some()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.mode == 2
                                && !matches!(this.config.target, CaptureTarget::Region { .. })
                            {
                                this.pick_region();
                                cx.notify();
                            } else {
                                this.toggle_record(cx);
                            }
                        })),
                    ),
            )
    }

    fn notification(&self, cx: &Context<Self>) -> Option<Div> {
        let message = self
            .stats
            .error
            .clone()
            .or_else(|| self.stats.warnings.last().cloned())
            .unwrap_or_else(|| self.message.clone());
        if message.is_empty() {
            return None;
        }
        let permission = message.contains("权限") || message.contains("ScreenCaptureKit");
        let brief = if permission {
            "屏幕或声音权限尚未生效，请授权后刷新。".to_owned()
        } else if message.chars().count() > 100 {
            format!("{}…", message.chars().take(100).collect::<String>())
        } else {
            message.clone()
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w(px(160.)).text_sm().child(brief));
        #[cfg(target_os = "macos")]
        if permission {
            let microphone = message.contains("麦克风") && !message.contains("ScreenCaptureKit");
            row = row.child(button("privacy-settings", "权限设置", false).ghost().on_click(cx.listener(move |_, _, _, cx| cx.open_url(if microphone { "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone" } else { "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture" }))))
                .child(button("refresh-permissions", "刷新", false).ghost().on_click(cx.listener(|this, _, _, cx| { this.refresh_targets(); cx.notify(); })));
        }
        row = row.child(
            button(
                "message-details",
                if self.message_expanded {
                    "收起详情"
                } else {
                    "详情"
                },
                false,
            )
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.message_expanded = !this.message_expanded;
                cx.notify();
            })),
        );
        let mut notice = div()
            .flex_shrink_0()
            .px_6()
            .py_2()
            .bg(rgb(0xfff4e7))
            .text_color(rgb(0x86501e))
            .flex()
            .flex_col()
            .gap_2()
            .child(row);
        if self.message_expanded {
            notice = notice.child(div().text_xs().child(message));
        }
        Some(notice)
    }
}

impl Render for Recorder {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "linux")]
        if log::log_enabled!(log::Level::Info) {
            static FIRST_RENDER: std::sync::Once = std::sync::Once::new();
            FIRST_RENDER.call_once(|| {
                log::info!(
                    "OpenCap main view rendered: viewport={:?}, visibility={:?}",
                    window.viewport_size(),
                    window.visibility()
                );
            });
        }
        self.sync_notes(window, cx);
        self.sync_transcript_history(cx);
        self.sync_ai_view(window, cx);
        #[cfg(feature = "internal-player")]
        self.sync_player_slider(window, cx);
        #[cfg(feature = "online-speaker")]
        let online_editable = self.online_speakers.as_ref().is_some_and(|controller| {
            controller.applies_to(self.transcript_output.as_ref(), &self.transcript)
        });
        #[cfg(not(feature = "online-speaker"))]
        let online_editable = false;
        #[cfg(feature = "internal-player")]
        let split_position = (self.player_base == self.transcript_output)
            .then(|| {
                self.player_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.position_ms)
            })
            .flatten();
        #[cfg(not(feature = "internal-player"))]
        let split_position = None;
        if self.transcript_new_document {
            self.speaker_compare_version = None;
            self.speaker_compare_page = 0;
        }
        self.speakers_view.update(cx, |view, cx| {
            view.replace(
                &self.transcript,
                self.transcript_new_document,
                self.transcript_dirty,
                self.active() || self.transcript_write_busy(),
                online_editable,
                split_position,
                window,
                cx,
            )
        });
        let task_running = self.asr.as_ref().is_some_and(|asr| !asr.finished());
        if self.transcript_dirty {
            self.transcript_view.update(cx, |view, cx| {
                view.replace(
                    &self.transcript,
                    self.transcript_new_document,
                    task_running,
                    cx,
                )
            });
            self.home_detail_dirty = true;
            self.home_detail_new_document |= self.transcript_new_document;
            self.transcript_dirty = false;
            self.transcript_new_document = false;
        }
        // Do not reformat the entire live history twice while the second screen
        // is offscreen. Its independent list catches up as soon as it is shown.
        if self.home_detail_dirty
            && !self.settings
            && !self.transcription_tab
            && self.page_scroll.offset().y < px(-32.)
        {
            self.home_detail_transcript.update(cx, |view, cx| {
                view.replace(
                    &self.transcript,
                    self.home_detail_new_document,
                    task_running,
                    cx,
                )
            });
            self.home_detail_dirty = false;
            self.home_detail_new_document = false;
        }
        let compact_transcript = !self.settings && !self.transcription_tab;
        self.transcript_view.update(cx, |view, cx| {
            view.set_compact(compact_transcript, cx);
        });
        #[cfg(feature = "internal-player")]
        self.transcript_view.update(cx, |view, cx| {
            view.set_playback_available(
                self.transcription_tab
                    && !self.settings
                    && !self.active()
                    && self.transcript_output.is_some()
                    && self.transcript.source.is_some(),
                cx,
            );
        });
        self.home_detail_transcript
            .update(cx, |view, cx| view.set_compact(true, cx));
        let viewport = f32::from(window.viewport_size().width);
        let compact = ui::sidebar_width(viewport) == 0.;
        let width = ui::content_width(viewport)
            + if !compact && self.config.sidebar_collapsed {
                148.
            } else {
                0.
            };
        let documents = self.transcription_tab
            && !self.settings
            && !matches!(
                self.meeting_page,
                workspace::MeetingPage::Library | workspace::MeetingPage::ReadOnly
            );
        let content = if self.settings {
            self.settings_view(width, cx)
        } else if self.transcription_tab {
            self.meeting_workspace(width, f32::from(window.viewport_size().height), cx)
        } else {
            self.recording_view(width, f32::from(window.viewport_size().height), cx)
        };
        // The library is a bounded two-pane workspace: its list/detail panes
        // use `h_full` and must not be placed inside the page scroll, where an
        // indefinite height collapses them to zero.
        let bounded_document = self.transcription_tab
            && !self.settings
            && matches!(
                self.meeting_page,
                workspace::MeetingPage::Library
                    | workspace::MeetingPage::ReadOnly
                    | workspace::MeetingPage::Minutes
                    | workspace::MeetingPage::Transcript
                    | workspace::MeetingPage::Notes
            );
        let page_content = if bounded_document {
            div().size_full().min_h(px(0.)).px_6().py_4().child(content)
        } else {
            widgets::scrollable(
                "page-scroll",
                div()
                    .w_full()
                    .min_h_full()
                    .px_6()
                    .py(px(18.))
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(content),
                &self.page_scroll,
            )
        };
        let mut workspace = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .child(self.topbar(compact, cx))
            .when(documents, |d| d.child(self.meeting_header(compact, cx)))
            .when_some(self.task_banner(), |d, banner| d.child(banner))
            .child(div().flex_1().min_h(px(0.)).child(page_content));
        if let Some(notice) = self.notification(cx) {
            workspace = workspace.child(notice);
        }
        workspace = workspace.child(self.recording_bar(compact, cx));
        div()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family(UI_FONT)
            .text_sm()
            .flex()
            .when(!compact, |d| d.child(self.sidebar(cx)))
            .child(workspace)
    }
}
