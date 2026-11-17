use super::*;

impl Recorder {
    pub(super) fn recording_view(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        let wide = ui::two_columns(width);
        let show_live = !self.config.recording_transcript_collapsed;
        let pane_height = (height - 150.).max(320.);
        let capture_width = if wide && show_live {
            width * 0.62
        } else {
            width
        };
        let target = if let CaptureTarget::Region { rect, .. } = self.config.target {
            format!("录制区域 · {} × {}", rect.width, rect.height)
        } else {
            self.targets
                .iter()
                .find(|t| same_target(&t.target, &self.config.target))
                .map(|t| format!("{} · {} × {}", t.name, t.width, t.height))
                .unwrap_or_else(|| match self.config.target {
                    CaptureTarget::Monitor(index) => format!("显示器 {}", index + 1),
                    _ => "选择要录制的画面".into(),
                })
        };
        let screen = ui::card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(ui::section_title(
                        "录制方式与画面",
                        "选择要录制的屏幕或窗口",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .when(wide, |d| {
                                d.child(
                                    button("jump-live-workspace", "纪要与全文", false)
                                        .ghost()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.page_scroll.set_offset(point(
                                                px(0.),
                                                px(-(pane_height + 20.)),
                                            ));
                                            cx.notify();
                                        })),
                                )
                            })
                            .when(wide && show_live, |d| {
                                d.child(
                                    ui::icon_button(
                                        "swap-recording-panes",
                                        gpui::assets::IconName::ArrowLeftRight,
                                        "交换左右区域",
                                    )
                                    .ghost()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.config.meeting_media_on_right =
                                                !this.config.meeting_media_on_right;
                                            this.save();
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .child(
                                ui::icon_button(
                                    "toggle-home-transcript",
                                    if show_live {
                                        gpui::assets::IconName::PanelRightClose
                                    } else {
                                        gpui::assets::IconName::PanelRightOpen
                                    },
                                    if show_live {
                                        "收起录制配置"
                                    } else {
                                        "展开录制配置"
                                    },
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.config.recording_transcript_collapsed =
                                            !this.config.recording_transcript_collapsed;
                                        this.save();
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div().flex().flex_wrap().gap_1().children(
                    ["整个屏幕", "应用窗口", "指定区域", "仅录音"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| {
                            button(("mode", i), text, self.mode == i)
                                .flex_1()
                                .ghost()
                                .when(self.mode == i, |b| b.bg(rgb(SOFT)).text_color(rgb(ACCENT)))
                                .disabled(self.active())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.change_mode(i);
                                    cx.notify();
                                }))
                        }),
                ),
            )
            .when(!self.config.audio_only, |d| {
                d.child(
                    Button::new("target")
                        .accessibility_label(format!("{}，切换画面", target))
                        .outline()
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .h(px(40.))
                        .bg(rgb(0xf8fafd))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(div().min_w(px(0.)).truncate().child(target))
                                .child(ui::badge("切换画面", ACCENT, SOFT)),
                        )
                        .disabled(self.active())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.choose_target();
                            cx.notify();
                        })),
                )
            })
            .when(self.config.audio_only, |d| {
                d.child(
                    div()
                        .h(px(250.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(0xf8fafd))
                        .justify_center()
                        .items_center()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().text_lg().child("只记录会议声音"))
                        .child(label("保存为 M4A，可同时录制系统声音和麦克风。"))
                        .child(label("开启实时转录后，可在工作台查看文字。"))
                        .when(cfg!(target_os = "macos"), |d| {
                            d.child(label(
                                "录系统声音仍需系统音频权限；只录麦克风无需屏幕权限。",
                            ))
                        }),
                )
            })
            .when(self.config.audio_only && self.active(), |d| {
                d.child(self.recording_timeline())
            })
            .when(self.mode == 2, |d| {
                d.child(
                    button(
                        "region",
                        if matches!(self.config.target, CaptureTarget::Region { .. }) {
                            "调整录制区域"
                        } else {
                            "框选录制区域"
                        },
                        false,
                    )
                    .disabled(self.active())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pick_region();
                        cx.notify();
                    })),
                )
            })
            .when(!self.config.audio_only, |d| {
                d.child(if self.choosing_target {
                    self.target_picker(capture_width - 40., cx)
                } else {
                    self.recording_preview(cx)
                })
            });
        let capture_content = div().w_full().flex().flex_col().gap_3().child(screen).when(
            !self.recoverables.is_empty(),
            |d| {
                d.child(
                    ui::card()
                        .child(ui::section_title(
                            "发现未完成的录制",
                            "恢复后会保存为新文件",
                        ))
                        .child(
                            button(
                                "recover",
                                format!("恢复录制（{}）", self.recoverables.len()),
                                false,
                            )
                            .disabled(self.recovering.is_some())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.recover();
                                cx.notify();
                            })),
                        ),
                )
            },
        );
        let first = if wide {
            let capture = widgets::pane_scrollable(
                "recording-capture-scroll",
                capture_content,
                &self.recording_capture_scroll,
            );
            if show_live {
                use gpui::component::resizable::resizable_panel;
                let capture = resizable_panel()
                    .size(px(width * 0.62))
                    .size_range(px(320.)..px(width - 300.))
                    .when(self.config.meeting_media_on_right, |p| p.pl_2())
                    .when(!self.config.meeting_media_on_right, |p| p.pr_2())
                    .child(capture);
                let text = resizable_panel()
                    .size(px(width * 0.38))
                    .size_range(px(280.)..px(width - 320.))
                    .when(self.config.meeting_media_on_right, |p| p.pr_2())
                    .when(!self.config.meeting_media_on_right, |p| p.pl_2())
                    .child(widgets::pane_scrollable(
                        "recording-config-scroll",
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(self.recording_options(cx))
                            .child(self.recording_models_summary(cx)),
                        &self.recording_config_scroll,
                    ));
                let split = ui::spaced_split("recording-workspace-v3");
                let split = if self.config.meeting_media_on_right {
                    split.child(text).child(capture)
                } else {
                    split.child(capture).child(text)
                };
                div().w_full().h(px(pane_height)).child(split)
            } else {
                div().w_full().h(px(pane_height)).child(capture)
            }
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_4()
                .child(capture_content)
                .when(show_live, |d| {
                    d.child(self.recording_options(cx))
                        .child(self.recording_models_summary(cx))
                })
        };
        let notes = widgets::pane_scrollable(
            "home-live-notes-scroll",
            self.live_minutes_overview(cx),
            &self.live_minutes_scroll,
        );
        let details = if wide {
            div()
                .w_full()
                .h(px((pane_height - 48.).max(280.)))
                .flex()
                .gap_4()
                .child(div().flex_1().min_w(px(0.)).h_full().child(notes))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .child(self.home_transcript_detail_card()),
                )
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_4()
                .child(div().h(px(400.)).child(notes))
                .child(div().h(px(440.)).child(self.home_transcript_detail_card()))
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(first)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "实时纪要与全文",
                        "录制过程中可向上查看画面",
                    ))
                    .child(
                        button("back-to-recording-preview", "返回画面", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    ),
            )
            .child(details)
    }
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
                        .border_color(rgb(if selected { ACCENT } else { BORDER }))
                        .bg(rgb(SURFACE))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(SOFT)))
                        .child(
                            div()
                                .h(px(
                                    ((width / columns as f32 - 28.) * 9. / 16.).clamp(140., 260.)
                                ))
                                .w_full()
                                .overflow_hidden()
                                .bg(rgb(0x090a0c))
                                .text_color(rgb(0xffffff))
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
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
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
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(button("expand-targets", "返回预览", false).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.close_targets();
                                    cx.notify();
                                }),
                            ))
                            .child(button("refresh-targets", "刷新窗口", false).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.refresh_targets();
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .h(px(280.))
                    .flex_shrink_0()
                    .child(widgets::contained_scrollable(
                        "target-list",
                        content,
                        &self.target_scroll,
                    )),
            )
    }
    fn recording_options(&self, cx: &Context<Self>) -> Div {
        ui::card()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .child(ui::section_title("声音来源", "录制前后均可检查电平"))
                    .child(
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
                            if let Some(session) = &this.session {
                                session.command(RecordingCommand::SetLevelMeter(
                                    this.config.monitor_levels,
                                ));
                            }
                            this.monitor = None;
                            this.monitor_levels =
                                (AudioRouteLevels::default(), AudioRouteLevels::default());
                            this.save();
                            cx.notify();
                        })),
                    ),
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
            .when(self.config.audio_only, |d| {
                d.child(label("无需采集或编码画面，适合长时间会议录音。"))
            })
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
                        "录制前预览 · 当前选择的画面"
                    } else {
                        "录制前预览"
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
                        .aspect_ratio(widgets::image_aspect(self.preview_image.as_ref()))
                        .min_h(px(150.))
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(if self.preview_image.is_some() {
                            0x090a0c
                        } else {
                            0xeff3fb
                        }))
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when_some(self.preview_image.clone(), |d, image| {
                            d.child(img(image).size_full().object_fit(ObjectFit::Contain))
                        })
                        .when(self.preview_image.is_none(), |d| {
                            d.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        ui::icon(gpui::assets::IconName::Monitor)
                                            .size(px(26.))
                                            .text_color(rgb(MUTED)),
                                    )
                                    .child(label(if self.idle_preview_error.is_some() {
                                        "无法获取预览画面，请检查屏幕录制权限和所选目标。"
                                    } else if self.active() {
                                        "等待录制画面…"
                                    } else {
                                        "正在获取所选画面…"
                                    }))
                                    .when_some(self.idle_preview_error.clone(), |d, error| {
                                        d.child(label(error)).child(
                                            button("retry-idle-preview", "重新尝试", false)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.idle_preview.take();
                                                    this.idle_preview_error = None;
                                                    cx.notify();
                                                })),
                                        )
                                    }),
                            )
                        }),
                )
            })
            .when(self.active(), |d| d.child(self.recording_timeline()))
    }
    fn recording_timeline(&self) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .pt_1()
            .child(ui::badge(
                if self.stats.state == RecordingState::Paused {
                    "已暂停"
                } else {
                    "LIVE"
                },
                RED,
                0xfdecee,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .child(div().flex_1().h(px(3.)).rounded_full().bg(rgb(ACCENT)))
                    .child(div().size(px(8.)).rounded_full().bg(rgb(RED))),
            )
            .child(
                div()
                    .font_family(MONO_FONT)
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(time_text(self.stats.duration_seconds)),
            )
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
            .flex_col()
            .items_start()
            .gap_1()
            .child(
                button(
                    ("audio", index),
                    format!("{}：{}", name, if enabled { "开" } else { "关" }),
                    enabled,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_audio(kind);
                    cx.notify();
                })),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
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
                    )
                    .when(enabled && self.active(), |d| {
                        let status = self
                            .stats
                            .audio_routes
                            .iter()
                            .find(|route| route.kind == kind);
                        let detail = match status {
                            Some(route) => match route.health {
                                AudioRouteHealth::Starting => "正在确认实际设备…".to_owned(),
                                AudioRouteHealth::Waiting => format!(
                                    "等待恢复 · {}",
                                    route.reason.as_deref().unwrap_or("设备不可用")
                                ),
                                AudioRouteHealth::Open => {
                                    let actual = route
                                        .resolved_device_name
                                        .as_deref()
                                        .or(route.resolved_device_id.as_deref())
                                        .unwrap_or("未知设备");
                                    let sample = match route.last_sample_at_us {
                                        _ if self.stats.state == RecordingState::Stopping => {
                                            "录制正在结束"
                                        }
                                        None => "尚未收到采样",
                                        Some(_) if self.stats.state == RecordingState::Paused => {
                                            "录制已暂停"
                                        }
                                        Some(last)
                                            if self.stats.duration_seconds * 1_000_000.0
                                                - last as f64
                                                > 3_000_000.0 =>
                                        {
                                            "近期未收到采样"
                                        }
                                        Some(_) => "正在接收采样",
                                    };
                                    format!("实际：{actual} · {sample}")
                                }
                            },
                            None => "正在确认实际设备…".to_owned(),
                        };
                        d.child(div().text_xs().text_color(rgb(MUTED)).child(detail))
                    }),
            );
        // While recording the session reports levels; otherwise the optional idle
        // monitor does, so the bar means the same thing in both states.
        let levels = if !enabled || !self.config.monitor_levels {
            AudioRouteLevels::default()
        } else if self.active() {
            if kind == AudioKind::System {
                AudioRouteLevels {
                    input: AudioLevel {
                        rms: self.stats.system_input_rms,
                        peak: self.stats.system_input_peak,
                    },
                    processed: AudioLevel {
                        rms: self.stats.system_rms,
                        peak: self.stats.system_peak,
                    },
                }
            } else {
                AudioRouteLevels {
                    input: AudioLevel {
                        rms: self.stats.microphone_input_rms,
                        peak: self.stats.microphone_input_peak,
                    },
                    processed: AudioLevel {
                        rms: self.stats.microphone_rms,
                        peak: self.stats.microphone_peak,
                    },
                }
            }
        } else if self.config.monitor_levels {
            if kind == AudioKind::System {
                self.monitor_levels.0
            } else {
                self.monitor_levels.1
            }
        } else {
            AudioRouteLevels::default()
        };
        let level = levels.processed;
        let mut result = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(0xf8fafd))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(row.flex_1().min_w(px(0.)))
                    .when(self.config.monitor_levels, |d| {
                        d.child(
                            div()
                                .h(px(6.))
                                .w(px(72.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(BORDER))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(level_bar(level.rms)))
                                        .rounded_full()
                                        .bg(rgb(if level.peak > 0.95 { RED } else { ui::SUCCESS })),
                                ),
                        )
                    })
                    .when(!self.config.monitor_levels, |d| d.child(label("检测关"))),
            )
            .when(
                enabled
                    && self.config.monitor_levels
                    && ((level.peak <= 0.001 && levels.input.peak <= 0.001) || level.peak >= 0.95),
                |d| {
                    d.child(label(if !enabled {
                        "声音来源已关闭".to_string()
                    } else if !self.active() && !self.config.monitor_levels {
                        "电平检测未开启".to_string()
                    } else if level.peak <= 0.001 && levels.input.peak <= 0.001 {
                        "输入与处理后均未检测到信号".to_string()
                    } else {
                        let db = |value: f32| {
                            if value <= 0.001 {
                                "低于 −60".into()
                            } else {
                                format!("{:.0}", 20. * value.log10())
                            }
                        };
                        format!(
                            "输入 RMS {} · 处理后 RMS {} · 峰值 {} dBFS（细条）{}",
                            db(levels.input.rms),
                            db(level.rms),
                            db(level.peak),
                            if level.peak >= 0.95 {
                                " · 接近削波"
                            } else {
                                ""
                            }
                        )
                    }))
                },
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
}
