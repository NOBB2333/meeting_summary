use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPage {
    Ai,
    Transcription,
    Recording,
}

impl Recorder {
    pub(super) fn settings_view(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(ui::heading("设置", "按工作流程管理服务、模型和录制偏好。"))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        (SettingsPage::Ai, "AI 服务"),
                        (SettingsPage::Transcription, "语音转录"),
                        (SettingsPage::Recording, "录制偏好"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (page, title))| {
                        button(("settings-page", i), title, self.settings_page == page).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.settings_page = page;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            }),
                        )
                    }),
                ),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(860.))
                    .child(match self.settings_page {
                        SettingsPage::Ai => self.ai_settings_view(cx),
                        SettingsPage::Transcription => self.transcription_settings(cx),
                        SettingsPage::Recording => self.recording_settings(cx),
                    }),
            )
    }
    fn recording_settings(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(self.interface_settings(cx))
            .child(self.audio_settings(cx))
            .child(self.video_settings(cx))
            .child(self.storage_settings(cx))
    }
    fn interface_settings(&self, cx: &Context<Self>) -> Div {
        ui::card()
            .child(ui::section_title("界面与提示", "录制过程中显示什么"))
            .child(ui::setting_row(
                "悬浮控制条",
                "录制时在桌面显示暂停与停止按钮",
                button(
                    "floating-controls",
                    if self.config.show_floating_controls {
                        "开启"
                    } else {
                        "关闭"
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
            ))
            .child(ui::setting_row(
                "录屏中隐藏应用",
                if cfg!(target_os = "linux") {
                    "X11 不支持排除应用，可选择要录制的目标窗口"
                } else {
                    "从录制画面中排除 OpenCap 窗口"
                },
                button(
                    "capture-exclusion",
                    if self.config.exclude_app {
                        "开启"
                    } else {
                        "关闭"
                    },
                    self.config.exclude_app,
                )
                .disabled(cfg!(target_os = "linux"))
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
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("录制范围边框"))
                    .child(
                        div().flex().flex_wrap().gap_2().children(
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
                                .disabled(cfg!(target_os = "linux"))
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
            .child(label(if cfg!(target_os = "linux") {
                "X11 暂不提供录制边框和窗口缩略预览；可通过悬浮控制条查看录制状态。"
            } else {
                "红框表示录制，黄框表示暂停；目标窗口最小化后隐藏，边框不录入视频。"
            }))
    }
    fn audio_settings(&self, cx: &Context<Self>) -> Div {
        ui::card().child(ui::section_title("声音", "输入处理与音频规格"))
            .child(ui::setting_row("麦克风降噪", "减少持续底噪，让发言更清晰", button(
                    "mic-denoise",
                    if self.config.mic_denoise {
                        "开启"
                    } else {
                        "关闭"
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
                })),))
            .child(ui::setting_row("实时电平检测", "录制前检查系统声音与麦克风", button(
                    "monitor-levels",
                    if self.config.monitor_levels {
                        "开启"
                    } else {
                        "关闭"
                    },
                    self.config.monitor_levels,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.config.monitor_levels = !this.config.monitor_levels;
                    this.monitor = None;
                    this.monitor_peaks = (0.0, 0.0);
                    this.save();
                    cx.notify();
                })),))
            .child(
                button(
                    "audio-quality",
                    format!(
                        "声音 {} kbps · {} 声道",
                        self.config.audio_bitrate_kbps, self.config.audio_channels
                    ),
                    false,
                )
                .disabled(self.active()).on_click(cx.listener(|this, _, _, cx| {
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
            .child(label(
                "降噪包含高通、噪声门和轻度压缩；实时电平检测只在未录制时打开所选设备，仅显示电平，不写入文件。",
            ))
    }
    fn video_settings(&self, cx: &Context<Self>) -> Div {
        ui::card()
            .child(ui::section_title("画面与编码", "画面规格在下次录制生效"))
            .child(label("编码格式"))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        (Codec::Auto, "自动"),
                        (Codec::H264, "H.264"),
                        (Codec::Hevc, "HEVC"),
                        (Codec::Av1, "AV1"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (codec, t))| {
                        button(("codec", i), t, self.config.codec == codec)
                            .disabled(self.active())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.active() {
                                    this.config.codec = codec;
                                    this.config.encoder = None;
                                    this.save();
                                }
                                cx.notify();
                            }))
                    }),
                ),
            )
            .child(label("H.264 兼容性最好；HEVC / AV1 需要播放器支持。"))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "quality-level",
                            format!("质量值 {}", self.config.quality),
                            false,
                        )
                        .disabled(self.active())
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
                        .disabled(self.active())
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
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("fps", format!("最大 {} FPS", self.config.max_fps), false)
                            .disabled(self.active())
                            .on_click(cx.listener(|this, _, _, cx| {
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
                            })),
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
                        .disabled(self.active())
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
                        .disabled(self.active())
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
    }
    fn storage_settings(&self, cx: &Context<Self>) -> Div {
        ui::card()
            .child(ui::section_title("保存与快捷键", "输出文件与操作入口"))
            .child(
                label(self.config.output_dir.to_string_lossy().into_owned())
                    .min_w(px(0.))
                    .truncate(),
            )
            .child(
                button("save-folder", "更改保存根目录", false)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
            )
            .child(label(
                "每场新会议单独建一个文件夹，集中保存录音、逐字稿、纪要和笔记。",
            ))
            .child(ui::setting_row(
                "保存独立音频",
                "同时保存同名 M4A 文件，下次录制生效",
                button(
                    "separate-audio",
                    if self.config.separate_audio {
                        "开启"
                    } else {
                        "关闭"
                    },
                    self.config.separate_audio,
                )
                .disabled(self.active())
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.active() {
                        this.config.separate_audio = !this.config.separate_audio;
                        this.save();
                        cx.notify();
                    }
                })),
            ))
            .child(label(
                "独立音频与视频同名，停止后保存；开关在下次录制生效。",
            ))
            .child(
                button("hotkeys", "切换快捷键组合", false)
                    .disabled(self.active())
                    .on_click(cx.listener(|this, _, _, cx| {
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
                    })),
            )
    }
}
