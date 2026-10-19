use super::*;
use gpui::component::input::Input;
use opencap_asr::{AsrEvent, AsrSession, ModelKind, Transcript, model_root};

pub(super) enum PrepareEvent {
    Progress(String),
    Done(anyhow::Result<PathBuf>),
}
pub(super) fn final_recording_path(path: &std::path::Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if let Some(stem) = name.strip_suffix(".partial.mp4") {
        path.with_file_name(format!("{stem}.mp4"))
    } else if let Some(stem) = name.strip_suffix(".partial.m4a") {
        path.with_file_name(format!("{stem}.m4a"))
    } else {
        path.to_path_buf()
    }
}
impl Recorder {
    pub(super) fn transcription_busy(&self) -> bool {
        self.asr.as_ref().is_some_and(|a| !a.finished())
            || self.preparing_model.is_some()
            || self.summary_task.is_some()
            || self.ai.document_task.is_some()
            || self.ai.credentials_busy
            || self.speaker_task.is_some()
            || self.transcript_sync.is_some()
            || self.voice_busy
    }
    pub(super) fn start_live_asr(&mut self) -> Option<crossbeam_channel::Sender<PcmAudio>> {
        self.text_document = None;
        // Start a new workspace only after recording preflight has succeeded.
        // A recording without ASR must not show the preceding meeting's words.
        self.transcript = Transcript::default();
        self.transcript_dirty = true;
        self.transcript_new_document = true;
        self.transcript_output = None;
        self.transcription_clock = task::TaskClock::default();
        self.speaker_clock = task::TaskClock::default();
        self.transcription_progress = 0.;
        self.reset_minutes();
        if !self.config.realtime_transcription {
            self.transcription_status = "本次未开启实时转录；录制保存后可在会议文件中补转".into();
            return None;
        }
        self.asr_models = opencap_asr::ModelAvailability::inspect(&model_root());
        let Some(plan) = self
            .asr_models
            .live_plan(self.config.transcription_context_revision)
        else {
            self.transcription_status =
                "未找到本地语音模型；请在设置下载。录制文件会正常保存".into();
            return None;
        };
        match AsrSession::live_with_plan(
            model_root(),
            self.config.transcription_hotwords.clone(),
            plan,
        ) {
            Ok(asr) => {
                let sender = asr.audio_sender();
                self.transcription_clock.start();
                self.asr = Some(asr);
                self.asr_live = true;
                self.transcription_status = format!("正在启动：{}", plan.description());
                Some(sender)
            }
            Err(e) => {
                self.transcription_status = format!("识别启动失败；录制继续：{e:#}");
                None
            }
        }
    }
    pub(super) fn prepare_streaming_model(&mut self) {
        if self.transcription_busy() || self.active() {
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let revision = self.config.transcription_context_revision;
        self.transcription_clock.start();
        self.preparing_model = Some(rx);
        self.transcription_status = "正在下载实时模型…".into();
        std::thread::spawn(move || {
            let result =
                opencap_asr::prepare(ModelKind::Streaming, &model_root(), |bytes, total| {
                    let text = total
                        .map(|n| {
                            format!(
                                "下载实时模型 {:.0}%",
                                bytes as f64 * 100.0 / n.max(1) as f64
                            )
                        })
                        .unwrap_or_else(|| format!("下载实时模型 {:.1} MB", bytes as f64 / 1e6));
                    let _ = tx.send(PrepareEvent::Progress(text));
                });
            let result = result.and_then(|path| {
                if revision {
                    opencap_asr::prepare(ModelKind::Offline, &model_root(), |bytes, total| {
                        let _ = tx.send(PrepareEvent::Progress(format!(
                            "下载校正模型 {:.0}%",
                            bytes as f64 * 100.0 / total.unwrap_or(bytes.max(1)) as f64
                        )));
                    })?;
                }
                Ok(path)
            });
            let _ = tx.send(PrepareEvent::Done(result));
        });
    }
    pub(super) fn choose_transcription_file(&mut self, realtime: bool, cx: &mut Context<Self>) {
        if self.active() || self.transcription_busy() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择音频或视频文件".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(path) = paths.first()
            {
                let path = path.clone();
                let _ = entity.update(cx, |this, cx| {
                    this.start_file_transcription(path, realtime, false);
                    cx.notify();
                });
            }
        })
        .detach();
    }
    pub(super) fn start_file_transcription(
        &mut self,
        source: PathBuf,
        realtime: bool,
        backfill: bool,
    ) {
        self.text_document = None;
        if source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".transcript.json"))
        {
            match std::fs::read(&source)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| serde_json::from_slice::<Transcript>(&bytes).map_err(Into::into))
            {
                Ok(transcript) => {
                    let name = source.file_name().unwrap().to_string_lossy();
                    self.transcript_output = Some(opencap_core::meeting_base(
                        &source.with_file_name(name.trim_end_matches(".transcript.json")),
                    ));
                    self.transcript = transcript;
                    self.transcript_dirty = true;
                    self.transcript_new_document = true;
                    self.transcription_clock = task::TaskClock::default();
                    self.speaker_clock = task::TaskClock::default();
                    self.reset_minutes();
                    self.transcription_tab = true;
                    self.settings = false;
                    self.load_minutes();
                    self.meeting_page = workspace::MeetingPage::Transcript;
                    self.remember_meeting();
                    self.transcription_status = "已载入转录、纪要版本与对话，可继续修改".into();
                }
                Err(e) => self.transcription_status = format!("无法载入转录稿：{e:#}"),
            }
            return;
        }
        let output = if source.with_extension("recording.json").exists() {
            source.clone()
        } else {
            let stem = source.file_stem().unwrap_or_default().to_string_lossy();
            match opencap_core::create_meeting_base(&self.config.output_dir, &stem) {
                Ok(base) => base,
                Err(error) => {
                    self.transcription_status = format!("无法准备会议文件夹：{error:#}");
                    return;
                }
            }
        };
        let output = opencap_core::meeting_base(&output);
        match AsrSession::file_with_revision(
            model_root(),
            if realtime {
                ModelKind::Streaming
            } else {
                ModelKind::Offline
            },
            source,
            output.clone(),
            realtime,
            self.config.transcription_hotwords.clone(),
            self.config.transcription_context_revision,
        ) {
            Ok(asr) => {
                self.transcription_clock.start();
                self.speaker_clock = task::TaskClock::default();
                self.asr = Some(asr);
                self.asr_live = false;
                self.transcript = Transcript::default();
                self.transcript_dirty = true;
                self.transcript_new_document = true;
                self.transcript_output = Some(output);
                self.reset_minutes();
                // Re-transcribing a recording must retain previous AI versions.
                self.load_minutes();
                self.transcription_progress = 0.0;
                self.transcription_status = if backfill {
                    "正在从完整录制补转，替换实时稿…"
                } else if realtime {
                    "按音频原速试播流式识别，当前句会更新…"
                } else {
                    "正在转录文件，首次使用将下载离线模型…"
                }
                .into();
                self.transcription_tab = true;
                self.settings = false;
                self.meeting_page = workspace::MeetingPage::Transcript;
            }
            Err(e) => self.transcription_status = format!("无法开始转录：{e:#}"),
        }
    }
    pub(super) fn poll_transcription(&mut self) -> bool {
        let mut changed = false;
        let events: Vec<_> = self
            .preparing_model
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            changed = true;
            match event {
                PrepareEvent::Progress(s) => self.transcription_status = s,
                PrepareEvent::Done(result) => {
                    self.asr_models = opencap_asr::ModelAvailability::inspect(&model_root());
                    self.preparing_model = None;
                    self.transcription_clock.finish();
                    self.transcription_status = match result {
                        Ok(_) => "实时模型已就绪；开启声音后开始录制即可看到文字".into(),
                        Err(e) => format!("下载失败，可重试：{e:#}"),
                    };
                }
            }
        }
        let events: Vec<_> = self
            .asr
            .as_ref()
            .map(|a| a.events.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            changed = true;
            match event {
                AsrEvent::Status(s) => self.transcription_status = s,
                AsrEvent::Segment(s) => {
                    self.transcript_dirty |= self.transcript.apply(s);
                }
                AsrEvent::Progress(p) => {
                    self.transcription_progress = p;
                    self.transcription_clock.progress(p);
                }
                AsrEvent::Gap(start, end) => {
                    self.transcription_status = format!(
                        "实时识别积压，{}–{} 将从保存文件补转",
                        opencap_asr::timestamp(start, '.'),
                        opencap_asr::timestamp(end, '.')
                    )
                }
                AsrEvent::Finished(t) => {
                    self.transcription_clock.finish();
                    if !t.cancelled && !t.gaps.is_empty() {
                        self.pending_backfill = t.source.clone().filter(|p| p.exists());
                    }
                    self.transcription_status = if t.cancelled {
                        "已取消，已确认文本与原媒体保留"
                    } else if t.complete {
                        "转录完成，已保存完整稿和字幕"
                    } else {
                        "转录尚未完整完成，原文件保留，可重新导入"
                    }
                    .into();
                    if !t.output_warnings.is_empty() {
                        self.transcription_status = format!(
                            "识别已完成；{} 个文件未同步，可关闭外部编辑器后重试",
                            t.output_warnings.len()
                        );
                    }
                    self.transcript = *t;
                    self.transcript_dirty = true;
                    self.transcription_progress = 1.0;
                    self.remember_meeting();
                }
                AsrEvent::Error(e) => {
                    self.transcription_clock.finish();
                    self.transcription_status = format!("转录错误，录制文件保留：{e}")
                }
            }
        }
        if !self.active()
            && self.asr.as_ref().is_none_or(|a| a.finished())
            && let Some(source) = self.pending_backfill.take()
        {
            self.start_file_transcription(source, false, true);
            changed = true;
        }
        if let Some(rx) = &self.ai.events {
            for event in rx.try_iter() {
                match event {
                    opencap_asr::MinutesEvent::Status(status) => self.ai.status = status,
                    opencap_asr::MinutesEvent::Delta(text) => self.ai.stream.push_str(&text),
                }
                self.ai.chat_dirty = true;
                changed = true;
            }
        }
        if let Some(result) = self
            .summary_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("AI 工作线程已退出；已有文件保留")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.summary_task = None;
            self.ai_clock.finish();
            self.ai.events = None;
            self.ai.chat_dirty = true;
            changed = true;
            match result {
                Ok(history) => {
                    self.summary_text = history.latest().to_string();
                    self.ai.history = history;
                    self.ai.stream.clear();
                    self.ai.pending_request.clear();
                    self.ai.clear_composer = true;
                    self.ai.editor_sync = true;
                    self.remember_meeting();
                    self.ai.status = "AI 结果已保存，可继续提问或修改；旧版本保留".into();
                }
                Err(e) => self.ai.status = format!("AI 处理未完成，已有稿件保留：{e:#}"),
            }
        }
        if let Some(result) = self
            .speaker_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("说话人分析线程已退出；已有文件保留")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.speaker_task = None;
            self.speaker_clock.finish();
            self.ai.progress = None;
            changed = true;
            match result {
                Ok(t) => {
                    let speakers = t.speaker_count();
                    self.transcript = t;
                    self.transcript_dirty = true;
                    self.remember_meeting();
                    self.transcription_status = if self.transcript.output_warnings.is_empty() {
                        format!(
                            "说话人分析完成 · {speakers} 组 · 已同步逐字稿 JSON、TXT、字幕和 Word。AI 纪要可在对话中要求更新。"
                        )
                    } else {
                        format!(
                            "说话人分析完成 · {speakers} 组；部分文件未同步，结果已显示，可重试同步。"
                        )
                    };
                }
                Err(e) => {
                    self.transcription_status = format!("说话人分析未完成；已有稿件保留：{e:#}")
                }
            }
        }
        if let Some(result) = self
            .transcript_sync
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(t) => Some(Ok(t)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(())),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.transcript_sync = None;
            changed = true;
            match result {
                Ok(t) => {
                    self.transcript = t;
                    self.transcript_dirty = true;
                    self.transcription_status = if self.transcript.output_warnings.is_empty() {
                        "逐字稿与说话人已同步到全部文件".into()
                    } else {
                        "部分文件仍未同步，请检查文件占用或文件夹权限".into()
                    };
                    self.remember_meeting();
                }
                Err(()) => self.transcription_status = "同步线程已退出，已有分析结果保留".into(),
            }
        }
        if let Some(rx) = &self.ai.progress {
            while let Ok(status) = rx.try_recv() {
                if self.speaker_task.is_some() {
                    self.transcription_status = status;
                } else {
                    self.ai.status = status;
                }
                changed = true;
            }
        }
        changed
    }
    pub(super) fn retry_transcript_sync(&mut self) {
        if self.active() || self.transcription_busy() {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let mut transcript = self.transcript.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.transcript_sync = Some(rx);
        self.transcription_status = "正在同步文档，无需重新识别…".into();
        std::thread::spawn(move || {
            transcript.sync_outputs(&base);
            let _ = tx.send(transcript);
        });
    }
    pub(super) fn start_speakers(&mut self, cx: &App) {
        if self.active() || self.transcription_busy() || !self.transcript.complete {
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            return;
        };
        let count = match self.speaker_count.read(cx).value().trim().parse::<u32>() {
            Ok(n) if n <= 20 => n,
            _ => {
                self.transcription_status = "人数请输入 0（自动）或 1–20".into();
                return;
            }
        };
        let transcript = self.transcript.clone();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threshold = match self
            .speaker_threshold
            .read(cx)
            .value()
            .trim()
            .parse::<f32>()
        {
            Ok(n) if n.is_finite() && (0.1..=1.0).contains(&n) => n,
            _ => {
                self.transcription_status = "阈值请输入 0.1–1.0；越大越容易合并为同一人".into();
                return;
            }
        };
        self.speaker_cancelled = cancelled.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        self.speaker_clock.start();
        let threads = self.speaker_threads;
        self.speaker_task = Some(rx);
        self.ai.progress = Some(progress_rx);
        self.transcription_status = "开始会后说话人分析；首次会下载本地模型…".into();
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::distinguish_speakers_with_threads(
                transcript,
                &model_root(),
                &output,
                count,
                threshold,
                threads,
                &cancelled,
                |s| {
                    let _ = progress_tx.send(s);
                },
            ));
        });
    }
    pub(super) fn live_transcript_card(&self, cx: &Context<Self>) -> Div {
        ui::card()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label("实时文字"))
                    .child(button("open-live-text", "查看完整逐字稿", false).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.transcription_tab = true;
                            this.settings = false;
                            this.meeting_page = workspace::MeetingPage::Transcript;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "recording-live-toggle",
                            if self.config.realtime_transcription {
                                "实时转录：开"
                            } else {
                                "实时转录：关"
                            },
                            self.config.realtime_transcription,
                        )
                        .disabled(self.active() || self.transcription_busy())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.realtime_transcription =
                                !this.config.realtime_transcription;
                            this.save();
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "recording-revision-toggle",
                            if self.config.transcription_context_revision {
                                "断句校正：开"
                            } else {
                                "断句校正：关"
                            },
                            self.config.transcription_context_revision,
                        )
                        .disabled(self.active() || self.transcription_busy())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.transcription_context_revision =
                                !this.config.transcription_context_revision;
                            this.save();
                            cx.notify();
                        })),
                    ),
            )
            .when(!self.active(), |d| {
                d.child(label(
                    self.asr_models
                        .live_plan(self.config.transcription_context_revision)
                        .map(|p| p.description())
                        .unwrap_or("未安装语音模型，请前往设置下载"),
                ))
            })
            .child(label(self.transcription_status.clone()))
            .child(
                div()
                    .h(px(
                        if !self.config.realtime_transcription
                            && self.transcript.segments.is_empty()
                        {
                            64.
                        } else if self.config.audio_only {
                            380.
                        } else {
                            320.
                        },
                    ))
                    .child(self.transcript_view.clone()),
            )
    }
    pub(super) fn transcription_view(&self, width: f32, cx: &Context<Self>) -> Div {
        if let Some(document) = &self.text_document {
            return div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    button("close-text-document", "返回逐字稿", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.text_document = None;
                            cx.notify();
                        },
                    )),
                )
                .child(div().h(px(560.)).child(document.clone()));
        }
        let busy = self.transcription_busy();
        let ready = self.transcript.complete && !self.transcript.cancelled;
        let speakers = self.transcript.speaker_count();
        let mut toolbar = div().flex().flex_wrap().items_center().gap_2();
        if !self.transcript.output_warnings.is_empty() {
            toolbar = toolbar.child(
                div()
                    .w_full()
                    .p_3()
                    .bg(rgb(SOFT))
                    .rounded_lg()
                    .child(label(self.transcript.output_warnings.join("\n")))
                    .child(
                        button("retry-transcript-sync", "重试同步文件", false)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retry_transcript_sync();
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(base) = &self.transcript_output {
            let word = base.with_extension("transcript.docx");
            let plain = base.with_extension("transcript.txt");
            let srt = base.with_extension("srt");
            toolbar = toolbar.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        primary_button("export-transcript-word", "导出逐字稿 Word…")
                            .disabled(busy || !ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(base) = &this.transcript_output {
                                    match this.transcript.export_word(base) {
                                        Ok(()) => {
                                            this.export_document(
                                                base.with_extension("transcript.docx"),
                                                cx,
                                            );
                                        }
                                        Err(e) => {
                                            this.transcription_status =
                                                format!("逐字稿导出失败：{e}")
                                        }
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        button("open-transcript-word", "打开逐字稿 Word", false)
                            .disabled(!word.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&word);
                            }),
                    )
                    .child(
                        button("open-transcript", "查看纯文本 TXT", false)
                            .disabled(!plain.exists())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_text_document(plain.clone(), window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("open-subtitles", "打开字幕 SRT", false)
                            .ghost()
                            .disabled(!srt.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&srt);
                            }),
                    ),
            );
        }

        toolbar =
            toolbar.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        primary_button("to-minutes", "生成 AI 纪要")
                            .disabled(!ready || self.active() || busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.meeting_page = workspace::MeetingPage::Minutes;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    )
                    .when(self.asr.as_ref().is_some_and(|a| !a.finished()), |d| {
                        d.child(button("cancel-asr", "取消转录", false).danger().on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(asr) = &mut this.asr {
                                    asr.cancel();
                                }
                                cx.notify();
                            }),
                        ))
                    }),
            );
        let document = ui::card()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "逐字稿",
                        "语音识别原文，保留时间与说话人标记",
                    ))
                    .child(ui::badge(
                        format!("{} 句", self.transcript.segments.len()),
                        MUTED,
                        BG,
                    )),
            )
            .child(label(format!(
                "{}{}",
                self.transcription_status,
                if self.transcription_progress > 0.0 && self.transcription_progress < 1.0 {
                    format!(" · {:.0}%", self.transcription_progress * 100.)
                } else {
                    String::new()
                }
            )))
            .when(
                self.transcription_clock.has_run() || self.transcript.processing_seconds.is_some(),
                |d| {
                    let elapsed = if self.transcription_clock.has_run() {
                        self.transcription_clock.elapsed()
                    } else {
                        task::saved_duration(self.transcript.processing_seconds)
                    };
                    d.child(label(format!(
                        "转录已用时 {}",
                        task::duration_text(elapsed)
                    )))
                },
            )
            .child(div().h(px(440.)).child(self.transcript_view.clone()));
        let speaker_panel = ui::card()
            .child(ui::section_title("说话人分析", "会后在本机完成，可选步骤"))
            .child(ui::badge(
                if speakers == 0 {
                    "尚未分析".into()
                } else {
                    format!("已识别 {speakers} 组")
                },
                ACCENT,
                SOFT,
            ))
            .child(label(
                "已知人数时填写人数；0 表示自动判断。编号不代表姓名，结果需要核对。",
            ))
            .child(label("实际参会人数 · 已知时请填写"))
            .child(Input::new(&self.speaker_count).w_full().disabled(busy))
            .child(label("自动模式合并阈值 · 人数已知时无需调整"))
            .child(Input::new(&self.speaker_threshold).w_full().disabled(busy))
            .child(label("CPU 使用方式"))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [(1, "省资源"), (2, "双线程"), (4, "四线程")]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (threads, title))| {
                            button(
                                ("speaker-threads", i),
                                title,
                                self.speaker_threads == threads,
                            )
                            .disabled(busy)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.speaker_threads = threads;
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .when(
                self.speaker_clock.has_run() || self.transcript.speaker_analysis_seconds.is_some(),
                |d| {
                    let elapsed = if self.speaker_clock.has_run() {
                        self.speaker_clock.elapsed()
                    } else {
                        task::saved_duration(self.transcript.speaker_analysis_seconds)
                    };
                    d.child(label(format!(
                        "分析已用时 {}",
                        task::duration_text(elapsed)
                    )))
                },
            )
            .child(
                button("distinguish-speakers", "分析 / 更新说话人", false)
                    .w_full()
                    .disabled(self.active() || busy || !ready)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_speakers(cx);
                        cx.notify();
                    })),
            )
            .when(self.speaker_task.is_some(), |d| {
                d.child(
                    button("cancel-speakers", "取消分析", false)
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.speaker_cancelled
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            cx.notify();
                        })),
                )
            })
            .child(label(
                "分析完成后，生成的纪要可按说话人组织。重新分析可能改变分组，请核对姓名。",
            ))
            .child(label(self.transcript.speaker_memory_status.clone()))
            .child(self.speakers_view.clone());
        div().flex().flex_col().gap_4().child(toolbar).child(
            div()
                .flex()
                .items_start()
                .gap_5()
                .when(!ui::two_columns(width), |d| d.flex_col())
                .child(document)
                .child(
                    speaker_panel.when(ui::two_columns(width), |d| d.w(px(280.)).flex_shrink_0()),
                ),
        )
    }
    pub(super) fn transcription_settings(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let streaming = self.asr_models.streaming;
        let offline = self.asr_models.offline;
        let ready = streaming && (!self.config.transcription_context_revision || offline);
        let plan = self
            .asr_models
            .live_plan(self.config.transcription_context_revision);
        ui::card()
            .child(div().text_lg().child("本地语音转录"))
            .child(label("实时转录会自动保存逐字稿，AI 纪要采用手动生成。音频与视频使用同一套转录和导出流程。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("live-asr-toggle", if self.config.realtime_transcription { "实时转录：开启" } else { "实时转录：关闭" }, self.config.realtime_transcription).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.realtime_transcription = !this.config.realtime_transcription; this.save(); cx.notify(); })))
                .child(button("context-revision", if self.config.transcription_context_revision { "断句校正：开启" } else { "断句校正：关闭" }, self.config.transcription_context_revision).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.transcription_context_revision = !this.config.transcription_context_revision; this.save(); cx.notify(); })))
                .child(button("download-asr", if ready { "流式模型已就绪" } else if !streaming { "下载流式模型 · 边说边显示" } else { "下载整句校正模型" }, false).disabled(busy || ready).on_click(cx.listener(|this,_,_,cx| { this.prepare_streaming_model(); cx.notify(); }))))
            .child(label(plan.map(|p| p.description()).unwrap_or("尚未安装语音模型")))
            .child(label(format!("流式模型：{} · 文件转录模型：{}", if streaming { "已安装" } else { "未安装" }, if offline { "已安装，可逐句实时识别" } else { "未安装" })))
            .child(label("流式模型会边说边出字并更新当前句；整句校正在独立队列运行。关闭校正可降低 CPU 和内存；校正积压或连续发言超过 30 秒时保留流式原文。仅文件模型时停顿后显示，不能逐字更新。"))
            .child(div().flex().flex_wrap().gap_2().child(Input::new(&self.hotwords_input).flex_1()).child(primary_button("apply-hotwords", "保存术语").disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.transcription_hotwords=this.hotwords_input.read(cx).value().to_string(); this.save(); cx.notify(); }))))
            .child(button("stream-file-demo", "试播文件，观察实时修正", false).ghost().disabled(busy).on_click(cx.listener(|this,_,_,cx| this.choose_transcription_file(true,cx))))
    }
}
