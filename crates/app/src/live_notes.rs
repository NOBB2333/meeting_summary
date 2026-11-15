//! Opt-in incremental minutes. One in-flight request per active meeting.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub(super) struct Controller {
    pub base: PathBuf,
    pub notes: opencap_asr::LiveNotes,
    saved_notes: opencap_asr::LiveNotes,
    cached_transcript: opencap_asr::Transcript,
    cached_at: Instant,
    cached_asr_terminal: bool,
    cached_input_done: bool,
    config: opencap_asr::AiConfig,
    key: String,
    cancelled: Arc<AtomicBool>,
    response: Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::NoteProposal>>>,
    request: Option<opencap_asr::LiveNotesRequest>,
    pending_save: Option<(opencap_asr::LiveNotes, bool)>,
    export: Option<crossbeam_channel::Receiver<anyhow::Result<PathBuf>>>,
    export_revision: Option<u64>,
    export_status: Option<String>,
    indexed_revision: u64,
    last_started: Option<Instant>,
    pub status: String,
    pub enabled: bool,
    pub finished: bool,
    failed: bool,
    paused_for_review: bool,
    rejected_replies: u8,
    source_check_blocked: bool,
    missing_terminal: bool,
    editing: Option<String>,
    page: usize,
    source_pages: std::collections::HashMap<String, usize>,
}

impl Controller {
    pub(super) fn has_unsaved_work(&self) -> bool {
        self.editing.is_some() || self.pending_save.is_some()
    }
    pub(super) fn settlement_pending(&self) -> bool {
        (self.enabled && !self.finished) || self.export.is_some()
    }
    pub(super) fn settlement_needs_attention(&self) -> bool {
        self.enabled
            && !self.finished
            && (self.failed || self.source_check_blocked || self.missing_terminal)
    }
    fn matches_transcript(
        &self,
        base: Option<&PathBuf>,
        transcript: &opencap_asr::Transcript,
    ) -> bool {
        base == Some(&self.base)
            && transcript
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == self.notes.run_id)
    }
    pub fn new(
        base: PathBuf,
        meeting_id: String,
        run_id: String,
        config: opencap_asr::AiConfig,
        key: String,
        transcript: &opencap_asr::Transcript,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Self> {
        let _meeting_activity = opencap_core::managed_meeting_activity(&base)?;
        anyhow::ensure!(
            transcript
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == run_id.as_str()
                    && run.meeting_id.as_deref() == Some(meeting_id.as_str())),
            "逐字稿与所选会议或识别运行不一致，未创建纪要"
        );
        let notes =
            opencap_asr::LiveNotes::prepare_run(&base, meeting_id, run_id, Some(cancelled))?;
        let indexed_revision = notes.revision;
        Ok(Self {
            base,
            saved_notes: notes.clone(),
            notes,
            cached_transcript: transcript.clone(),
            cached_at: Instant::now(),
            cached_asr_terminal: false,
            cached_input_done: false,
            config,
            key,
            cancelled: Arc::new(AtomicBool::new(false)),
            response: None,
            request: None,
            pending_save: None,
            export: None,
            export_revision: None,
            export_status: None,
            indexed_revision,
            last_started: None,
            status: "等待已稳定的转写内容…".into(),
            enabled: true,
            finished: false,
            failed: false,
            paused_for_review: false,
            rejected_replies: 0,
            source_check_blocked: false,
            missing_terminal: false,
            editing: None,
            page: 0,
            source_pages: std::collections::HashMap::new(),
        })
    }
    fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.enabled = false;
        self.paused_for_review = false;
        self.response = None;
        if let Some(request) = self.request.take() {
            self.notes.abandon(&request.request_id);
        }
        self.key.clear();
        self.status = "已停止自动更新；已保存内容保留。正在阻塞的网络请求可能稍后才结束。".into();
    }
    fn recover_pending(&mut self) {
        let Some((pending, _)) = &self.pending_save else {
            return;
        };
        let mut recovered = pending.clone();
        recovered.extra.remove("source_completion");
        for item in &mut recovered.items {
            item.needs_review = true;
        }
        match recovered.save_recovery(&self.base) {
            Ok(path) => {
                self.pending_save = None;
                self.stop();
                self.failed = false;
                self.status = format!(
                    "结果已另存至 {}；来源待核对，自动更新已停止。",
                    path.display()
                );
            }
            Err(error) => self.status = format!("独立保存或登记未完成；内存结果保留：{error:#}"),
        }
    }
    fn retry(&mut self, transcript: &opencap_asr::Transcript) {
        if self.missing_terminal {
            self.status =
                "原转写终态尚未核实，无法安全继续；请核对原运行，待保存结果可另存为待核对副本。"
                    .into();
            return;
        }
        self.paused_for_review = false;
        self.rejected_replies = 0;
        if let Some((mut pending, final_pass)) = self.pending_save.take() {
            // Saving can fail after an otherwise valid response. While the user
            // fixes storage, the transcript or speaker attribution may change.
            // Preserve the generated text but do not publish it as current.
            let stale = match pending.refresh_staleness(transcript) {
                // refresh_staleness reports newly marked items, not every
                // item that was already marked before a previous retry.
                Ok(newly_stale) => {
                    newly_stale || pending.items.iter().any(|item| item.needs_review)
                }
                Err(error) => {
                    self.pending_save = Some((pending, final_pass));
                    self.status = format!("无法核对待保存纪要的来源：{error:#}");
                    return;
                }
            };
            let has_uncovered = transcript.segments.iter().any(|segment| {
                segment.is_confirmed()
                    && !segment.text.trim().is_empty()
                    && segment.end_ms >= segment.start_ms
                    && pending.coverage.get(&segment.id) != Some(&segment.revision)
            });
            let completion = serde_json::json!({
                "complete": transcript.is_complete(),
                "cancelled": transcript.cancelled,
                "gaps": &transcript.gaps,
            });
            let completion_changed =
                final_pass && pending.extra.get("source_completion") != Some(&completion);
            let final_pass = final_pass && !stale && !has_uncovered && !completion_changed;
            if !final_pass {
                pending.extra.remove("source_completion");
            }
            match pending.save_if_unchanged(&self.base, &self.saved_notes) {
                Ok(()) => {
                    self.saved_notes = pending.clone();
                    self.notes = pending;
                    self.finished = final_pass;
                    self.failed = false;
                    self.status = if !self.enabled && (stale || has_uncovered || completion_changed)
                    {
                        "纪要已保存但来源仍需核对或整理；自动更新已停止。".into()
                    } else if !self.enabled {
                        "纪要正文和来源已保存；自动更新已停止。".into()
                    } else if stale || has_uncovered || completion_changed {
                        "纪要已保存；来源有变化，仍需继续核对和整理。".into()
                    } else {
                        "纪要正文和来源已保存。".into()
                    };
                }
                Err(error) => {
                    self.pending_save = Some((pending, final_pass));
                    self.status = format!("保存失败，内存中的结果保留：{error:#}");
                }
            }
        } else {
            self.failed = false;
            self.last_started = None;
            self.status = if self.enabled {
                "已安排重试，等待稳定内容。"
            } else {
                "自动更新已停止；关闭面板后可重新开启。"
            }
            .into();
        }
    }
    fn retry_background(&mut self) {
        if self.missing_terminal || (self.pending_save.is_some() && !self.cached_input_done) {
            self.status =
                "原运行或说话人尚未收尾，不能按缓存来源发布；可等待、返回原运行或另存待核对副本。"
                    .into();
            return;
        }
        let cached = self.cached_transcript.clone();
        self.retry(&cached);
    }
    fn tick(&mut self, transcript: &opencap_asr::Transcript, input_done: bool) -> bool {
        if !self.enabled {
            return false;
        }
        // Keep an in-flight response queued until the edit is saved or
        // cancelled. Applying it now could revise or remove the item whose
        // unsaved text is in the editor. A saved edit advances the document
        // revision, so apply() will reject the older response afterwards.
        if self.editing.is_some() {
            return false;
        }
        let mut changed = false;
        let reply = self.response.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(value) => Some(value),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("实时纪要线程已退出")))
            }
        });
        if let Some(reply) = reply {
            changed = true;
            self.response = None;
            if let Some(request) = self.request.take() {
                match reply {
                    Ok(proposal) => {
                        let mut updated = self.notes.clone();
                        let source_changed = updated.refresh_staleness(transcript);
                        let stale_on_refresh = !matches!(&source_changed, Ok(false));
                        let checked = source_changed
                            .and_then(|_| updated.apply(&request, transcript, proposal));
                        match checked {
                            Ok(()) => {
                                self.rejected_replies = 0;
                                match updated.save_if_unchanged(&self.base, &self.saved_notes) {
                                    Ok(()) => {
                                        self.saved_notes = updated.clone();
                                        self.notes = updated;
                                        self.finished = request.final_pass;
                                        self.status = if self.finished && !transcript.is_complete()
                                        {
                                            "部分转写的纪要已整理并保存；原转写不完整，请核对缺口。"
                                                .into()
                                        } else if self.finished
                                            && self.notes.items.iter().any(|item| item.needs_review)
                                        {
                                            "实时纪要已保存；仍有锁定条目的来源待人工核对。".into()
                                        } else if self.finished {
                                            "实时纪要已整理并保存。".into()
                                        } else {
                                            format!(
                                                "已保存 {} 条纪要 · 覆盖 {} 段稳定转写",
                                                self.notes.items.len(),
                                                self.notes.coverage.len()
                                            )
                                        };
                                        if request.changed.is_empty()
                                            && !request.final_stage
                                            && self
                                                .notes
                                                .items
                                                .iter()
                                                .any(|item| item.needs_review && !item.user_locked)
                                        {
                                            self.failed = true;
                                            self.paused_for_review = true;
                                            self.status = "部分来源仍待核对，自动更新已暂停；可编辑条目后重试。".into();
                                        }
                                    }
                                    Err(error) => {
                                        self.pending_save = Some((updated, request.final_pass));
                                        self.failed = true;
                                        self.status = format!(
                                            "已生成的正文尚未保存，结果保留在内存；请重试：{error:#}"
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                let stale = stale_on_refresh
                                    || self.notes.validate_request(&request, transcript).is_err();
                                self.notes.abandon(&request.request_id);
                                if stale {
                                    self.rejected_replies = 0;
                                    self.status =
                                        "纪要来源或正文已变化，旧回包已丢弃，将稍后重试。".into();
                                    return true;
                                }
                                self.rejected_replies = self.rejected_replies.saturating_add(1);
                                if self.rejected_replies >= 3 {
                                    self.failed = true;
                                    self.status = format!(
                                        "连续三次纪要回包未通过校验，已暂停；已有内容保留，可重试：{error:#}"
                                    );
                                } else {
                                    self.status = format!(
                                        "纪要回包未通过校验，已有内容保留，将稍后重试：{error:#}"
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        self.notes.abandon(&request.request_id);
                        self.failed = true;
                        self.status = format!("实时纪要未更新；已有内容保留，可重试：{error:#}");
                    }
                }
            }
        }
        if self.failed || self.response.is_some() {
            return changed;
        }
        let source_check = self.notes.refresh_staleness(transcript);
        if source_check.is_ok() && self.source_check_blocked {
            self.source_check_blocked = false;
            self.status = if self.finished && self.notes.items.iter().any(|item| item.needs_review)
            {
                "纪要来源检查已恢复；仍有条目待人工核对。"
            } else if self.finished {
                "纪要来源检查已恢复，已保存的整理结果保留。"
            } else {
                "纪要来源已恢复，继续检查更新。"
            }
            .into();
            changed = true;
        }
        match source_check {
            Ok(true) => {
                // Persist review markers even when all affected notes are
                // manually locked and no further model request is needed.
                self.finished = false;
                if let Err(error) = self.notes.save_if_unchanged(&self.base, &self.saved_notes) {
                    self.pending_save = Some((self.notes.clone(), false));
                    self.failed = true;
                    self.status = format!("来源已变化，待核对状态尚未保存；请重试：{error:#}");
                    return true;
                }
                self.saved_notes = self.notes.clone();
                self.status = "来源已变化，相关纪要已标记待核对。".into();
                changed = true;
            }
            Ok(false) => {}
            Err(error) => {
                self.source_check_blocked = true;
                let status =
                    format!("暂无法核对纪要来源，已有内容保留；修复转写后自动继续：{error:#}");
                if self.status != status {
                    self.status = status;
                    return true;
                }
                return changed;
            }
        }
        let pending: Vec<_> = transcript
            .segments
            .iter()
            .filter(|segment| {
                segment.is_confirmed()
                    && !segment.text.trim().is_empty()
                    && segment.end_ms >= segment.start_ms
                    && self.notes.coverage.get(&segment.id) != Some(&segment.revision)
            })
            .collect();
        if self.finished
            && !self.notes.extra.contains_key("source_completion")
            && (!self.notes.items.is_empty() || !self.notes.coverage.is_empty())
        {
            self.finished = false;
            self.status = "纪要已修改，将重新整理。".into();
            changed = true;
        }
        if self.finished {
            if pending.is_empty() {
                return changed;
            }
            self.finished = false;
            self.status = "最终整理后收到新的稳定转写，将继续更新纪要。".into();
            changed = true;
        }
        let stale = self
            .notes
            .items
            .iter()
            .any(|item| item.needs_review && !item.user_locked);
        let voiced_ms = pending.iter().fold(0u64, |sum, s| {
            sum.saturating_add(s.end_ms.saturating_sub(s.start_ms))
        });
        let first_ready = pending.len() >= 3 || voiced_ms >= 20_000;
        let interval_ready = self
            .last_started
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(30));
        let has_work = !pending.is_empty() || stale;
        let final_pass = input_done && !has_work;
        let ready = if input_done {
            self.last_started
                .is_none_or(|time| time.elapsed() >= Duration::from_secs(5))
        } else if self.last_started.is_none() {
            first_ready
        } else {
            has_work && interval_ready
        };
        if !ready {
            return changed;
        }
        if final_pass && self.notes.items.is_empty() && self.notes.coverage.is_empty() {
            self.finished = true;
            self.status = if transcript.is_complete() {
                "没有可用于纪要的稳定转写。"
            } else {
                "转写未完整完成，当前没有可用于纪要的稳定内容。"
            }
            .into();
            return true;
        }
        let request = if final_pass {
            self.notes.begin_final_stage(transcript)
        } else {
            self.notes.begin(transcript, false)
        };
        match request {
            Ok(Some(request)) => {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let config = self.config.clone();
                let key = self.key.clone();
                let cancelled = self.cancelled.clone();
                let work = request.clone();
                self.request = Some(request);
                self.response = Some(rx);
                self.last_started = Some(Instant::now());
                self.status = if final_pass {
                    "正在分批整理最终纪要…"
                } else {
                    "正在更新增量纪要…"
                }
                .into();
                if let Err(error) = std::thread::Builder::new()
                    .name("opencap-live-notes-request".into())
                    .spawn(move || {
                        let _ = tx.send(opencap_asr::request_live_notes(
                            &config, &key, &work, &cancelled,
                        ));
                    })
                {
                    self.response = None;
                    if let Some(request) = self.request.take() {
                        self.notes.abandon(&request.request_id);
                    }
                    self.failed = true;
                    self.status = format!("无法启动纪要任务，已有内容保留，可重试：{error}");
                }
                true
            }
            Ok(None) => changed,
            Err(error) => {
                self.failed = true;
                self.status = format!("暂时无法更新实时纪要：{error:#}");
                true
            }
        }
    }
}

impl Recorder {
    fn close_live_notes_panel(&mut self) {
        let Some(controller) = &self.live_notes else {
            return;
        };
        if controller.pending_save.is_some()
            || controller.editing.is_some()
            || controller.export.is_some()
            || (controller.enabled && !controller.finished)
        {
            return;
        }
        if controller.notes.revision != controller.indexed_revision {
            self.meeting_scan_extra.insert(controller.base.clone());
            self.notes_index_pending = Some(Instant::now());
        }
        self.live_notes = None;
    }
    /// Retain the old run before a new recording, import or backfill replaces
    /// the shared transcript view. The AI task never blocks the new ASR job.
    pub(super) fn stash_live_notes_before_switch(&mut self) {
        self.live_notes_prepare_cancel
            .store(true, Ordering::Release);
        let asr_terminal = self
            .asr
            .as_ref()
            .is_none_or(|asr| asr.finished() && asr.events.is_empty())
            && self.transcript.run.as_ref().is_some_and(|run| {
                matches!(
                    run.state.as_str(),
                    "completed" | "partial" | "cancelled" | "failed"
                )
            });
        #[cfg(feature = "online-speaker")]
        let terminal = if asr_terminal {
            if let (Some(speaker), Some(run)) = (&mut self.online_speakers, &self.transcript.run) {
                let recording_end = (self.stats.state == RecordingState::Finished)
                    .then_some((self.stats.duration_seconds * 1000.0) as u64);
                match speaker.finish_for_run(
                    self.transcript_output.as_ref(),
                    &run.run_id,
                    recording_end,
                ) {
                    Ok(true) => speaker.settled_for_notes(),
                    Ok(false) => true,
                    Err(error) => {
                        self.online_speaker_status =
                            format!("原运行说话人收尾身份不匹配：{error:#}");
                        true
                    }
                }
            } else {
                true
            }
        } else {
            false
        };
        #[cfg(not(feature = "online-speaker"))]
        let terminal = asr_terminal;
        if let Some(controller) = &mut self.live_notes
            && controller.matches_transcript(self.transcript_output.as_ref(), &self.transcript)
        {
            controller.cached_transcript = self.transcript.clone();
            controller.cached_asr_terminal = asr_terminal;
            controller.cached_input_done = terminal;
            controller.cached_at = Instant::now();
        }
    }
    pub(super) fn poll_live_notes(&mut self) -> bool {
        let local_asr_idle = self
            .asr
            .as_ref()
            .is_none_or(|asr| asr.finished() && asr.events.is_empty());
        let capture_inactive = !self.active();
        let asr_terminal = local_asr_idle
            && (!self.asr_live || capture_inactive)
            && self.transcript.run.as_ref().is_some_and(|run| {
                matches!(
                    run.state.as_str(),
                    "completed" | "partial" | "cancelled" | "failed"
                )
            });
        #[cfg(feature = "online-speaker")]
        let input_done = asr_terminal
            && self.online_speakers.as_ref().is_none_or(|speaker| {
                speaker.settled_for_notes()
                    || self.transcript.run.as_ref().is_none_or(|run| {
                        !speaker.may_belong_to(self.transcript_output.as_ref(), &run.run_id)
                    })
            });
        #[cfg(not(feature = "online-speaker"))]
        let input_done = asr_terminal;
        let Some(controller) = &mut self.live_notes else {
            return false;
        };
        let export_result = controller
            .export
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("导出线程已退出")))
                }
            });
        let export_changed = export_result.is_some();
        if let Some(result) = export_result {
            controller.export = None;
            let export_revision = controller.export_revision.take();
            controller.export_status = Some(match result {
                Ok(path) => {
                    let mut checked = controller.notes.clone();
                    let source_uncertain = export_revision != Some(controller.notes.revision)
                        || controller.editing.is_some()
                        || controller.pending_save.is_some()
                        || !controller
                            .matches_transcript(self.transcript_output.as_ref(), &self.transcript)
                        || !matches!(checked.refresh_staleness(&self.transcript), Ok(false));
                    if source_uncertain {
                        format!(
                            "已导出至 {}；纪要正在编辑、来源已变化或无法核对，请检查后重新导出。",
                            path.display()
                        )
                    } else {
                        format!("已导出至 {}", path.display())
                    }
                }
                Err(error) => format!("导出失败，已保存的纪要保留，可重试：{error:#}"),
            });
        }
        let changed = if controller
            .matches_transcript(self.transcript_output.as_ref(), &self.transcript)
        {
            let no_local_terminal = !asr_terminal
                && capture_inactive
                && local_asr_idle
                && self
                    .transcript
                    .run
                    .as_ref()
                    .is_some_and(|run| run.state == "running");
            let attention_changed = controller.missing_terminal != no_local_terminal;
            controller.missing_terminal = no_local_terminal;
            if no_local_terminal && attention_changed {
                controller.status = "当前检查点未记录转写终态，本进程没有可继续的识别任务；请核对原稿或停止自动更新。".into();
            } else if attention_changed {
                controller.status = "已重新连接原运行，继续核对纪要来源。".into();
            }
            if (asr_terminal
                && (!controller.cached_asr_terminal
                    || controller.cached_input_done != input_done
                    || self.transcript_dirty))
                || (!asr_terminal
                    && self.transcript_dirty
                    && controller.cached_at.elapsed() >= Duration::from_secs(1))
            {
                controller.cached_transcript = self.transcript.clone();
                controller.cached_at = Instant::now();
                controller.cached_asr_terminal = asr_terminal;
                controller.cached_input_done = input_done;
            }
            if no_local_terminal {
                attention_changed
            } else {
                controller.tick(&self.transcript, input_done) || attention_changed
            }
        } else {
            let mut terminal_attention_changed = false;
            if !controller.cached_asr_terminal
                && controller.enabled
                && !controller.finished
                && !controller.missing_terminal
            {
                // The shared view no longer carries this run, and no terminal
                // transcript was observed before switching. Do not assume the
                // old ASR completed or continue waiting without explanation.
                controller.missing_terminal = true;
                controller.status = "原运行已离开当前视图且未记录转写终态；请返回原运行核对，或停止自动更新以保留已保存纪要。".into();
                terminal_attention_changed = true;
            }
            if !controller.cached_input_done && controller.cached_asr_terminal {
                #[cfg(feature = "online-speaker")]
                let waiting = if let Some(speaker) = &self.online_speakers
                    && let Some(run) = &controller.cached_transcript.run
                    && speaker.belongs_to(Some(&controller.base), &run.run_id)
                {
                    if speaker.settled_for_notes() {
                        if speaker.registry.is_some()
                            && let Err(error) = speaker.apply(&mut controller.cached_transcript)
                        {
                            controller.status =
                                format!("原运行说话人结果未合入，保留已有纪要：{error:#}");
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    controller.status =
                        "原运行说话人任务不可用；继续使用已保留的来源整理纪要。".into();
                    false
                };
                #[cfg(not(feature = "online-speaker"))]
                let waiting = false;
                if !waiting {
                    controller.cached_input_done = true;
                }
            }
            if !controller.cached_input_done {
                terminal_attention_changed
            } else {
                let cached = std::mem::take(&mut controller.cached_transcript);
                let changed = controller.tick(&cached, true);
                controller.cached_transcript = cached;
                changed || terminal_attention_changed
            }
        };
        if controller.pending_save.is_none()
            && controller.notes.revision != controller.indexed_revision
        {
            controller.indexed_revision = controller.notes.revision;
            // Reuse the existing debounced background index refresh. The
            // meeting files, rather than the UI state, remain authoritative.
            self.meeting_scan_extra.insert(controller.base.clone());
            self.notes_index_pending = Some(Instant::now());
        }
        changed || export_changed
    }
    pub(super) fn live_notes_panel(&self, cx: &Context<Self>) -> Div {
        let can_start = self.transcript_output.is_some()
            && self.transcript.run.is_some()
            && self.asr.as_ref().is_some_and(|asr| !asr.finished())
            && !self.live_notes_credentials_busy
            && !self.live_notes_preparing;
        let mut view = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(label("增量纪要（开启后向当前 AI 服务发送稳定转写）"));
        if let Some(base) = &self.transcript_output
            && let Some(folder) = base.parent().map(|path| path.to_path_buf())
        {
            let prefix = format!(
                "{}.",
                base.file_name().unwrap_or_default().to_string_lossy()
            );
            let archives = opencap_core::meeting_manifest(base)
                .ok()
                .flatten()
                .map(|manifest| {
                    manifest
                        .assets
                        .into_iter()
                        .filter(|asset| {
                            asset.ownership == opencap_core::AssetOwnership::Managed
                                && asset
                                    .path
                                    .components()
                                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                                && asset
                                    .path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .and_then(|name| name.strip_prefix(&prefix))
                                    .is_some_and(|suffix| {
                                        suffix.ends_with(".json")
                                            && ((suffix.starts_with("summary.live.archive-")
                                                && (asset.role == "live_minutes_archive"
                                                    || asset.role == suffix))
                                                || (suffix.starts_with("summary.live.v-")
                                                    && (asset.role == "live_minutes"
                                                        || asset.role == suffix)))
                                    })
                        })
                        .map(|asset| {
                            let kind = if asset.role == "live_minutes_archive"
                                || asset
                                    .path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .is_some_and(|name| name.contains(".summary.live.archive-"))
                            {
                                "归档"
                            } else {
                                "快照"
                            };
                            (asset.asset_id, kind)
                        })
                        .rev()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let archived = archives.len();
            let archive_label = if archived > 0 {
                format!("打开纪要历史文件夹（已登记 {archived}）")
            } else {
                "打开会议文件夹".to_string()
            };
            view = view.child(
                button("open-live-archives", archive_label, false)
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Err(error) = opencap_platform::open_path(&folder) {
                            let message = format!("无法打开归档文件夹：{error:#}");
                            if let Some(controller) = &mut this.live_notes {
                                controller.status = message;
                            } else {
                                this.live_notes_status = message;
                            }
                        }
                        cx.notify();
                    })),
            );
            if archived > 0 {
                view = view.child(label("已登记的纪要历史（只读预览，最近登记优先）"));
                for (index, (asset_id, kind)) in archives.into_iter().take(10).enumerate() {
                    let base = base.clone();
                    view = view.child(
                        button(
                            ("preview-live-archive", index),
                            format!("查看{kind} {}", index + 1),
                            false,
                        )
                        .ghost()
                        .disabled(self.live_archive_loading)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.live_archive_loading
                                || this.transcript_output.as_ref() != Some(&base)
                            {
                                return;
                            }
                            this.live_archive_loading = true;
                            this.live_archive_preview =
                                Some((base.clone(), "正在读取纪要归档…".into()));
                            let base = base.clone();
                            let asset_id = asset_id.clone();
                            cx.notify();
                            cx.spawn(async move |entity, cx| {
                                let task_base = base.clone();
                                let result = cx
                                    .background_executor()
                                    .spawn(async move {
                                        let notes = opencap_asr::LiveNotes::load_archive(
                                            &task_base, &asset_id,
                                        )?;
                                        let mut preview = format!(
                                            "运行：{} · 版本：{} · 共 {} 条\n",
                                            notes.run_id,
                                            notes.revision,
                                            notes.items.len()
                                        );
                                        for item in notes.items.iter().take(40) {
                                            let kind = match item.kind {
                                                opencap_asr::LiveNoteKind::Topic => "议题",
                                                opencap_asr::LiveNoteKind::Point => "要点",
                                                opencap_asr::LiveNoteKind::Decision => "决定",
                                                opencap_asr::LiveNoteKind::Action => "待办",
                                                opencap_asr::LiveNoteKind::Risk => "风险",
                                                opencap_asr::LiveNoteKind::Unresolved => "待解决",
                                            };
                                            let text: String =
                                                item.text.chars().take(200).collect();
                                            preview.push_str(&format!(
                                                "\n{}{}：{}{}\n",
                                                kind,
                                                if item.needs_review {
                                                    "（待核对）"
                                                } else {
                                                    ""
                                                },
                                                text,
                                                if item.text.chars().count() > 200 {
                                                    "…"
                                                } else {
                                                    ""
                                                }
                                            ));
                                        }
                                        if notes.items.len() > 40 {
                                            preview.push_str(
                                                "\n仅显示前 40 条；完整归档可在会议文件夹查看。\n",
                                            );
                                        }
                                        Ok::<_, anyhow::Error>(preview)
                                    })
                                    .await;
                                let _ = entity.update(cx, |this, cx| {
                                    this.live_archive_loading = false;
                                    if this.transcript_output.as_ref() == Some(&base)
                                        && this.live_archive_preview.as_ref().is_some_and(
                                            |(preview_base, status)| {
                                                preview_base == &base
                                                    && status == "正在读取纪要归档…"
                                            },
                                        )
                                    {
                                        this.live_archive_preview = Some((
                                            base,
                                            match result {
                                                Ok(preview) => preview,
                                                Err(error) => format!("无法读取归档：{error:#}"),
                                            },
                                        ));
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        })),
                    );
                }
                if archived > 10 {
                    view = view.child(label(
                        "仅列出最近登记的 10 份；其余历史可在会议文件夹查看。",
                    ));
                }
            }
            if let Some((preview_base, preview)) = &self.live_archive_preview
                && preview_base == base
            {
                view = view.child(label(preview.clone())).child(
                    button("close-live-archive-preview", "关闭归档预览", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.live_archive_preview = None;
                            cx.notify();
                        })),
                );
            }
        }
        if let Some(controller) = &self.live_notes {
            if !controller.matches_transcript(self.transcript_output.as_ref(), &self.transcript) {
                view = view
                    .child(label("另一场会议或识别版本的增量纪要仍由后台任务管理。"))
                    .child(label(controller.status.clone()));
                if let Some(status) = &controller.export_status {
                    view = view.child(label(status.clone()));
                }
                if let Some(folder) = controller.base.parent().map(|path| path.to_path_buf()) {
                    view = view.child(
                        button("open-background-live-folder", "打开原运行文件夹", false)
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(error) = opencap_platform::open_path(&folder)
                                    && let Some(controller) = &mut this.live_notes
                                {
                                    controller.status = format!("无法打开原运行文件夹：{error:#}");
                                }
                                cx.notify();
                            })),
                    );
                }
                if controller.editing.is_some() {
                    view = view.child(label("原运行有未保存的纪要编辑，请保存或取消后继续自动整理。"))
                        .child(div().flex().gap_2()
                            .child(gpui::component::input::Input::new(&self.live_note_edit_input).flex_1())
                            .child(button("save-background-live-note-edit", "保存并锁定", false)
                                .disabled(controller.pending_save.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let text = this.live_note_edit_input.read(cx).value().to_string();
                                    if let Some(controller) = &mut this.live_notes
                                        && controller.pending_save.is_none()
                                        && let Some(id) = controller.editing.clone()
                                    {
                                        let mut updated = controller.notes.clone();
                                        let source_unsettled = !controller.cached_input_done || controller.missing_terminal;
                                        let edited = updated.edit_and_lock(&id, text).and_then(|()| {
                                            if source_unsettled && let Some(item) = updated.items.iter_mut().find(|item| item.item_id == id) {
                                                item.needs_review = true;
                                            }
                                            updated.save_if_unchanged(&controller.base, &controller.saved_notes)
                                        });
                                        match edited {
                                            Ok(()) => {
                                                controller.saved_notes = updated.clone();
                                                controller.notes = updated;
                                                controller.editing = None;
                                                controller.status = if source_unsettled {
                                                    "原运行的手动修改已保存并锁定；来源尚未收尾，条目待核对。"
                                                } else { "原运行的手动修改已保存并锁定。" }.into();
                                            }
                                            Err(error) => controller.status = format!("手动修改未保存，输入保留：{error:#}"),
                                        }
                                    }
                                    cx.notify();
                                })))
                            .child(button("cancel-background-live-note-edit", "取消编辑", false)
                                .ghost().on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        controller.editing = None;
                                    }
                                    cx.notify();
                                }))));
                }
                if controller.failed {
                    view = view.child(
                        button("retry-background-live-notes", "重试原运行纪要", false).on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(controller) = &mut this.live_notes {
                                    controller.retry_background();
                                }
                                cx.notify();
                            }),
                        ),
                    );
                }
                if controller.pending_save.is_some() {
                    view = view.child(
                        button("recover-background-live-notes", "另存待保存结果", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(controller) = &mut this.live_notes {
                                    controller.recover_pending();
                                }
                                cx.notify();
                            })),
                    );
                }
                if controller.enabled && !controller.finished {
                    view = view.child(
                        button("stop-background-live-notes", "停止原运行增量更新", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(controller) = &mut this.live_notes {
                                    controller.stop();
                                }
                                cx.notify();
                            })),
                    );
                }
                if (!controller.enabled || controller.finished)
                    && controller.pending_save.is_none()
                    && controller.editing.is_none()
                    && controller.export.is_none()
                {
                    view = view.child(
                        button("close-background-live-notes", "关闭原运行纪要面板", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_live_notes_panel();
                                cx.notify();
                            })),
                    );
                }
                return view;
            }
            view = view.child(label(controller.status.clone()));
            if let Some(status) = &controller.export_status {
                view = view.child(label(status.clone()));
            }
            view = view.child(div().flex().gap_2()
                .children([(false, "export-live-markdown", "导出 Markdown"), (true, "export-live-word", "导出 Word")]
                    .into_iter().map(|(word, id, title)| {
                        let revision = controller.notes.revision;
                        button(id, title, false)
                            .ghost().disabled(controller.pending_save.is_some() || controller.editing.is_some() || controller.export.is_some())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(controller) = &mut this.live_notes {
                                    if controller.pending_save.is_some() || controller.editing.is_some()
                                        || controller.export.is_some() || controller.notes.revision != revision
                                        || !controller.matches_transcript(this.transcript_output.as_ref(), &this.transcript) {
                                        controller.export_status = Some("纪要正在编辑、导出或已变化，请重新查看后导出。".into());
                                    } else {
                                        let notes = controller.notes.clone();
                                        let transcript = this.transcript.clone();
                                        let base = controller.base.clone();
                                        let (tx, rx) = crossbeam_channel::bounded(1);
                                        controller.export = Some(rx);
                                        controller.export_revision = Some(revision);
                                        controller.export_status = Some("正在后台导出已保存的纪要…".into());
                                        if let Err(error) = std::thread::Builder::new().name("opencap-live-notes-export".into()).spawn(move || {
                                            let result = (|| -> anyhow::Result<PathBuf> {
                                                let mut checked = notes.clone();
                                                checked.refresh_staleness(&transcript)?;
                                                anyhow::ensure!(serde_json::to_value(&checked)? == serde_json::to_value(&notes)?,
                                                    "来源已有变化，请等待纪要更新或重新核对后再导出");
                                                notes.export_saved(&base, word)
                                            })();
                                            let _ = tx.send(result);
                                        }) {
                                            controller.export = None;
                                            controller.export_revision = None;
                                            controller.export_status = Some(format!("无法启动导出线程，已保存的纪要保留，可重试：{error}"));
                                        }
                                    }
                                }
                                cx.notify();
                            }))
                    })));
            if let Some(removed) = controller
                .notes
                .extra
                .get("manual_removed_items")
                .and_then(|value| value.as_array())
                .and_then(|items| items.last())
            {
                let preview = removed
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("无法读取正文")
                    .chars()
                    .take(60)
                    .collect::<String>();
                let rendered_revision = controller.notes.revision;
                view = view.child(
                    button(
                        "restore-live-note",
                        format!("恢复最近删除：{preview}"),
                        false,
                    )
                    .ghost()
                    .disabled(controller.pending_save.is_some() || controller.editing.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(controller) = &mut this.live_notes {
                            if controller.pending_save.is_some() || controller.editing.is_some() {
                                return;
                            }
                            if controller.notes.revision != rendered_revision {
                                controller.status = "纪要已变化，请重新查看删除记录后恢复。".into();
                                cx.notify();
                                return;
                            }
                            let mut updated = controller.notes.clone();
                            match updated.restore_last_removed().and_then(|()| {
                                updated.save_if_unchanged(&controller.base, &controller.saved_notes)
                            }) {
                                Ok(()) => {
                                    controller.saved_notes = updated.clone();
                                    controller.notes = updated;
                                    controller.page = 0;
                                    controller.finished = false;
                                    controller.status = "已恢复并锁定条目，请核对来源。".into();
                                }
                                Err(error) => controller.status = format!("条目未恢复：{error:#}"),
                            }
                        }
                        cx.notify();
                    })),
                );
            }
            let page_count = controller.notes.items.len().max(1).div_ceil(6);
            let page = controller.page.min(page_count - 1);
            for (index, item) in controller
                .notes
                .items
                .iter()
                .rev()
                .skip(page * 6)
                .take(6)
                .rev()
                .enumerate()
            {
                let timestamp = item
                    .evidence
                    .first()
                    .map(|e| {
                        if e.end_ms > e.start_ms {
                            opencap_asr::timestamp(e.start_ms, '.')
                        } else {
                            "时间不可用".into()
                        }
                    })
                    .unwrap_or_default();
                let id = item.item_id.clone();
                let unlock_id = id.clone();
                let remove_id = id.clone();
                let confirm_id = id.clone();
                let reviewed_notes_revision = controller.notes.revision;
                let reviewed_sources: Vec<_> = item
                    .evidence
                    .iter()
                    .map(|evidence| {
                        let mut matches = self
                            .transcript
                            .segments
                            .iter()
                            .filter(|segment| segment.id == evidence.segment_id);
                        matches
                            .next()
                            .filter(|_| matches.next().is_none())
                            .map(|segment| {
                                (
                                    segment.id,
                                    segment.revision,
                                    segment.start_ms,
                                    segment.end_ms,
                                    segment.text.clone(),
                                )
                            })
                    })
                    .collect();
                let reviewed_speakers = serde_json::to_vec(&(
                    &self.transcript.speaker_turns,
                    &self.transcript.speaker_names,
                ))
                .ok();
                let all_sources_stable = !item.evidence.is_empty()
                    && item.evidence.iter().all(|evidence| {
                        let mut matches = self
                            .transcript
                            .segments
                            .iter()
                            .filter(|segment| segment.id == evidence.segment_id);
                        matches.next().is_some_and(|segment| {
                            segment.is_confirmed()
                                && !segment.text.trim().is_empty()
                                && segment.end_ms >= segment.start_ms
                        }) && matches.next().is_none()
                    });
                let merge_source = id.clone();
                let undo_merge_id = id.clone();
                let can_undo_merge = item
                    .extra
                    .get("manual_merged_items")
                    .and_then(|value| value.as_array())
                    .and_then(|history| history.last())
                    .and_then(|record| record.get("merged_sha256"))
                    .and_then(|value| value.as_str())
                    .is_some();
                let merge_target = controller
                    .notes
                    .items
                    .iter()
                    .position(|candidate| candidate.item_id == item.item_id)
                    .and_then(|position| position.checked_sub(1))
                    .and_then(|position| controller.notes.items.get(position))
                    .map(|previous| (previous.item_id.clone(), previous.text.clone()));
                let text = item.text.clone();
                #[cfg(feature = "internal-player")]
                let play_item_id = item.item_id.clone();
                let segment = item.evidence.first().map(|e| e.segment_id);
                let source_item_id = item.item_id.clone();
                let displayed_evidence = item
                    .evidence
                    .first()
                    .and_then(|evidence| serde_json::to_value(evidence).ok());
                #[cfg(feature = "internal-player")]
                let playback_evidence = displayed_evidence.clone();
                #[cfg(feature = "internal-player")]
                let audio_start = item
                    .evidence
                    .first()
                    .filter(|e| e.end_ms > e.start_ms)
                    .map(|e| e.start_ms);
                view = view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            label(format!(
                                "{}{}",
                                if item.needs_review {
                                    "待核对 · "
                                } else {
                                    ""
                                },
                                item.text
                            ))
                            .flex_1(),
                        )
                        .when_some(segment, |d, segment| {
                            d.child(
                                button(
                                    ("live-note-source", index),
                                    format!("来源 {timestamp}"),
                                    false,
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let current = this.live_notes.as_ref().and_then(|controller| {
                                            if !controller.matches_transcript(this.transcript_output.as_ref(), &this.transcript) { return None; }
                                            let mut checked = controller.notes.clone();
                                            checked.refresh_staleness(&this.transcript).ok()?;
                                            let item = checked.items.iter().find(|item| item.item_id == source_item_id)?;
                                            let evidence = item.evidence.first()?;
                                            let shown = displayed_evidence.as_ref()?;
                                            let current = serde_json::to_value(evidence).ok()?;
                                            if evidence.segment_id != segment
                                                || shown != &current
                                            { return None; }
                                            Some(item.needs_review)
                                        });
                                        let Some(stale) = current else {
                                            this.transcription_status = "纪要来源不可用或当前稿件已切换。".into();
                                            cx.notify();
                                            return;
                                        };
                                        if this.transcript.segments.iter().filter(|row| row.id == segment).take(2).count() != 1 {
                                            this.transcription_status = "纪要来源句段不存在或编号不唯一，请先核对。".into();
                                            cx.notify();
                                            return;
                                        }
                                        if stale { this.transcription_status = "纪要来源已变化，已打开当前句段供核对。".into(); }
                                        this.transcription_tab = true;
                                        this.meeting_page = workspace::MeetingPage::Transcript;
                                        this.transcript_view.update(cx, |view, cx| {
                                            view.reveal_segment(segment, cx)
                                        });
                                        cx.notify();
                                    },
                                )),
                            )
                        })
                        .when(item.needs_review, |d| {
                            d.child(button(("confirm-live-note-sources", index), "确认当前来源", false)
                                .ghost()
                                .disabled(controller.pending_save.is_some() || controller.editing.is_some() || !all_sources_stable)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        if controller.pending_save.is_some() || controller.editing.is_some() { return; }
                                        let same_sources = reviewed_sources.iter().all(|source| source.as_ref().is_some_and(|(id, revision, start, end, text)| {
                                            let mut matches = this.transcript.segments.iter().filter(|segment| segment.id == *id);
                                            matches.next().is_some_and(|segment| segment.revision == *revision
                                                && segment.start_ms == *start && segment.end_ms == *end && segment.text == *text)
                                                && matches.next().is_none()
                                        }));
                                        let same_speakers = reviewed_speakers.as_ref().is_some_and(|before| {
                                            serde_json::to_vec(&(&this.transcript.speaker_turns, &this.transcript.speaker_names)).is_ok_and(|current| &current == before)
                                        });
                                        if controller.notes.revision != reviewed_notes_revision
                                            || !controller.matches_transcript(this.transcript_output.as_ref(), &this.transcript)
                                            || !same_sources || !same_speakers {
                                            controller.status = "纪要、来源或说话人已变化，请重新核对后确认。".into();
                                            cx.notify();
                                            return;
                                        }
                                        let mut updated = controller.notes.clone();
                                        match updated.confirm_sources(&confirm_id, &this.transcript)
                                            .and_then(|()| updated.save_if_unchanged(&controller.base, &controller.saved_notes)) {
                                            Ok(()) => {
                                                controller.saved_notes = updated.clone();
                                                controller.notes = updated;
                                                if controller.failed
                                                    && controller.paused_for_review
                                                    && !controller.notes.items.iter().any(|item| item.needs_review && !item.user_locked)
                                                {
                                                    controller.failed = false;
                                                    controller.paused_for_review = false;
                                                    controller.last_started = None;
                                                    controller.status = "已确认当前来源并锁定条目，将继续整理；原来源记录已保留。".into();
                                                } else if controller.failed && controller.paused_for_review {
                                                    controller.status = "部分来源仍待核对；已确认的条目已锁定并保存。".into();
                                                } else {
                                                    controller.status = "已确认当前来源并锁定条目，原来源记录已保留。".into();
                                                }
                                            }
                                            Err(error) => controller.status = format!("来源未确认：{error:#}"),
                                        }
                                    }
                                    cx.notify();
                                })))
                        })
                        .when(item.user_locked, |d| {
                            d.child(
                                button(("unlock-live-note", index), "解除锁定", false)
                                    .ghost()
                                    .disabled(controller.pending_save.is_some() || controller.editing.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(controller) = &mut this.live_notes {
                                            if controller.pending_save.is_some() || controller.editing.is_some() {
                                                return;
                                            }
                                            if controller.notes.revision != reviewed_notes_revision {
                                                controller.status = "纪要已变化，请重新查看后解除锁定。".into();
                                                cx.notify();
                                                return;
                                            }
                                            let mut updated = controller.notes.clone();
                                            match updated
                                                .unlock(&unlock_id)
                                                .and_then(|()| updated.save_if_unchanged(&controller.base, &controller.saved_notes))
                                            {
                                                Ok(()) => {
                                                    controller.saved_notes = updated.clone();
                                                    controller.notes = updated;
                                                    controller.status =
                                                        "已解除锁定；后续更新可提出修改。".into();
                                                }
                                                Err(error) => {
                                                    controller.status =
                                                        format!("解除锁定未保存：{error:#}")
                                                }
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(button(("remove-live-note", index), "删除条目", false)
                            .ghost().disabled(controller.pending_save.is_some() || controller.editing.is_some())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(controller) = &mut this.live_notes {
                                    if controller.pending_save.is_some() || controller.editing.is_some() { return; }
                                    if controller.notes.revision != reviewed_notes_revision {
                                        controller.status = "纪要已变化，请重新查看后删除。".into();
                                        cx.notify();
                                        return;
                                    }
                                    let mut updated = controller.notes.clone();
                                    match updated.remove_manually(&remove_id).and_then(|()| updated.save_if_unchanged(&controller.base, &controller.saved_notes)) {
                                        Ok(()) => {
                                            controller.saved_notes = updated.clone();
                                            controller.notes = updated;
                                            if controller.paused_for_review && !controller.notes.items.iter().any(|item| item.needs_review && !item.user_locked) {
                                                controller.paused_for_review = false;
                                                controller.failed = false;
                                                controller.last_started = None;
                                            }
                                            controller.status = "条目已删除，原记录保留在历史中。".into();
                                        }
                                        Err(error) => controller.status = format!("条目未删除：{error:#}"),
                                    }
                                }
                                cx.notify();
                            })))
                        .when(can_undo_merge, |d| d.child(
                            button(("undo-merge-live-note", index), "撤销最近合并", false)
                                .ghost().disabled(controller.pending_save.is_some() || controller.editing.is_some())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        if controller.pending_save.is_some() || controller.editing.is_some() { return; }
                                        if controller.notes.revision != reviewed_notes_revision {
                                            controller.status = "纪要已变化，请重新查看后撤销合并。".into();
                                        } else {
                                            let mut updated = controller.notes.clone();
                                            match updated.undo_last_merge(&undo_merge_id).and_then(|()| updated.save_if_unchanged(&controller.base, &controller.saved_notes)) {
                                                Ok(()) => {
                                                    controller.saved_notes = updated.clone();
                                                    controller.notes = updated;
                                                    controller.status = "已拆回两条并锁定，请重新核对来源。".into();
                                                }
                                                Err(error) => controller.status = format!("未撤销合并：{error:#}"),
                                            }
                                        }
                                    }
                                    cx.notify();
                                }))))
                        .when_some(merge_target, |d, (target, target_text)| {
                            d.child(
                                button(("merge-live-note", index), if index == 0 {
                                    format!("与上一条合并：{}", target_text.chars().take(50).collect::<String>())
                                } else { "与上一条合并".into() }, false)
                                    .ghost()
                                    .disabled(controller.pending_save.is_some() || controller.editing.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(controller) = &mut this.live_notes {
                                            if controller.pending_save.is_some() || controller.editing.is_some() {
                                                return;
                                            }
                                            if controller.notes.revision != reviewed_notes_revision {
                                                controller.status = "纪要已变化，请重新查看两条内容后合并。".into();
                                                cx.notify();
                                                return;
                                            }
                                            let mut updated = controller.notes.clone();
                                            match updated
                                                .merge_and_lock(&target, &merge_source)
                                                .and_then(|()| updated.save_if_unchanged(&controller.base, &controller.saved_notes))
                                            {
                                                Ok(()) => {
                                                    controller.saved_notes = updated.clone();
                                                    controller.notes = updated;
                                                    controller.editing = None;
                                                    controller.status =
                                                        "已合并并锁定；来源和原条目记录已保留。"
                                                            .into();
                                                }
                                                Err(error) => {
                                                    controller.status = format!("未合并：{error:#}")
                                                }
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .when_some(
                            {
                                #[cfg(feature = "internal-player")]
                                {
                                    audio_start
                                }
                                #[cfg(not(feature = "internal-player"))]
                                {
                                    None::<u64>
                                }
                            },
                            |d, start| {
                                #[cfg(feature = "internal-player")]
                                let d = d.child(
                                    button(("live-note-play", index), "回听", false)
                                        .ghost()
                                        .disabled(self.active() || item.needs_review)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let current = this.live_notes.as_ref().and_then(|controller| {
                                                if !controller.matches_transcript(this.transcript_output.as_ref(), &this.transcript) {
                                                    return None;
                                                }
                                                let mut checked = controller.notes.clone();
                                                checked.refresh_staleness(&this.transcript).ok()?;
                                                let item = checked.items.iter().find(|item| item.item_id == play_item_id)?;
                                                let source = item.evidence.first()?;
                                                if this.transcript.segments.iter().filter(|segment| segment.id == source.segment_id).take(2).count() != 1 {
                                                    return None;
                                                }
                                                let shown = playback_evidence.as_ref()?;
                                                let current = serde_json::to_value(source).ok()?;
                                                if item.needs_review || source.end_ms <= source.start_ms
                                                    || shown != &current {
                                                    return None;
                                                }
                                                Some(source.start_ms)
                                            });
                                            if current == Some(start) {
                                                this.seek_player_from_segment(start, cx);
                                            } else {
                                                this.transcription_status = "纪要来源已变化，请先核对后再回听。".into();
                                                cx.notify();
                                            }
                                        })),
                                );
                                #[cfg(not(feature = "internal-player"))]
                                let _ = start;
                                d
                            },
                        )
                        .child(
                            button(
                                ("live-note-edit", index),
                                if item.user_locked {
                                    "编辑 · 已锁定"
                                } else {
                                    "编辑"
                                },
                                false,
                            )
                            .disabled(controller.pending_save.is_some() || controller.editing.is_some())
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    let valid = this.live_notes.as_ref().is_some_and(|controller| {
                                        controller.pending_save.is_none() && controller.editing.is_none()
                                            && controller.notes.revision == reviewed_notes_revision
                                            && controller.matches_transcript(this.transcript_output.as_ref(), &this.transcript)
                                    });
                                    if !valid {
                                        if let Some(controller) = &mut this.live_notes {
                                            controller.status = "纪要或当前稿件已变化，请重新查看后编辑。".into();
                                        }
                                        cx.notify();
                                        return;
                                    }
                                    this.live_note_edit_input.update(cx, |input, cx| {
                                        input.set_value(text.clone(), window, cx)
                                    });
                                    if let Some(controller) = &mut this.live_notes {
                                        controller.editing = Some(id.clone());
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                );
                if item.needs_review || item.evidence.len() > 1 {
                    const SOURCES_PER_PAGE: usize = 8;
                    let source_page_count = item.evidence.len().div_ceil(SOURCES_PER_PAGE).max(1);
                    let source_page = controller
                        .source_pages
                        .get(&item.item_id)
                        .copied()
                        .unwrap_or(0)
                        .min(source_page_count - 1);
                    let timeline = opencap_asr::SpeakerTimeline::new(&self.transcript);
                    let mut sources =
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label(if item.needs_review {
                                "请逐条核对当前来源后再确认："
                            } else {
                                "所有来源（点击查看对应原文）："
                            }));
                    if item.evidence.is_empty() {
                        sources = sources.child(label("该条目没有可核对的来源。"));
                    }
                    for (source_index, evidence) in item
                        .evidence
                        .iter()
                        .enumerate()
                        .skip(source_page * SOURCES_PER_PAGE)
                        .take(SOURCES_PER_PAGE)
                    {
                        let segment_id = evidence.segment_id;
                        let mut matches = self
                            .transcript
                            .segments
                            .iter()
                            .filter(|segment| segment.id == segment_id);
                        let current = matches.next().filter(|_| matches.next().is_none());
                        let shown_source = serde_json::to_value(evidence).ok();
                        let shown_item_id = item.item_id.clone();
                        #[cfg(feature = "internal-player")]
                        let playback_source = shown_source.clone();
                        #[cfg(feature = "internal-player")]
                        let playback_item_id = shown_item_id.clone();
                        #[cfg(feature = "internal-player")]
                        let playback_start = evidence.start_ms;
                        let shown_segment = current.map(|segment| {
                            (
                                segment.revision,
                                segment.start_ms,
                                segment.end_ms,
                                segment.text.clone(),
                                segment.finalized,
                                segment.correcting,
                                segment.review.clone(),
                                segment.is_confirmed(),
                            )
                        });
                        let shown_speaker =
                            current.map(|segment| timeline.label(segment.start_ms, segment.end_ms));
                        let description = current
                            .map(|segment| {
                                let speaker = timeline.label(segment.start_ms, segment.end_ms);
                                let unstable = !segment.is_confirmed()
                                    || segment.text.trim().is_empty()
                                    || segment.end_ms < segment.start_ms;
                                let excerpt: String = segment.text.chars().take(200).collect();
                                let truncated = segment.text.chars().nth(200).is_some();
                                format!(
                                    "{}{} · {} · {}{}",
                                    if unstable { "尚未稳定 · " } else { "" },
                                    if segment.end_ms > segment.start_ms {
                                        opencap_asr::timestamp(segment.start_ms, '.')
                                    } else {
                                        "时间不可用".into()
                                    },
                                    if speaker.is_empty() {
                                        "说话人未确定"
                                    } else {
                                        &speaker
                                    },
                                    excerpt,
                                    if truncated { "…" } else { "" }
                                )
                            })
                            .unwrap_or_else(|| format!("句段 {segment_id} 不存在或编号不唯一"));
                        sources = sources.child(
                            button(
                                format!("review-live-source-{index}-{source_index}"),
                                description,
                                false,
                            )
                            .ghost()
                            .disabled(current.is_none())
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let same_item =
                                        this.live_notes.as_ref().is_some_and(|controller| {
                                            controller.matches_transcript(
                                                this.transcript_output.as_ref(),
                                                &this.transcript,
                                            ) && controller.notes.revision
                                                == reviewed_notes_revision
                                                && controller
                                                    .notes
                                                    .items
                                                    .iter()
                                                    .find(|item| item.item_id == shown_item_id)
                                                    .and_then(|item| {
                                                        item.evidence.get(source_index)
                                                    })
                                                    .and_then(|evidence| {
                                                        serde_json::to_value(evidence).ok()
                                                    })
                                                    .zip(shown_source.clone())
                                                    .is_some_and(|(current, shown)| {
                                                        current == shown
                                                    })
                                        });
                                    let mut matches = this
                                        .transcript
                                        .segments
                                        .iter()
                                        .filter(|segment| segment.id == segment_id);
                                    let current =
                                        matches.next().filter(|_| matches.next().is_none());
                                    let same_segment = current
                                        .zip(shown_segment.as_ref())
                                        .is_some_and(|(segment, shown)| {
                                            segment.revision == shown.0
                                                && segment.start_ms == shown.1
                                                && segment.end_ms == shown.2
                                                && segment.text.as_str() == shown.3.as_str()
                                                && segment.finalized == shown.4
                                                && segment.correcting == shown.5
                                                && segment.review.as_ref() == shown.6.as_ref()
                                                && segment.is_confirmed() == shown.7
                                        });
                                    let same_speaker = current
                                        .zip(shown_speaker.as_ref())
                                        .is_some_and(|(segment, shown)| {
                                            &this
                                                .transcript
                                                .speaker_timeline()
                                                .label(segment.start_ms, segment.end_ms)
                                                == shown
                                        });
                                    if !same_item || !same_segment || !same_speaker {
                                        this.transcription_status =
                                            "纪要、来源句段或说话人已变化，请重新查看后打开。"
                                                .into();
                                        cx.notify();
                                        return;
                                    }
                                    this.transcription_tab = true;
                                    this.meeting_page = workspace::MeetingPage::Transcript;
                                    this.transcript_view
                                        .update(cx, |view, cx| view.reveal_segment(segment_id, cx));
                                    cx.notify();
                                },
                            )),
                        );
                        #[cfg(feature = "internal-player")]
                        if evidence.end_ms > evidence.start_ms {
                            sources = sources.child(
                                button(
                                    format!("play-live-source-{index}-{source_index}"),
                                    "回听此来源",
                                    false,
                                )
                                .ghost()
                                .disabled(self.active() || item.needs_review || current.is_none())
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let valid =
                                            this.live_notes.as_ref().is_some_and(|controller| {
                                                if !controller.matches_transcript(
                                                    this.transcript_output.as_ref(),
                                                    &this.transcript,
                                                ) {
                                                    return false;
                                                }
                                                let mut checked = controller.notes.clone();
                                                if checked
                                                    .refresh_staleness(&this.transcript)
                                                    .is_err()
                                                {
                                                    return false;
                                                }
                                                checked
                                                    .items
                                                    .iter()
                                                    .find(|item| item.item_id == playback_item_id)
                                                    .filter(|item| !item.needs_review)
                                                    .and_then(|item| {
                                                        item.evidence.get(source_index)
                                                    })
                                                    .filter(|source| {
                                                        source.segment_id == segment_id
                                                            && source.start_ms == playback_start
                                                            && source.end_ms > source.start_ms
                                                    })
                                                    .and_then(|source| {
                                                        serde_json::to_value(source).ok()
                                                    })
                                                    .zip(playback_source.clone())
                                                    .is_some_and(|(current, shown)| {
                                                        current == shown
                                                    })
                                            });
                                        let unique = this
                                            .transcript
                                            .segments
                                            .iter()
                                            .filter(|segment| segment.id == segment_id)
                                            .take(2)
                                            .count()
                                            == 1;
                                        if valid && unique {
                                            this.seek_player_from_segment(playback_start, cx);
                                        } else {
                                            this.transcription_status =
                                                "纪要来源已变化或不唯一，请先核对后再回听。".into();
                                            cx.notify();
                                        }
                                    },
                                )),
                            );
                        }
                    }
                    if source_page_count > 1 {
                        let previous_id = item.item_id.clone();
                        let next_id = previous_id.clone();
                        let previous_meeting = controller.notes.meeting_id.clone();
                        let next_meeting = previous_meeting.clone();
                        let previous_run = controller.notes.run_id.clone();
                        let next_run = previous_run.clone();
                        sources = sources.child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    button(format!("source-prev-{index}"), "上一组来源", false)
                                        .ghost()
                                        .disabled(source_page == 0)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(controller) = &mut this.live_notes
                                                && controller.notes.revision
                                                    == reviewed_notes_revision
                                                && controller.notes.meeting_id == previous_meeting
                                                && controller.notes.run_id == previous_run
                                                && controller
                                                    .notes
                                                    .items
                                                    .iter()
                                                    .any(|item| item.item_id == previous_id)
                                            {
                                                controller.source_pages.insert(
                                                    previous_id.clone(),
                                                    source_page.saturating_sub(1),
                                                );
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(label(format!(
                                    "{} / {}",
                                    source_page + 1,
                                    source_page_count
                                )))
                                .child(
                                    button(format!("source-next-{index}"), "下一组来源", false)
                                        .ghost()
                                        .disabled(source_page + 1 >= source_page_count)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(controller) = &mut this.live_notes
                                                && controller.notes.revision
                                                    == reviewed_notes_revision
                                                && controller.notes.meeting_id == next_meeting
                                                && controller.notes.run_id == next_run
                                                && controller
                                                    .notes
                                                    .items
                                                    .iter()
                                                    .any(|item| item.item_id == next_id)
                                            {
                                                controller.source_pages.insert(
                                                    next_id.clone(),
                                                    (source_page + 1).min(source_page_count - 1),
                                                );
                                            }
                                            cx.notify();
                                        })),
                                ),
                        );
                    }
                    view = view.child(sources);
                }
            }
            if page_count > 1 {
                view = view.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("live-notes-newer", "较新的条目", false)
                                .ghost()
                                .disabled(page == 0 || controller.editing.is_some())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        controller.page = page.saturating_sub(1);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(label(format!("{} / {}", page + 1, page_count)))
                        .child(
                            button("live-notes-older", "较早的条目", false)
                                .ghost()
                                .disabled(page + 1 >= page_count || controller.editing.is_some())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        controller.page = page.saturating_add(1);
                                    }
                                    cx.notify();
                                })),
                        ),
                );
            }
            if controller.editing.is_some() {
                view = view.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            gpui::component::input::Input::new(&self.live_note_edit_input).flex_1(),
                        )
                        .child(
                            button("save-live-note-edit", "保存并锁定", false)
                                .disabled(controller.pending_save.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let text =
                                        this.live_note_edit_input.read(cx).value().to_string();
                                    if let Some(controller) = &mut this.live_notes {
                                        if controller.pending_save.is_some() {
                                            controller.status =
                                                "请先处理尚未保存的纪要结果；编辑输入仍保留。"
                                                    .into();
                                            cx.notify();
                                            return;
                                        }
                                        if let Some(id) = controller.editing.clone() {
                                            let mut updated = controller.notes.clone();
                                            match updated.edit_and_lock(&id, text).and_then(|_| {
                                                updated.save_if_unchanged(
                                                    &controller.base,
                                                    &controller.saved_notes,
                                                )
                                            }) {
                                                Ok(()) => {
                                                    controller.saved_notes = updated.clone();
                                                    controller.notes = updated;
                                                    controller.editing = None;
                                                    controller.status =
                                                        "已保存并锁定手动修改。".into();
                                                }
                                                Err(error) => {
                                                    controller.status = format!(
                                                        "手动修改未保存，输入保留：{error:#}"
                                                    )
                                                }
                                            }
                                        }
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            button("cancel-live-note-edit", "取消", false)
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(controller) = &mut this.live_notes {
                                        controller.editing = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                );
            }
            if controller.enabled && !controller.finished {
                view = view.child(
                    button("stop-live-notes", "停止增量更新", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(controller) = &mut this.live_notes {
                                controller.stop();
                            }
                            cx.notify();
                        })),
                );
            }
            if controller.failed {
                view = view.child(button("retry-live-notes", "重试增量纪要", false).on_click(
                    cx.listener(|this, _, _, cx| {
                        if let Some(controller) = &mut this.live_notes {
                            controller.retry(&this.transcript);
                        }
                        cx.notify();
                    }),
                ));
            }
            if controller.pending_save.is_some() {
                view = view.child(
                    button("recover-live-notes", "另存待保存结果", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(controller) = &mut this.live_notes {
                                controller.recover_pending();
                            }
                            cx.notify();
                        })),
                );
            }
            if (!controller.enabled || controller.finished)
                && controller.pending_save.is_none()
                && controller.editing.is_none()
                && controller.export.is_none()
            {
                view = view.child(
                    button("close-live-notes", "关闭增量面板", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.close_live_notes_panel();
                            cx.notify();
                        })),
                );
            }
        } else {
            view = view.child(label(self.live_notes_status.clone())).child(
                button("start-live-notes", "开启增量纪要", false)
                    .disabled(!can_start)
                    .on_click(cx.listener(|this, _, _, cx| this.enable_live_notes(cx))),
            );
        }
        view
    }
}
