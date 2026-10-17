use super::*;
use gpui::component::IconName;

impl Recorder {
    fn task_banner(&self) -> Option<Div> {
        if self.summary_task.is_some()
            && self.transcription_tab
            && !self.settings
            && self.meeting_page == workspace::MeetingPage::Minutes
        {
            return None; // The conversation shows its own status and timer without moving the document.
        }
        let progress = if self.speaker_task.is_some() {
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
                "正在转录逐字稿",
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

    fn navigation(&self, compact: bool, cx: &Context<Self>) -> Div {
        let mut nav = div()
            .flex()
            .gap_2()
            .when(!compact, |d| d.flex_col().w_full());
        for (i, (title, icon)) in [
            ("录制工作台", IconName::GalleryVerticalEnd),
            ("会议文件", IconName::FolderOpen),
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
            let item = if compact {
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
        div()
            .w(px(208.))
            .h_full()
            .flex_shrink_0()
            .p_4()
            .bg(rgb(SURFACE))
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_3()
                    .child(
                        div()
                            .size(px(34.))
                            .rounded_lg()
                            .bg(rgb(ACCENT))
                            .text_color(rgb(SURFACE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(IconName::GalleryVerticalEnd)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("OpenCap"),
                            )
                            .child(label("会议记录工作台")),
                    ),
            )
            .child(label("工作空间").px_3())
            .child(self.navigation(false, cx))
            .child(div().flex_1())
            .child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(BG))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("从会议到行动"),
                    )
                    .child(label("录制 → 转写 → 纪要"))
                    .child(label("文件与笔记保存在本机")),
            )
            .child(
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
            .child(
                button("quit", "退出应用", false)
                    .ghost()
                    .w_full()
                    .justify_start()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.persist_minutes_draft(cx);
                        this.persist_notes(cx);
                        this.quit_pending = true;
                        if let Some(s) = &this.session {
                            s.command(RecordingCommand::Stop);
                        }
                        if !this.active() {
                            cx.quit();
                        }
                    })),
            )
            .child(label("OpenCap 0.1.0").px_3())
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
        let title = if self.settings {
            "工作空间 / 设置"
        } else if self.transcription_tab {
            "工作空间 / 会议文件"
        } else {
            "工作空间 / 录制"
        };
        div()
            .flex_shrink_0()
            .px_6()
            .py_3()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(label(title))
                    .child(ui::badge(
                        self.recording_status(),
                        if self.active() { RED } else { ui::SUCCESS },
                        if self.active() { 0xffedf0 } else { 0xeaf6f0 },
                    )),
            )
            .when(compact, |d| d.child(self.navigation(true, cx)))
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
            .py_4()
            .bg(rgb(SURFACE))
            .border_t_1()
            .border_color(rgb(BORDER))
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_full().bg(rgb(if active {
                                RED
                            } else {
                                MUTED
                            })))
                            .child(
                                div()
                                    .font_family(MONO_FONT)
                                    .text_2xl()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(time_text(self.stats.duration_seconds)),
                            ),
                    )
                    .child(label(if !compact && active {
                        format!(
                            "{:.1} FPS · {:.0} kbps · {:.1} MB",
                            self.stats.fps,
                            self.stats.bitrate_kbps,
                            self.stats.bytes as f64 / 1_000_000.
                        )
                    } else {
                        self.recording_status().to_owned()
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(!compact && !active, |d| {
                        d.child(label(format!("{} 开始 / 停止", self.config.hotkey_record)))
                    })
                    .when(active, |d| {
                        d.child(
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
                            .h(px(44.))
                            .disabled(transitioning)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_pause();
                                cx.notify();
                            })),
                        )
                    })
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
                        .h(px(44.))
                        .min_w(px(144.))
                        .disabled(
                            transitioning
                                || (!active
                                    && (self.transcription_busy() || self.recovering.is_some())),
                        )
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
        self.sync_notes(window, cx);
        self.sync_ai_view(window, cx);
        if self.transcript_dirty {
            self.transcript_view.update(cx, |view, cx| {
                view.replace(&self.transcript, self.transcript_new_document, cx)
            });
            self.transcript_dirty = false;
            self.transcript_new_document = false;
        }
        let viewport = f32::from(window.viewport_size().width);
        let compact = ui::sidebar_width(viewport) == 0.;
        let width = ui::content_width(viewport);
        let documents = self.transcription_tab
            && !self.settings
            && self.meeting_page != workspace::MeetingPage::Library;
        let content = if self.settings {
            self.settings_view(cx)
        } else if self.transcription_tab {
            self.meeting_workspace(width, f32::from(window.viewport_size().height), cx)
        } else {
            self.recording_view(width, cx)
        };
        let mut workspace = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .child(self.topbar(compact, cx))
            .when(documents, |d| d.child(self.meeting_header(cx)))
            .when_some(self.task_banner(), |d, banner| d.child(banner))
            .child(
                div().flex_1().min_h(px(0.)).child(widgets::scrollable(
                    "page-scroll",
                    div()
                        .w_full()
                        .min_h_full()
                        .p(px(28.))
                        .flex()
                        .flex_col()
                        .gap_5()
                        .child(content),
                    &self.page_scroll,
                )),
            );
        if let Some(notice) = self.notification(cx) {
            workspace = workspace.child(notice);
        }
        if (!self.settings && !self.transcription_tab) || self.active() {
            workspace = workspace.child(self.recording_bar(compact, cx));
        }
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
