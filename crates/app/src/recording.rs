use super::*;

impl Recorder {
    pub(super) fn recording_view(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        let target = if let CaptureTarget::Region { rect, .. } = self.config.target {
            format!("录制区域 · {} × {}", rect.width, rect.height)
        } else {
            self.targets
                .iter()
                .find(|t| same_target(&t.target, &self.config.target))
                .map(|t| format!("{} · {} × {}", t.name, t.width, t.height))
                .unwrap_or_else(|| "选择要录制的画面".into())
        };
        let screen = ui::card()
            .flex_1()
            .child(ui::section_title(
                "录制方式",
                "选择画面录制，或只保存会议声音",
            ))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    ["整个屏幕", "应用窗口", "指定区域", "仅录音"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| {
                            button(("mode", i), text, self.mode == i)
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
                    button("target", target, false)
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .justify_start()
                        .h(px(42.))
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
                        .py_6()
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
            .when(self.choosing_target, |d| {
                d.child(self.target_picker(width - 40., cx))
            })
            .when(!self.choosing_target && !self.config.audio_only, |d| {
                d.child(self.recording_preview(cx))
            });
        let content = if ui::two_columns(width) && !self.choosing_target {
            use gpui::component::resizable::{h_resizable, resizable_panel};
            let capture = div()
                .id("recording-capture-scroll")
                .size_full()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_3()
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(screen)
                .child(self.recording_options(cx));
            let text = div()
                .id("recording-transcript-scroll")
                .size_full()
                .min_h(px(0.))
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(self.live_transcript_card(cx));
            let capture = resizable_panel()
                .size(px(width * 0.52))
                .size_range(px(300.)..px(width - 320.))
                .child(capture);
            let text = resizable_panel()
                .size(px(width * 0.48))
                .size_range(px(300.)..px(width - 320.))
                .child(text);
            let split = h_resizable("recording-workspace-split");
            let split = if self.config.meeting_media_on_right {
                split.child(text).child(capture)
            } else {
                split.child(capture).child(text)
            };
            div().h(px((height - 220.).max(420.))).w_full().child(split)
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_5()
                .child(screen)
                .when(!self.choosing_target, |d| {
                    d.child(self.recording_options(cx))
                })
                .child(self.live_transcript_card(cx))
        };
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(ui::heading(
                        "录制工作台",
                        "准备好画面和声音，专注会议本身。",
                    ))
                    .when(ui::two_columns(width) && !self.choosing_target, |d| {
                        d.child(
                            button("swap-recording-panes", "交换左右区域", false)
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.config.meeting_media_on_right =
                                        !this.config.meeting_media_on_right;
                                    this.save();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(content)
            .when(!self.recoverables.is_empty(), |d| {
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
            })
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
                                .h(px(120.))
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
        ui::card()
            .child(ui::section_title(
                if self.config.audio_only {
                    "录音声音"
                } else {
                    "声音与画质"
                },
                "录制前确认声音来源",
            ))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .child(label("声音来源"))
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
                                this.monitor_levels = (AudioRouteLevels::default(), AudioRouteLevels::default());
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
            .when(!self.config.audio_only, |d| {
                d.child(label("画质与体积"))
                    .child(
                        div().flex().flex_wrap().gap_2().children(
                            [
                                (Preset::Space, "省空间"),
                                (Preset::Balanced, "平衡"),
                                (Preset::Quality, "高质量"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(i, (p, t))| {
                                button(("preset", i), t, self.config.preset == p)
                                    .disabled(self.active())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.active() {
                                            this.config.apply_preset(p);
                                            this.save();
                                        }
                                        cx.notify();
                                    }))
                            }),
                        ),
                    )
                    .child(label("省空间适合文档和会议；画面越动态，文件越大。"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                button("reduce-recording-load", "一键降低占用", false)
                                    .disabled(self.active())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.active() {
                                            this.config.reduce_recording_load();
                                            this.save();
                                            this.message = "已降低画面采集和编码负载；预览与实时转录保持原设置。".into();
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(label(format!("当前最高 {} FPS", self.config.max_fps))),
                    )
                    .child(label("适合老电脑：最高 10 FPS、自动编码优先 H.264，并加快软件编码。预览和实时转录保持原设置；画面流畅度或质量可能降低，文件可能变大。"))
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
                        .h(px(300.))
                        .rounded_lg()
                        .bg(rgb(0x090a0c))
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
                                    .gap_3()
                                    .child(
                                        ui::icon(gpui::component::IconName::GalleryVerticalEnd)
                                            .size(px(36.))
                                            .text_color(rgb(0x8291aa)),
                                    )
                                    .child(div().text_sm().text_color(rgb(0xc7d2e4)).child(
                                        if self.active() {
                                            "等待录制画面…"
                                        } else {
                                            "画面将在开始录制后显示"
                                        },
                                    ))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x91a1ba))
                                            .child("实际捕获画面 · 与保存的视频一致"),
                                    ),
                            )
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
        let levels = if !enabled {
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
            .gap_1()
            .child(row)
            .child(
                div()
                    .h(px(4.))
                    .rounded_full()
                    .w_full()
                    .bg(rgb(BORDER))
                    .child(
                        div()
                            .h_full()
                            .w(relative(level_bar(level.rms)))
                            .bg(rgb(if level.peak > 0.95 { RED } else { 0x6cc5a2 })),
                    ),
            )
            .child(
                div()
                    .h(px(2.))
                    .rounded_full()
                    .w_full()
                    .bg(rgb(BORDER))
                    .child(
                        div()
                            .h_full()
                            .w(relative(level_bar(level.peak)))
                            .bg(rgb(if level.peak >= 0.95 { RED } else { 0xb2d8c7 })),
                    ),
            )
            .child(label(if !enabled {
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
            }));
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
