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
    } else {
        path.to_path_buf()
    }
}
impl Recorder {
    pub(super) fn transcription_busy(&self) -> bool {
        self.asr.as_ref().is_some_and(|a| !a.finished())
            || self.preparing_model.is_some()
            || self.summary_task.is_some()
            || self.ai.credentials_busy
            || self.speaker_task.is_some()
    }
    pub(super) fn start_live_asr(&mut self) -> Option<crossbeam_channel::Sender<PcmAudio>> {
        // Start a new workspace only after recording preflight has succeeded.
        // A recording without ASR must not show the preceding meeting's words.
        self.transcript = Transcript::default();
        self.transcript_output = None;
        self.reset_minutes();
        if !self.config.realtime_transcription {
            self.transcription_status = "本次未开启实时转录；录制保存后可在会议文件中补转".into();
            return None;
        }
        if !ModelKind::Streaming.ready(&model_root())
            || (self.config.transcription_context_revision
                && !ModelKind::Offline.ready(&model_root()))
        {
            self.transcription_status =
                "实时模型尚未下载；本次先保存录制，可在转录页导入文件".into();
            return None;
        }
        match AsrSession::live_with_revision(
            model_root(),
            self.config.transcription_hotwords.clone(),
            self.config.transcription_context_revision,
        ) {
            Ok(asr) => {
                let sender = asr.audio_sender();
                self.asr = Some(asr);
                self.asr_live = true;
                self.transcription_status = "正在启动实时识别…".into();
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
                    self.transcript_output =
                        Some(source.with_file_name(name.trim_end_matches(".transcript.json")));
                    self.transcript = transcript;
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
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let stem = source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .replace('.', "_");
        let output = if source.with_extension("recording.json").exists() {
            source.clone()
        } else {
            self.config
                .output_dir
                .join("Transcripts")
                .join(format!("{stem}-{stamp}"))
        };
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
                self.asr = Some(asr);
                self.asr_live = false;
                self.transcript = Transcript::default();
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
                    self.preparing_model = None;
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
                    self.transcript.apply(s);
                    self.transcript_scroll.scroll_to_bottom();
                }
                AsrEvent::Progress(p) => self.transcription_progress = p,
                AsrEvent::Gap(start, end) => {
                    self.transcription_status = format!(
                        "实时识别积压，{}–{} 将从保存文件补转",
                        opencap_asr::timestamp(start, '.'),
                        opencap_asr::timestamp(end, '.')
                    )
                }
                AsrEvent::Finished(t) => {
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
                    self.transcript = t;
                    self.transcription_progress = 1.0;
                    self.remember_meeting();
                }
                AsrEvent::Error(e) => {
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
            self.ai.progress = None;
            changed = true;
            match result {
                Ok(history) => {
                    self.summary_text = history.latest().to_string();
                    self.ai.history = history;
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
            self.ai.progress = None;
            changed = true;
            match result {
                Ok(t) => {
                    let speakers = t.speaker_turns.iter().map(|s| s.speaker).max().unwrap_or(0);
                    self.transcript = t;
                    self.remember_meeting();
                    self.transcription_status = format!(
                        "分为 {speakers} 个说话人组，请核对（自动人数可能偏多，可填写已知人数重试）；编号仅适用于本场会议，多人交替的句子会标注歧义。可重新生成纪要。"
                    );
                }
                Err(e) => {
                    self.transcription_status = format!("说话人分析未完成；已有稿件保留：{e:#}")
                }
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
    pub(super) fn transcript_lines(&self) -> Div {
        let start = if self.transcript.complete {
            0
        } else {
            self.transcript.segments.len().saturating_sub(40)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .children(self.transcript.segments[start..].iter().map(|s| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label(format!(
                        "{}  ·  {}  ·  {}",
                        self.transcript.speaker_label(s.start_ms, s.end_ms),
                        opencap_asr::timestamp(s.start_ms, '.'),
                        if s.finalized {
                            "已确认"
                        } else {
                            "识别中 · 后续语音可修正"
                        }
                    )))
                    .child(
                        div()
                            .text_base()
                            .text_color(rgb(if s.finalized { 0xeceef1 } else { 0x8cceee }))
                            .child(s.text.clone()),
                    )
            }))
            .when(self.transcript.segments.is_empty(), |d| {
                d.child(label("开启系统声音或麦克风后开始录制，或者导入已有文件。"))
            })
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
        self.speaker_task = Some(rx);
        self.ai.progress = Some(progress_rx);
        self.transcription_status = "开始会后说话人分析；首次会下载本地模型…".into();
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::distinguish_speakers(
                transcript,
                &model_root(),
                &output,
                count,
                threshold,
                &cancelled,
                |s| {
                    let _ = progress_tx.send(s);
                },
            ));
        });
    }
    pub(super) fn live_transcript_card(&self, cx: &Context<Self>) -> Div {
        div()
            .p_4()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label("实时文字"))
                    .child(
                        button("open-live-text", "查看转录", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.transcription_tab = true;
                                this.settings = false;
                                this.meeting_page = workspace::MeetingPage::Transcript;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(label(self.transcription_status.clone()))
            .child(div().max_h(px(130.)).overflow_hidden().children(
                self.transcript.segments.last().map(|s| {
                    div()
                        .text_base()
                        .text_color(rgb(if s.finalized { 0xeceef1 } else { 0x8cceee }))
                        .child(s.text.clone())
                }),
            ))
    }
    pub(super) fn transcription_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.transcription_busy();
        let ready = self.transcript.complete && !self.transcript.cancelled;
        let speakers = self
            .transcript
            .speaker_turns
            .iter()
            .map(|s| s.speaker)
            .max()
            .unwrap_or(0);
        let mut view = div().flex().flex_col().gap_4()
            .child(workspace::heading("逐字稿", "语音识别原文，不经过 AI 改写。识别中的句子会随上下文修正。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("import-transcription", "导入新的音频 / 视频", false).disabled(self.active() || busy).on_click(cx.listener(|this,_,_,cx| this.choose_transcription_file(false,cx))))
                .child(button("to-minutes", "下一步：生成 AI 纪要 →", true).disabled(!ready).on_click(cx.listener(|this,_,_,cx| { this.meeting_page=workspace::MeetingPage::Minutes; this.page_scroll.set_offset(point(px(0.),px(0.))); cx.notify(); })))
                .when(self.asr.as_ref().is_some_and(|a| !a.finished()), |d| d.child(button("cancel-asr", "取消转录", false).danger().on_click(cx.listener(|this,_,_,cx| { if let Some(asr)=&mut this.asr { asr.cancel(); } cx.notify(); })))))
            .child(div().p_3().rounded_lg().bg(rgb(SURFACE)).child(format!("{}{}",self.transcription_status,if self.transcription_progress > 0.0 && self.transcription_progress < 1.0 { format!(" · {:.0}%", self.transcription_progress*100.) } else { String::new() })))
            .child(div().p_4().rounded_lg().bg(rgb(SURFACE)).h(px(360.)).child(widgets::scrollable("transcript-scroll",self.transcript_lines(),&self.transcript_scroll)))
            .child(label(format!("{} 句；完成后显示完整逐字稿，识别中显示最近 40 句。也可打开纯文本或 Word。", self.transcript.segments.len())))
            .child(div().p_4().rounded_lg().border_1().border_color(rgb(0x313640)).flex().flex_col().gap_3()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(if speakers == 0 { "说话人：尚未分析".to_string() } else { format!("说话人：分为 {speakers} 组 · 请核对") }))
                .child(label("先做本地分析，再生成按说话人整理的纪要。自动人数可能偏多，已知人数时建议填写；编号不等同于姓名，交替或重叠发言会标注歧义。"))
                .child(div().flex().flex_wrap().gap_2().child(Input::new(&self.speaker_count).w(px(250.))).child(Input::new(&self.speaker_threshold).w(px(100.)))
                    .child(button("distinguish-speakers", "分析 / 更新说话人", false).disabled(self.active() || busy || !ready).on_click(cx.listener(|this,_,_,cx| { this.start_speakers(cx); cx.notify(); }))))
                .when(self.speaker_task.is_some(), |d| d.child(button("cancel-speakers", "取消说话人分析", false).danger().on_click(cx.listener(|this,_,_,cx| { this.speaker_cancelled.store(true,std::sync::atomic::Ordering::Relaxed); cx.notify(); })))));
        if let Some(base) = &self.transcript_output {
            let word = base.with_extension("transcript.docx");
            let plain = base.with_extension("transcript.txt");
            let srt = base.with_extension("srt");
            view = div()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            button("export-transcript-word", "导出逐字稿 Word", true)
                                .disabled(busy || !ready)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(base) = &this.transcript_output {
                                        match this.transcript.save(base) {
                                            Ok(()) => {
                                                this.transcription_status =
                                                    "逐字稿已保存，Word 保留原话、时间和说话人标签"
                                                        .into();
                                                let _ = opencap_platform::open_path(
                                                    &base.with_extension("transcript.docx"),
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
                            button("open-transcript", "打开纯文本 TXT", false)
                                .disabled(!plain.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&plain);
                                }),
                        )
                        .child(
                            button("open-subtitles", "打开字幕 SRT", false)
                                .ghost()
                                .disabled(!srt.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&srt);
                                }),
                        ),
                )
                .child(view);
        }
        view
    }
    pub(super) fn transcription_settings(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let ready = ModelKind::Streaming.ready(&model_root())
            && (!self.config.transcription_context_revision
                || ModelKind::Offline.ready(&model_root()));
        div().p_4().rounded_lg().bg(rgb(SURFACE)).flex().flex_col().gap_3()
            .child(div().text_lg().child("本地语音转录"))
            .child(label("实时转录会自动保存逐字稿，AI 纪要采用手动生成。音频与视频使用同一套转录和导出流程。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("live-asr-toggle", if self.config.realtime_transcription { "实时转录：开启" } else { "实时转录：关闭" }, self.config.realtime_transcription).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.realtime_transcription = !this.config.realtime_transcription; this.save(); cx.notify(); })))
                .child(button("context-revision", if self.config.transcription_context_revision { "上下文校正：开启" } else { "上下文校正：关闭" }, self.config.transcription_context_revision).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.transcription_context_revision = !this.config.transcription_context_revision; this.save(); cx.notify(); })))
                .child(button("download-asr", if ready { "实时模型已就绪" } else { "下载实时模型" }, false).disabled(busy || ready).on_click(cx.listener(|this,_,_,cx| { this.prepare_streaming_model(); cx.notify(); }))))
            .child(label("上下文校正每约 5 秒复核一次，关闭可降低 CPU 和内存。文件转录会在首次使用时下载模型。"))
            .child(div().flex().gap_2().child(Input::new(&self.hotwords_input).flex_1()).child(button("apply-hotwords", "保存术语", false).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.transcription_hotwords=this.hotwords_input.read(cx).value().to_string(); this.save(); cx.notify(); }))))
            .child(button("stream-file-demo", "试播文件，观察实时修正", false).ghost().disabled(busy).on_click(cx.listener(|this,_,_,cx| this.choose_transcription_file(true,cx))))
    }
}
