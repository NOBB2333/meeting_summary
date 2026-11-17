use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPage {
    Ai,
    Recognition,
    Templates,
    Recording,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HotkeyCapture {
    Record,
    Pause,
}

fn shortcut_from_key(key: &Keystroke) -> Option<String> {
    let m = key.modifiers;
    if m.function || !(m.control || m.alt || m.shift || m.platform) {
        return None;
    }
    let value = key.key.to_ascii_lowercase();
    let supported = value.len() == 1 && value.as_bytes()[0].is_ascii_alphanumeric()
        || value
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n));
    if !supported {
        return None;
    }
    let mut parts = Vec::new();
    if m.control {
        parts.push("Ctrl".to_string());
    }
    if m.alt {
        parts.push("Alt".to_string());
    }
    if m.shift {
        parts.push("Shift".to_string());
    }
    if m.platform {
        parts.push(
            if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Win"
            }
            .to_string(),
        );
    }
    parts.push(value.to_ascii_uppercase());
    Some(parts.join("+"))
}

impl Recorder {
    pub(super) fn restore_hotkeys(&mut self) {
        match opencap_platform::desktop_controls(
            &self.config.hotkey_record,
            &self.config.hotkey_pause,
        ) {
            Ok(controls) => self.controls = Some(controls),
            Err(error) => self.message = format!("无法恢复快捷键：{error:#}"),
        }
    }
    fn capture_hotkey(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(which) = self.hotkey_capture else {
            return;
        };
        if event.is_held {
            return;
        }
        cx.stop_propagation();
        if event.keystroke.key == "escape" {
            self.hotkey_capture = None;
            self.restore_hotkeys();
            self.message = "已取消修改快捷键".into();
            cx.notify();
            return;
        }
        let Some(shortcut) = shortcut_from_key(&event.keystroke) else {
            self.message = "请按修饰键加字母、数字或 F1–F12；Esc 取消。".into();
            cx.notify();
            return;
        };
        let (record, pause) = match which {
            HotkeyCapture::Record => (shortcut.clone(), self.config.hotkey_pause.clone()),
            HotkeyCapture::Pause => (self.config.hotkey_record.clone(), shortcut.clone()),
        };
        if record == pause {
            self.message = "录制和暂停快捷键不能相同，请重新按键。".into();
            cx.notify();
            return;
        }
        self.hotkey_capture = None;
        match opencap_platform::desktop_controls_strict(&record, &pause) {
            Ok(controls) => {
                self.controls = Some(controls);
                self.config.hotkey_record = record;
                self.config.hotkey_pause = pause;
                self.message = format!("快捷键已设置为 {shortcut}");
                self.save();
            }
            Err(error) => {
                self.restore_hotkeys();
                self.message = format!("快捷键未设置，已保留原组合：{error:#}");
            }
        }
        cx.notify();
    }
    pub(super) fn settings_view(&self, _width: f32, cx: &Context<Self>) -> Div {
        let pages = [
            (SettingsPage::Ai, "AI 服务", "协议、地址、模型和密钥"),
            (
                SettingsPage::Recognition,
                "语音识别与词表",
                "语言、模型矩阵与全局专业词表",
            ),
            (
                SettingsPage::Templates,
                "纪要模板",
                "默认风格、格式和参考文件",
            ),
            (
                SettingsPage::Recording,
                "录制偏好",
                "画面、声音、存储和快捷键",
            ),
        ];
        let nav = div().flex().flex_wrap().items_center().gap_2().children(
            pages.into_iter().enumerate().map(|(i, (page, title, _))| {
                button(("settings-page", i), title, self.settings_page == page)
                    .ghost()
                    .when(self.settings_page == page, |b| {
                        b.bg(rgb(SOFT)).text_color(rgb(ACCENT))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_page = page;
                        this.page_scroll.set_offset(point(px(0.), px(0.)));
                        cx.notify();
                    }))
            }),
        );
        let content = match self.settings_page {
            SettingsPage::Ai => self.ai_settings_view(cx),
            SettingsPage::Recognition => self.transcription_settings(cx),
            SettingsPage::Templates => self.template_settings_view(cx),
            SettingsPage::Recording => self.recording_settings(cx),
        };
        div()
            .flex()
            .flex_col()
            .gap_5()
            .when(
                self.transcription_tab && self.transcript_output.is_some(),
                |d| {
                    d.child(
                        div().flex().child(
                            button("settings-back-meeting", "返回当前会议", false)
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings = false;
                                    this.page_scroll.set_offset(point(px(0.), px(0.)));
                                    cx.notify();
                                })),
                        ),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(nav)
                    .child(div().flex_1().min_w(px(0.)).max_w(px(1100.)).child(content)),
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
                "录制画面预览",
                "选择屏幕或窗口后显示画面，可随时在录制工作台收起",
                button("settings-recording-preview", if self.config.show_recording_preview { "开启" } else { "关闭" }, self.config.show_recording_preview)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.show_recording_preview = !this.config.show_recording_preview;
                        if !this.config.show_recording_preview && let Some(image) = this.preview_image.take() {
                            cx.drop_image(image, None);
                        }
                        this.save();
                        cx.notify();
                    })),
            ))
            .child(ui::setting_row(
                "减少界面动画",
                "减少弹层及其他界面动效，不影响录制和回放",
                button("reduce-motion", if self.config.reduce_motion { "开启" } else { "关闭" }, self.config.reduce_motion)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.reduce_motion = !this.config.reduce_motion;
                        cx.set_reduce_motion(this.config.reduce_motion);
                        this.save();
                        cx.notify();
                    })),
            ))
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
                } else if cfg!(target_os = "macos") {
                    "本应用的显示器采集可过滤自身；窗口采集和其他录屏软件的行为可能不同，此设置不排除声音"
                } else {
                    "请求系统排除 OpenCap 自身窗口；其他捕获方式可能不遵循，此设置不排除声音"
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
                .disabled(cfg!(target_os = "linux") && !self.config.exclude_app)
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
                                .disabled(cfg!(target_os = "linux") && mode != BorderMode::Never)
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
            .child(ui::setting_row("麦克风降噪与人声增强", "处理麦克风底噪并自动调整人声；不处理系统声音", button(
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
                    if this.stats.state == opencap_core::RecordingState::Stopping && this.active() {
                        this.message = "录制正在停止，降噪设置将用于下一次录制。".into();
                    } else if let Some(session) = &this.session {
                        session.command(RecordingCommand::SetMicDenoise(this.config.mic_denoise));
                    }
                    this.monitor = None;
                    this.save();
                    cx.notify();
                })),))
            .child(ui::setting_row("系统声音增益", "对方声音偏小时调高；请配合电平检测，过高可能失真", button(
                    "system-audio-gain", format!("{:.0}%", self.config.system_audio_gain * 100.), false)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.config.system_audio_gain = if this.config.system_audio_gain < 1.25 { 1.5 }
                        else if this.config.system_audio_gain < 1.75 { 2.0 }
                        else if this.config.system_audio_gain < 2.5 { 3.0 } else { 1.0 };
                    if let Some(session) = &this.session {
                        session.command(RecordingCommand::SetSystemGain(this.config.system_audio_gain));
                    }
                    this.save();
                    cx.notify();
                })),))
            .child(label("录制音轨暂不提供回声消除；会议软件的处理未必作用于本应用采集的麦克风。建议使用耳机，并确认系统声音来源与会议输出设备一致。"))
            .child(ui::setting_row("实时电平检测", "录制前或录制中检查系统声音与麦克风", button(
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
                    if let Some(session) = &this.session {
                        session.command(RecordingCommand::SetLevelMeter(this.config.monitor_levels));
                    }
                    this.monitor = None;
                    this.monitor_levels = (AudioRouteLevels::default(), AudioRouteLevels::default());
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
            .child(label("画质与体积"))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        (Preset::Space, "省空间"),
                        (Preset::Balanced, "平衡"),
                        (Preset::Quality, "高质量"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (preset, title))| {
                        button(("preset", i), title, self.config.preset == preset)
                            .disabled(self.active())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.active() {
                                    this.config.apply_preset(preset);
                                    this.save();
                                }
                                cx.notify();
                            }))
                    }),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button("reduce-recording-load", "一键降低占用", false)
                            .disabled(self.active())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.active() {
                                    this.config.reduce_recording_load();
                                    this.save();
                                    this.message =
                                        "已降低画面采集和编码负载；预览与实时转录保持原设置。"
                                            .into();
                                }
                                cx.notify();
                            })),
                    )
                    .child(label(format!("当前最高 {} FPS", self.config.max_fps))),
            )
            .child(label(
                "降低占用会使用较低帧率并加快软件编码；画面流畅度或质量可能降低，文件可能变大。",
            ))
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
            .child(ui::setting_row(
                "自动编码优先 H.264",
                "自动选择时先尝试 H.264；明确选择 HEVC、AV1 或指定编码器时保持原选择。",
                button(
                    "prefer-h264",
                    if self.config.prefer_h264 {
                        "开启"
                    } else {
                        "关闭"
                    },
                    self.config.prefer_h264,
                )
                .disabled(self.active())
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.active() {
                        this.config.prefer_h264 = !this.config.prefer_h264;
                        this.save();
                    }
                    cx.notify();
                })),
            ))
            .when(cfg!(windows), |d| {
                d.child(ui::setting_row(
                    "Windows 异步采集（实验）",
                    "尝试减少读取显卡画面时的等待；旧驱动兼容性需实测，关闭后使用原采集方式。",
                    button(
                        "experimental-async-wgc",
                        if self.config.experimental_async_wgc {
                            "开启"
                        } else {
                            "关闭"
                        },
                        self.config.experimental_async_wgc,
                    )
                    .disabled(self.active())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.active() {
                            this.config.experimental_async_wgc =
                                !this.config.experimental_async_wgc;
                            this.save();
                        }
                        cx.notify();
                    })),
                ))
            })
            .child(ui::setting_row(
                "软件编码速度",
                "更快模式减少 H.264/H.265 软件编码负担，文件可能变大或画质降低；硬件编码不受影响。",
                button(
                    "fast-software-encoding",
                    if self.config.fast_software_encoding {
                        "更快"
                    } else {
                        "标准"
                    },
                    self.config.fast_software_encoding,
                )
                .disabled(self.active())
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.active() {
                        this.config.fast_software_encoding = !this.config.fast_software_encoding;
                        this.save();
                    }
                    cx.notify();
                })),
            ))
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
                                        10 => 15,
                                        15 => 30,
                                        30 => 60,
                                        _ => 10,
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
            .child(label(
                "降低最大 FPS 会减少实际采集次数，适合性能较低的电脑；自适应帧率主要减少编码次数。",
            ))
    }
    fn storage_settings(&self, cx: &Context<Self>) -> impl IntoElement {
        ui::card()
            .id("storage-shortcut-settings")
            .track_focus(&self.hotkey_focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.capture_hotkey(event, cx)),
            )
            .child(ui::section_title("保存与快捷键", "输出文件与操作入口"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .w_full()
                    .child(
                        label(self.config.output_dir.to_string_lossy().into_owned())
                            .flex_1()
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .child(
                        button("save-folder", "更改目录", false)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    ),
            )
            .child(label(
                "每场新会议单独建一个文件夹，集中保存录音、逐字稿、纪要和笔记。",
            ))
            .child(ui::setting_row(
                "保存独立音频",
                "视频录制结束后额外保存同名 M4A；仅录音本身即为 M4A",
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
                "字幕：录制中可查看实时文字；转录完成后自动保存同名 SRT 字幕，可在逐字稿中打开。",
            ))
            .children(
                [
                    (
                        HotkeyCapture::Record,
                        "开始 / 停止录制",
                        self.config.hotkey_record.clone(),
                    ),
                    (
                        HotkeyCapture::Pause,
                        "暂停 / 继续",
                        self.config.hotkey_pause.clone(),
                    ),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (which, title, shortcut))| {
                    let waiting = self.hotkey_capture == Some(which);
                    ui::setting_row(
                        title,
                        "点击后按下新的组合键，Esc 取消",
                        button(
                            ("hotkey-edit", index),
                            if waiting {
                                "请按组合键…".to_string()
                            } else {
                                shortcut
                            },
                            waiting,
                        )
                        .disabled(self.active())
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if !this.active() {
                                    this.controls = None;
                                    this.hotkey_capture = Some(which);
                                    this.hotkey_focus.focus(window, cx);
                                }
                                cx.notify();
                            },
                        )),
                    )
                }),
            )
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::shortcut_from_key;
    use gpui::Keystroke;
    #[test]
    fn shortcut_capture_requires_a_modifier_and_supports_common_keys() {
        assert_eq!(
            shortcut_from_key(&Keystroke::parse("ctrl-alt-f9").unwrap()).as_deref(),
            Some("Ctrl+Alt+F9")
        );
        assert_eq!(
            shortcut_from_key(&Keystroke::parse("ctrl-shift-r").unwrap()).as_deref(),
            Some("Ctrl+Shift+R")
        );
        assert!(shortcut_from_key(&Keystroke::parse("a").unwrap()).is_none());
        assert!(shortcut_from_key(&Keystroke::parse("ctrl-f13").unwrap()).is_none());
    }
}
