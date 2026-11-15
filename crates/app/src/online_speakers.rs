//! Owns the opt-in online speaker task independently of recording and ASR.
use super::*;
use opencap_asr::{OnlineSpeakerEvent, OnlineSpeakerRegistry, OnlineSpeakerSession, Transcript};
use std::time::Instant;

fn run_file_evidence(
    run: &opencap_asr::TranscriptionRun,
) -> Option<(u64, opencap_core::MediaFingerprint)> {
    let stream = run
        .extra
        .get("input_audio")?
        .get("audio_stream_index")?
        .as_u64()?;
    let fingerprint =
        serde_json::from_value(run.extra.get("input_media_fingerprint")?.clone()).ok()?;
    Some((stream, fingerprint))
}

pub(super) struct Controller {
    session: Option<OnlineSpeakerSession>,
    pub base: Option<PathBuf>,
    pub registry: Option<OnlineSpeakerRegistry>,
    pub status: String,
    pub done: bool,
    expected_run_id: Option<String>,
    finish_started: Option<Instant>,
    drain_progress: u64,
    drain_progress_at: Option<Instant>,
    worker_stopped: Option<Instant>,
    terminal_wait_started: Option<Instant>,
    save: Option<(
        u64,
        Option<PathBuf>,
        crossbeam_channel::Receiver<anyhow::Result<()>>,
    )>,
    save_failed: bool,
    saved_revision: u64,
    recovered_revision: u64,
    pub recovery_dir: Option<PathBuf>,
    last_save: Instant,
    sync_needed: bool,
    model_sha256: Option<String>,
    library_sha256: Option<String>,
    file_input: bool,
    input_incomplete_reason: Option<String>,
    expected_file_end_ms: Option<u64>,
    file_terminal_pending: bool,
    file_transcription_pending: bool,
    independent_file: bool,
    file_source_evidence: Option<(u64, opencap_core::MediaFingerprint)>,
    transcription_source_evidence: Option<(u64, opencap_core::MediaFingerprint)>,
    file_stage_seen: u8,
}
impl Controller {
    pub fn has_unsaved_work(&self) -> bool {
        self.file_terminal_pending
            || self.file_transcription_pending
            || self
                .session
                .as_ref()
                .is_some_and(|session| !session.finished())
            || self.save.is_some()
            || self.registry.as_ref().is_some_and(|registry| {
                registry.revision
                    > if self.base.is_none() || registry.run_id.is_none() {
                        self.saved_revision.max(self.recovered_revision)
                    } else {
                        self.saved_revision
                    }
            })
    }
    pub fn awaiting_identity(&self) -> bool {
        self.has_unsaved_work()
            && (self.base.is_none()
                || self
                    .registry
                    .as_ref()
                    .is_some_and(|registry| registry.run_id.is_none()))
    }
    pub fn settlement_needs_attention(&self) -> bool {
        self.has_unsaved_work() && self.save_failed
    }
    pub fn can_replace(&self) -> bool {
        self.done
            && !self.file_terminal_pending
            && !self.file_transcription_pending
            && self
                .session
                .as_ref()
                .is_none_or(|session| session.finished())
            && self.save.is_none()
            && self.registry.as_ref().is_none_or(|registry| {
                registry.revision
                    <= if self.base.is_none() || registry.run_id.is_none() {
                        self.saved_revision.max(self.recovered_revision)
                    } else {
                        self.saved_revision
                    }
            })
    }
    pub fn ready_for_backfill(&self) -> bool {
        self.can_replace() && !self.sync_needed
    }
    /// Minutes may proceed after inference and file identity/EOF settle; they
    /// need not wait for a failed checkpoint save or invent speaker names.
    pub fn settled_for_notes(&self) -> bool {
        self.done && !self.file_terminal_pending && !self.file_transcription_pending
    }
    pub fn can_cancel_file(&self) -> bool {
        self.independent_file && !self.done
    }
    pub fn start(
        library: PathBuf,
        model: PathBuf,
        file_base: Option<PathBuf>,
    ) -> anyhow::Result<(Self, crossbeam_channel::Sender<PcmAudio>)> {
        let session = OnlineSpeakerSession::start_nemo(library, model)?;
        let sender = session
            .audio_sender()
            .ok_or_else(|| anyhow::anyhow!("在线说话人输入不可用"))?;
        Ok((Self::from_session(session, file_base, false), sender))
    }

    pub fn start_file(
        library: PathBuf,
        model: PathBuf,
        base: PathBuf,
        source: PathBuf,
    ) -> anyhow::Result<Self> {
        let session = OnlineSpeakerSession::start_nemo_file(library, model, source, None)?;
        Ok(Self::from_session(session, Some(base), true))
    }

    fn from_session(
        session: OnlineSpeakerSession,
        file_base: Option<PathBuf>,
        independent_file: bool,
    ) -> Self {
        Self {
            session: Some(session),
            base: file_base.clone(),
            registry: None,
            status: if independent_file {
                "正在准备说话人模型和文件输入…"
            } else {
                "正在准备本机在线说话人模型…"
            }
            .into(),
            done: false,
            expected_run_id: None,
            finish_started: None,
            drain_progress: 0,
            drain_progress_at: None,
            worker_stopped: None,
            terminal_wait_started: None,
            save: None,
            save_failed: false,
            saved_revision: 0,
            recovered_revision: 0,
            recovery_dir: None,
            last_save: Instant::now() - Duration::from_secs(5),
            sync_needed: false,
            model_sha256: None,
            library_sha256: None,
            file_input: file_base.is_some(),
            input_incomplete_reason: None,
            expected_file_end_ms: None,
            file_terminal_pending: false,
            file_transcription_pending: file_base.is_some(),
            independent_file,
            file_source_evidence: None,
            transcription_source_evidence: None,
            file_stage_seen: 0,
        }
    }

    pub fn restore(base: PathBuf, registry: OnlineSpeakerRegistry) -> Self {
        let saved_revision = registry.revision;
        let model_sha256 = registry.model_sha256.clone();
        let library_sha256 = registry.library_sha256.clone();
        let expected_run_id = registry.run_id.clone();
        let status = if registry.finished {
            "已载入上次保存的在线说话人结果；匿名编号请核对。"
        } else {
            "已载入在线说话人检查点；该运行未记录完整收尾。"
        };
        Self {
            session: None,
            base: Some(base),
            registry: Some(registry),
            status: status.into(),
            done: true,
            expected_run_id,
            finish_started: None,
            drain_progress: 0,
            drain_progress_at: None,
            worker_stopped: None,
            terminal_wait_started: None,
            save: None,
            save_failed: false,
            saved_revision,
            recovered_revision: 0,
            recovery_dir: None,
            last_save: Instant::now(),
            sync_needed: false,
            model_sha256,
            library_sha256,
            file_input: false,
            input_incomplete_reason: None,
            expected_file_end_ms: None,
            file_terminal_pending: false,
            file_transcription_pending: false,
            independent_file: false,
            file_source_evidence: None,
            transcription_source_evidence: None,
            file_stage_seen: 0,
        }
    }

    pub fn cancel(&mut self, reason: &str) {
        if let Some(session) = &mut self.session {
            session.cancel();
        }
        self.done = true;
        self.file_terminal_pending = false;
        self.status = reason.into();
        if let Some(registry) = &mut self.registry {
            registry.finished = false;
            registry.incomplete_reason = Some(reason.into());
            registry.revision = registry.revision.saturating_add(1);
            self.sync_needed = true;
        }
    }

    pub fn belongs_to(&self, base: Option<&PathBuf>, run_id: &str) -> bool {
        self.base.as_ref() == base
            && self.expected_run_id.as_deref().or_else(|| {
                self.registry
                    .as_ref()
                    .and_then(|registry| registry.run_id.as_deref())
            }) == Some(run_id)
    }
    /// While the model is still loading, a same-meeting task with no bound
    /// run is unresolved rather than evidence that it belongs elsewhere.
    pub fn may_belong_to(&self, base: Option<&PathBuf>, run_id: &str) -> bool {
        self.base.as_ref() == base
            && self
                .expected_run_id
                .as_deref()
                .or_else(|| {
                    self.registry
                        .as_ref()
                        .and_then(|registry| registry.run_id.as_deref())
                })
                .is_none_or(|id| id == run_id)
    }

    /// Bind file input to the ASR run as soon as its Started event arrives.
    /// A second run in the same folder must not claim the older task.
    pub fn bind_file_run(&mut self, base: Option<&PathBuf>, run_id: &str) -> anyhow::Result<()> {
        if !self.file_input || self.base.as_ref() != base {
            return Ok(());
        }
        anyhow::ensure!(!run_id.is_empty(), "识别运行身份为空");
        if let Some(existing) = self.expected_run_id.as_deref().or_else(|| {
            self.registry
                .as_ref()
                .and_then(|registry| registry.run_id.as_deref())
        }) {
            anyhow::ensure!(existing == run_id, "在线说话人属于另一识别运行");
        }
        if let Some(registry) = &mut self.registry {
            registry.bind_run(run_id)?;
        }
        self.expected_run_id = Some(run_id.to_owned());
        Ok(())
    }

    /// Pin the original run and begin bounded drain before the shared view is
    /// switched. Late model startup must not bind to the next meeting's run.
    pub fn finish_for_run(
        &mut self,
        base: Option<&PathBuf>,
        run_id: &str,
        recording_end_ms: Option<u64>,
    ) -> anyhow::Result<bool> {
        if base.is_none() || self.base.as_ref() != base {
            return Ok(false);
        }
        if let Some(existing) = self.expected_run_id.as_deref().or_else(|| {
            self.registry
                .as_ref()
                .and_then(|registry| registry.run_id.as_deref())
        }) {
            anyhow::ensure!(existing == run_id, "在线说话人属于其他识别运行");
        }
        self.expected_run_id = Some(run_id.to_owned());
        if let Some(registry) = &mut self.registry {
            registry.bind_run(run_id)?;
        }
        if !self.done && self.finish_started.is_none() && !self.independent_file {
            if let Some(session) = &mut self.session {
                session.finish_input(if self.file_input {
                    None
                } else {
                    recording_end_ms
                });
            }
            self.finish_started = Some(Instant::now());
        }
        Ok(true)
    }
    pub fn finish_file_input(&mut self, end_ms: Option<u64>, incomplete_reason: Option<String>) {
        if !self.file_input {
            return;
        }
        if let Some(end) = end_ms {
            self.expected_file_end_ms = Some(end);
        }
        if self.done {
            // The worker may finish after the producer drops its sender but
            // before the UI consumes the producer's EOF event.
            let reason = incomplete_reason.or_else(|| {
                self.registry
                    .as_ref()
                    .zip(end_ms)
                    .and_then(|(registry, end)| {
                        (end > registry.input_end_ms.saturating_add(2)).then(|| {
                            "说话人音频未覆盖文件解码终点；结果不完整，请核对。".to_owned()
                        })
                    })
            });
            if let Some(reason) = reason {
                self.file_terminal_pending = false;
                self.input_incomplete_reason = Some(reason.clone());
                if let Some(registry) = &mut self.registry {
                    registry.finished = false;
                    registry.incomplete_reason = Some(reason.clone());
                    registry.revision = registry.revision.saturating_add(1);
                    self.sync_needed = true;
                }
                self.status = reason;
            } else if self.file_terminal_pending
                && end_ms.is_some()
                && self.input_incomplete_reason.is_none()
                && (!self.independent_file || !self.file_transcription_pending)
            {
                self.file_terminal_pending = false;
                if let Some(registry) = &mut self.registry {
                    registry.finished = true;
                    registry.incomplete_reason = None;
                    registry.revision = registry.revision.saturating_add(1);
                    self.sync_needed = true;
                }
                self.status = "在线说话人已收尾；匿名标签仍需核对。".into();
            }
            return;
        }
        if incomplete_reason.is_some() {
            self.input_incomplete_reason = incomplete_reason;
        }
        if self.finish_started.is_some() {
            // A meeting switch can begin draining before the file decoder
            // releases its sender. Preserve the eventual known EOF check.
            if self.input_incomplete_reason.is_none()
                && let Some(session) = &mut self.session
            {
                session.finish_input(end_ms);
            }
            return;
        }
        if let Some(session) = &mut self.session {
            session.finish_input(if self.input_incomplete_reason.is_some() {
                None
            } else {
                end_ms
            });
        }
        self.finish_started = Some(Instant::now());
        self.status = if self.input_incomplete_reason.is_some() {
            "文件音频输入不完整，正在保存已接收的说话人结果…"
        } else {
            "文件音频输入结束，在线说话人正在排空已接收的音频…"
        }
        .into();
    }

    /// A fingerprint check can finish after the speaker worker has drained.
    pub fn finish_file_transcription(
        &mut self,
        base: Option<&PathBuf>,
        run_id: Option<&str>,
        succeeded: bool,
    ) {
        let existing = self.expected_run_id.as_deref().or_else(|| {
            self.registry
                .as_ref()
                .and_then(|registry| registry.run_id.as_deref())
        });
        if self.file_input
            && self.base.as_ref() == base
            && existing.is_none_or(|id| Some(id) == run_id)
        {
            if self.file_transcription_pending && !succeeded {
                if self.independent_file {
                    self.cancel("文件转写未正常结束，在线说话人已停止；已有结果待核对。");
                } else {
                    self.finish_file_input(
                        None,
                        Some("文件转写未正常结束，无法确认说话人输入完整性；请核对。".into()),
                    );
                }
            }
            self.file_transcription_pending = false;
            if succeeded
                && self.file_terminal_pending
                && self.independent_file
                && self
                    .file_source_evidence
                    .as_ref()
                    .zip(self.transcription_source_evidence.as_ref())
                    .is_some_and(|(speaker, transcription)| speaker == transcription)
            {
                self.finish_file_input(self.expected_file_end_ms, None);
            }
        }
    }

    pub fn verify_file_transcription(
        &mut self,
        base: Option<&PathBuf>,
        run: &opencap_asr::TranscriptionRun,
    ) {
        if !self.independent_file
            || self.base.as_ref() != base
            || self
                .expected_run_id
                .as_deref()
                .or_else(|| {
                    self.registry
                        .as_ref()
                        .and_then(|registry| registry.run_id.as_deref())
                })
                .is_some_and(|id| id != run.run_id.as_str())
        {
            return;
        }
        self.transcription_source_evidence = run_file_evidence(run);
        if self.transcription_source_evidence.is_none() {
            self.mark_file_identity_unavailable(
                "转写未记录可核实的媒体指纹和音轨，说话人结果保留为独立检查点。",
            );
        } else {
            self.check_file_identity();
        }
    }

    fn mark_file_identity_unavailable(&mut self, reason: &str) {
        self.file_terminal_pending = false;
        self.input_incomplete_reason = Some(reason.into());
        if let Some(registry) = &mut self.registry {
            registry.finished = false;
            registry.incomplete_reason = Some(reason.into());
            registry.revision = registry.revision.saturating_add(1);
            self.sync_needed = true;
        }
        self.status = reason.into();
    }

    fn check_file_identity(&mut self) {
        if let Some((speaker, transcription)) = self
            .file_source_evidence
            .as_ref()
            .zip(self.transcription_source_evidence.as_ref())
        {
            if speaker != transcription {
                self.mark_file_identity_unavailable(
                    "说话人与转写的媒体指纹或音轨不一致，结果保留为独立检查点。",
                );
            } else {
                self.sync_needed = true;
            }
        }
    }

    pub fn mark_file_input_changed(&mut self, base: Option<&PathBuf>, run_id: &str) {
        if !self.file_input
            || self.base.as_ref() != base
            || self
                .expected_run_id
                .as_deref()
                .or_else(|| {
                    self.registry
                        .as_ref()
                        .and_then(|registry| registry.run_id.as_deref())
                })
                .is_some_and(|existing| existing != run_id)
        {
            return;
        }
        self.expected_run_id = Some(run_id.to_owned());
        let reason = "输入文件在转写期间发生变化；说话人结果不完整，请核对并重新处理。";
        self.file_terminal_pending = false;
        self.input_incomplete_reason = Some(reason.into());
        if let Some(registry) = &mut self.registry {
            registry.finished = false;
            registry.incomplete_reason = Some(reason.into());
            registry.revision = registry.revision.saturating_add(1);
            self.sync_needed = true;
        }
        self.status = reason.into();
    }

    pub fn applies_to(&self, base: Option<&PathBuf>, transcript: &Transcript) -> bool {
        (!self.independent_file
            || self
                .file_source_evidence
                .as_ref()
                .zip(self.transcription_source_evidence.as_ref())
                .is_some_and(|(speaker, transcription)| {
                    speaker == transcription
                        && transcript.run.as_ref().and_then(run_file_evidence).as_ref()
                            == Some(transcription)
                }))
            && self.base.as_ref() == base
            && self.registry.as_ref().is_some_and(|registry| {
                registry
                    .run_id
                    .as_deref()
                    .is_some_and(|id| transcript.run.as_ref().is_some_and(|run| run.run_id == id))
            })
    }
    pub fn apply(&self, transcript: &mut Transcript) -> anyhow::Result<()> {
        if self.independent_file {
            anyhow::ensure!(
                self.file_source_evidence
                    .as_ref()
                    .zip(self.transcription_source_evidence.as_ref())
                    .is_some_and(|(speaker, transcription)| speaker == transcription
                        && transcript.run.as_ref().and_then(run_file_evidence).as_ref()
                            == Some(transcription)),
                "说话人与当前稿件的媒体指纹或音轨不一致"
            );
        }
        if let Some(registry) = &self.registry {
            anyhow::ensure!(
                transcript.speaker_turns.iter().all(|turn| {
                    turn.extra
                        .get("online_session_id")
                        .and_then(|value| value.as_str())
                        == Some(registry.session_id.as_str())
                }),
                "当前稿件含独立说话人版本，在线结果未覆盖"
            );
            registry.apply_to_transcript(transcript)?;
            transcript.extra.insert("online_speakers".into(), serde_json::json!({
                "session_id": registry.session_id.as_str(), "revision": registry.revision,
                "backend": "nemo_diar_stream_experimental", "model_sha256": self.model_sha256.as_deref(),
                "library_sha256": self.library_sha256.as_deref(), "finished": registry.finished,
                "incomplete_reason": registry.incomplete_reason.as_deref(),
            }));
        }
        Ok(())
    }

    pub fn edit_name(&mut self, speaker: u32, name: &str) -> anyhow::Result<()> {
        self.registry
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("在线说话人尚未就绪"))?
            .rename(speaker, name)?;
        self.sync_needed = true;
        Ok(())
    }
    pub fn merge(&mut self, from: u32, into: u32) -> anyhow::Result<()> {
        self.registry
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("在线说话人尚未就绪"))?
            .merge(from, into)?;
        self.sync_needed = true;
        Ok(())
    }
    pub fn confirm(&mut self, speaker: u32) -> anyhow::Result<()> {
        self.registry
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("在线说话人尚未就绪"))?
            .confirm(speaker)?;
        self.sync_needed = true;
        Ok(())
    }
    pub fn split(&mut self, speaker: u32, at_ms: u64) -> anyhow::Result<()> {
        self.registry
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("在线说话人尚未就绪"))?
            .split(speaker, at_ms)?;
        self.sync_needed = true;
        Ok(())
    }

    fn poll(&mut self, app: &mut Recorder) -> bool {
        let mut changed = false;
        if self.base.is_none() && app.asr_live {
            self.base = app.transcript_output.clone();
        }
        let input_finished = if self.file_input {
            app.asr
                .as_ref()
                .is_none_or(|asr| asr.finished() && asr.events.is_empty())
        } else {
            !app.active()
        };
        if input_finished && self.finish_started.is_none() && !self.independent_file {
            if let Some(session) = &mut self.session {
                let end = (!self.file_input && app.stats.state == RecordingState::Finished)
                    .then_some((app.stats.duration_seconds * 1000.0) as u64);
                session.finish_input(end);
            }
            self.finish_started = Some(Instant::now());
        }
        let events: Vec<_> = self
            .session
            .as_mut()
            .map(|session| session.drain_events())
            .unwrap_or_default();
        for event in events {
            // The independent feeder can report its verified EOF after the
            // final model snapshot, including later in this same batch.
            // Never drop that identity evidence just because inference ended.
            if self.done
                && !matches!(
                    &event,
                    OnlineSpeakerEvent::FileInputEnded { .. } | OnlineSpeakerEvent::Failed(_)
                )
            {
                continue;
            }
            changed = true;
            match event {
                OnlineSpeakerEvent::Started {
                    capacity,
                    model_sha256,
                    library_sha256,
                } => {
                    let run_id = self.expected_run_id.clone().or_else(|| {
                        (self.base.is_some() && self.base == app.transcript_output)
                            .then(|| app.transcript.run.as_ref().map(|run| run.run_id.clone()))
                            .flatten()
                    });
                    match OnlineSpeakerRegistry::new(capacity, run_id) {
                        Ok(registry) => {
                            let mut registry = registry;
                            registry.model_sha256 = Some(model_sha256.clone());
                            registry.library_sha256 = Some(library_sha256.clone());
                            if self.independent_file {
                                registry
                                    .extra
                                    .insert("input_mode".into(), "independent_file".into());
                                if let Some((stream, fingerprint)) = &self.file_source_evidence {
                                    registry.extra.insert(
                                        "input_audio_stream_index".into(),
                                        (*stream).into(),
                                    );
                                    registry.extra.insert(
                                        "input_media_fingerprint".into(),
                                        serde_json::to_value(fingerprint)
                                            .unwrap_or(serde_json::Value::Null),
                                    );
                                }
                                if let Some(reason) = &self.input_incomplete_reason {
                                    registry.incomplete_reason = Some(reason.clone());
                                }
                            }
                            self.registry = Some(registry);
                            self.model_sha256 = Some(model_sha256);
                            self.library_sha256 = Some(library_sha256);
                            self.status = if self.independent_file
                                && self
                                    .session
                                    .as_ref()
                                    .is_some_and(|session| session.file_stage() == 1)
                            {
                                format!(
                                    "说话人模型已启动，正在核对文件指纹 · 最多 {capacity} 个槽。"
                                )
                            } else {
                                format!("在线说话人已启动 · 最多 {capacity} 个槽；匿名标签需核对。")
                            };
                        }
                        Err(error) => self.cancel(&format!("在线说话人不可用：{error:#}")),
                    }
                }
                OnlineSpeakerEvent::Snapshot(mut snapshot) => {
                    if let Some(registry) = &mut self.registry {
                        let finished = snapshot.finished;
                        if finished
                            && self.file_input
                            && self.input_incomplete_reason.is_none()
                            && self
                                .expected_file_end_ms
                                .is_some_and(|end| end > snapshot.input_end_ms.saturating_add(2))
                        {
                            self.input_incomplete_reason =
                                Some("说话人音频未覆盖文件解码终点；结果不完整，请核对。".into());
                        }
                        let pending_file_end = finished
                            && self.file_input
                            && (self.expected_file_end_ms.is_none()
                                || (self.independent_file && self.file_transcription_pending))
                            && self.input_incomplete_reason.is_none();
                        if finished && (self.input_incomplete_reason.is_some() || pending_file_end)
                        {
                            snapshot.finished = false;
                        }
                        match registry.apply_snapshot(snapshot) {
                            Ok(true) => {
                                if finished {
                                    registry.seal_backend();
                                }
                                if pending_file_end {
                                    self.file_terminal_pending = true;
                                    registry.incomplete_reason =
                                        Some("等待文件解码终点和来源核对。".into());
                                }
                                if finished && let Some(reason) = &self.input_incomplete_reason {
                                    registry.incomplete_reason = Some(reason.clone());
                                    registry.revision = registry.revision.saturating_add(1);
                                }
                                self.status = if pending_file_end {
                                    "在线说话人已收尾，正在等待文件解码终点核对。".into()
                                } else if finished && self.input_incomplete_reason.is_some() {
                                    "在线说话人已生成部分结果，正在保存；文件音频输入不完整，尾部待核对。".into()
                                } else if finished {
                                    "在线说话人已收尾；匿名标签仍需核对。".into()
                                } else {
                                    format!(
                                        "在线说话人已处理至 {}；匿名标签可能修订。",
                                        opencap_asr::timestamp(registry.input_end_ms, '.')
                                    )
                                };
                                self.sync_needed = true;
                                if finished {
                                    self.done = true;
                                }
                            }
                            Ok(false) => (),
                            Err(error) => self
                                .cancel(&format!("在线说话人结果无效，已保留先前结果：{error:#}")),
                        }
                    }
                }
                OnlineSpeakerEvent::Failed(error) => {
                    self.cancel(&format!("在线说话人未完成：{error}"))
                }
                OnlineSpeakerEvent::FileInputEnded {
                    end_ms,
                    audio_stream_index,
                    fingerprint,
                    error,
                } => {
                    self.file_source_evidence = audio_stream_index.zip(fingerprint);
                    if self.independent_file
                        && let Some(registry) = &mut self.registry
                    {
                        if let Some((stream, fingerprint)) = &self.file_source_evidence {
                            registry
                                .extra
                                .insert("input_audio_stream_index".into(), (*stream).into());
                            registry.extra.insert(
                                "input_media_fingerprint".into(),
                                serde_json::to_value(fingerprint)
                                    .unwrap_or(serde_json::Value::Null),
                            );
                        }
                        registry.revision = registry.revision.saturating_add(1);
                        self.sync_needed = true;
                    }
                    if self.independent_file
                        && self.file_source_evidence.is_none()
                        && error.is_none()
                    {
                        self.mark_file_identity_unavailable(
                            "文件说话人输入未返回可核实的媒体指纹和音轨；结果保留为独立检查点。",
                        );
                    }
                    self.check_file_identity();
                    self.finish_file_input(
                        end_ms,
                        error.map(|error| format!("文件说话人输入未完成：{error}")),
                    );
                }
            }
        }
        if self.independent_file
            && !self.done
            && self.finish_started.is_none()
            && self.input_incomplete_reason.is_none()
            && let Some(session) = &self.session
        {
            let stage = session.file_stage();
            if stage != self.file_stage_seen {
                self.file_stage_seen = stage;
                if let Some(message) = match stage {
                    1 => Some("正在核对文件指纹；说话人模型同时准备中。"),
                    2 => Some("正在独立读取文件并供给说话人模型。"),
                    3 => Some("文件音频已读取，正在复核媒体指纹；说话人模型继续处理。"),
                    _ => None,
                } {
                    self.status = message.into();
                    changed = true;
                }
            }
        }
        // If the producer has stopped and its event queue has been drained,
        // no EOF confirmation can arrive. Retain the partial checkpoint and
        // release the controller instead of waiting indefinitely.
        let producer_finished = if self.independent_file {
            self.session
                .as_ref()
                .is_none_or(|session| session.finished())
        } else {
            input_finished
        };
        let missing_terminal_confirmed = if self.file_terminal_pending
            && self.expected_file_end_ms.is_none()
            && producer_finished
        {
            // A producer may enqueue EOF immediately after this frame's drain.
            // Poll again before deciding it cannot arrive.
            self.terminal_wait_started
                .get_or_insert_with(Instant::now)
                .elapsed()
                >= Duration::from_secs(1)
        } else {
            self.terminal_wait_started = None;
            false
        };
        if missing_terminal_confirmed {
            self.file_terminal_pending = false;
            let reason = "未收到文件解码终点；说话人结果不完整，请核对。";
            self.input_incomplete_reason = Some(reason.into());
            if let Some(registry) = &mut self.registry {
                registry.finished = false;
                registry.incomplete_reason = Some(reason.into());
                registry.revision = registry.revision.saturating_add(1);
                self.sync_needed = true;
            }
            self.status = reason.into();
            changed = true;
        }
        // No further transcription events can arrive once the worker and its
        // queue have both ended, including after an unexpected worker exit.
        let current_run = app.transcript.run.as_ref().map(|run| run.run_id.as_str());
        let associated_run = self.expected_run_id.as_deref().or_else(|| {
            self.registry
                .as_ref()
                .and_then(|registry| registry.run_id.as_deref())
        });
        let current_producer = self.base.as_ref() == app.transcript_output.as_ref()
            && associated_run.is_none_or(|id| Some(id) == current_run);
        let displaced = self.file_transcription_pending
            && associated_run.is_some()
            && current_run.is_some()
            && !current_producer;
        if self.file_transcription_pending && input_finished && current_producer {
            let base = self.base.clone();
            let run_id = associated_run.map(str::to_owned);
            self.finish_file_transcription(base.as_ref(), run_id.as_deref(), false);
            changed = true;
        } else if displaced {
            // The old ASR can no longer supply its terminal evidence through
            // the shared view. Keep the independent speaker task running and
            // save it separately rather than binding the new run's result.
            self.file_transcription_pending = false;
            self.mark_file_identity_unavailable(
                "原转写运行已切换，无法核实其说话人来源；已有结果单独保存。",
            );
            changed = true;
        }
        if self.file_input
            && !self.done
            && let Some(start) = self.finish_started
        {
            let processed = self
                .session
                .as_ref()
                .map_or(0, |session| session.processed_blocks());
            if processed != self.drain_progress {
                self.drain_progress = processed;
                self.drain_progress_at = Some(Instant::now());
            } else if self.drain_progress_at.is_none() {
                self.drain_progress_at = Some(start);
            }
        }
        let drain_timed_out = self.finish_started.is_some_and(|start| {
            if self.file_input {
                start.elapsed() >= Duration::from_secs(30 * 60)
                    || self
                        .drain_progress_at
                        .is_some_and(|last| last.elapsed() >= Duration::from_secs(60))
            } else {
                start.elapsed() >= Duration::from_secs(5)
            }
        });
        if !self.done && drain_timed_out {
            self.cancel("在线说话人收尾超时；已收到的结果保留，剩余部分待核对。");
            changed = true;
        }
        if !self.done
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.finished())
        {
            let stopped = self.worker_stopped.get_or_insert_with(Instant::now);
            if stopped.elapsed() >= Duration::from_secs(1) {
                self.cancel("在线说话人提前结束；已收到的结果保留。");
                changed = true;
            }
        }
        if self.base.is_some()
            && self.base.as_ref() == app.transcript_output.as_ref()
            && let Some(registry) = &mut self.registry
            && (!self.done || self.expected_run_id.is_some())
            && let Some(run) = &app.transcript.run
            && self
                .expected_run_id
                .as_deref()
                .is_none_or(|id| id == run.run_id.as_str())
            && let Err(error) = registry.bind_run(&run.run_id)
        {
            self.cancel(&format!("在线说话人运行身份不匹配：{error:#}"));
            changed = true;
        }
        let transcript_online = app.transcript.extra.get("online_speakers");
        let needs_apply = self.registry.as_ref().is_some_and(|registry| {
            transcript_online
                .and_then(|value| value.get("session_id"))
                .and_then(|value| value.as_str())
                != Some(registry.session_id.as_str())
                || transcript_online
                    .and_then(|value| value.get("revision"))
                    .and_then(|value| value.as_u64())
                    .is_none_or(|revision| revision < registry.revision)
        });
        // A sync owns a cloned transcript and will return it later. Defer
        // speaker updates until it finishes so that older clone cannot replace
        // a newer in-memory speaker revision.
        if app.transcript_sync.is_none()
            && !app.voice_busy
            && app.speaker_task.is_none()
            && app.ai.correction_task.is_none()
            && needs_apply
            && self.applies_to(app.transcript_output.as_ref(), &app.transcript)
        {
            if let Err(error) = self.apply(&mut app.transcript) {
                self.status = format!("说话人结果未应用：{error:#}");
                // The checkpoint remains separate; do not claim to sync a
                // result that was intentionally refused by the current draft.
                self.sync_needed = false;
            } else {
                app.transcript_dirty = true;
                app.transcript.primary_save_pending = true;
                changed = true;
            }
        }
        if let Some((revision, recovery, receiver)) = &self.save {
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    self.save_failed = false;
                    if let Some(directory) = recovery {
                        self.recovered_revision = *revision;
                        self.recovery_dir = Some(directory.clone());
                        self.status = format!(
                            "未绑定身份的说话人结果已保存至 {}；不会自动套用到其他转写。",
                            directory.display()
                        );
                        changed = true;
                    } else {
                        self.saved_revision = *revision;
                        if self.done
                            && self.input_incomplete_reason.is_some()
                            && self
                                .registry
                                .as_ref()
                                .is_some_and(|registry| registry.revision == *revision)
                        {
                            self.status = "在线说话人部分结果已保存；输入不完整，请核对。".into();
                            changed = true;
                        }
                    }
                    self.save = None;
                }
                Ok(Err(error)) => {
                    self.save_failed = true;
                    self.status = format!("在线说话人检查点未保存：{error:#}");
                    self.save = None;
                    changed = true;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.save_failed = true;
                    self.status = "在线说话人保存线程退出；结果仍在内存。".into();
                    self.save = None;
                    changed = true;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => (),
            }
        }
        if self.save.is_none()
            && self.last_save.elapsed() >= Duration::from_secs(5)
            && let (Some(base), Some(registry)) = (&self.base, &self.registry)
            && registry.run_id.is_some()
            && registry.revision > self.saved_revision
        {
            let revision = registry.revision;
            let registry = registry.clone();
            let base = base.clone();
            let (tx, rx) = crossbeam_channel::bounded(1);
            self.save = Some((revision, None, rx));
            self.last_save = Instant::now();
            std::thread::spawn(move || {
                let _ = tx.send(registry.save(&base));
            });
        }
        if self.done
            && self.save.is_none()
            && self.last_save.elapsed() >= Duration::from_secs(5)
            && let Some(registry) = &self.registry
            && (self.base.is_none() || registry.run_id.is_none())
            && registry.revision > self.recovered_revision
        {
            let revision = registry.revision;
            let registry = registry.clone();
            let root = app.config.output_dir.join(".opencap-recovery");
            let (tx, rx) = crossbeam_channel::bounded(1);
            self.save = Some((revision, Some(root.clone()), rx));
            self.last_save = Instant::now();
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    std::fs::create_dir_all(&root)?;
                    // Persist the directory entry as well as the file within
                    // it when the platform supports directory syncing.
                    opencap_core::sync_parent_directory(&root)?;
                    let path =
                        root.join(format!("speakers-{}.json", opencap_core::new_stable_id()));
                    opencap_core::atomic_create_bytes(
                        &path,
                        &serde_json::to_vec_pretty(&registry)?,
                    )?;
                    Ok(())
                })();
                let _ = tx.send(result);
            });
        }
        // Once both producers have stopped, missing or mismatched source
        // evidence cannot become valid later. The checkpoint is still saved,
        // but there is no transcript update to wait for before backfill.
        if self.independent_file
            && self.done
            && !self.file_transcription_pending
            && self
                .session
                .as_ref()
                .is_none_or(|session| session.finished())
            && !self
                .file_source_evidence
                .as_ref()
                .zip(self.transcription_source_evidence.as_ref())
                .is_some_and(|(speaker, transcription)| speaker == transcription)
        {
            self.sync_needed = false;
        }
        if self.done
            && self.sync_needed
            && !app.active()
            && app.asr.as_ref().is_none_or(|asr| asr.finished())
            && self.applies_to(app.transcript_output.as_ref(), &app.transcript)
            && !app.transcript_write_busy()
        {
            self.sync_needed = false;
            app.retry_transcript_sync();
        }
        changed
    }
}

impl Recorder {
    pub(super) fn restore_online_for_transcript(
        &mut self,
        base: &std::path::Path,
        transcript: &mut Transcript,
    ) {
        if let Some(controller) = &self.online_speakers
            && !controller.can_replace()
        {
            if controller.applies_to(Some(&base.to_path_buf()), transcript) {
                let same_session = controller.registry.as_ref().is_some_and(|registry| {
                    transcript.speaker_turns.iter().all(|turn| {
                        turn.extra
                            .get("online_session_id")
                            .and_then(|value| value.as_str())
                            == Some(registry.session_id.as_str())
                    })
                });
                if !transcript.speaker_turns.is_empty() && !same_session {
                    self.online_speaker_status =
                        "当前稿件已有独立说话人版本；在线结果继续单独保存。".into();
                } else if let Err(error) = controller.apply(transcript) {
                    self.online_speaker_status = format!("当前在线说话人结果未应用：{error:#}");
                } else {
                    transcript.primary_save_pending = true;
                    self.online_speaker_status = controller.status.clone();
                }
            } else {
                self.online_speaker_status =
                    "上一场在线说话人仍在处理或保存；当前稿件保持原有说话人信息。".into();
            }
            // Reopening the same path is not permission to drop an active
            // worker: its run may still be draining or its checkpoint writing.
            return;
        }
        match OnlineSpeakerRegistry::load(base) {
            Ok(Some(registry)) => {
                let same_run = registry
                    .run_id
                    .as_deref()
                    .zip(transcript.run.as_ref().map(|run| run.run_id.as_str()))
                    .is_some_and(|(a, b)| a == b);
                if !same_run {
                    self.online_speaker_status =
                        "在线说话人检查点属于其他识别运行，未覆盖当前稿件。".into();
                    return;
                }
                if registry
                    .extra
                    .get("input_mode")
                    .and_then(|value| value.as_str())
                    == Some("independent_file")
                {
                    let saved = registry
                        .extra
                        .get("input_audio_stream_index")
                        .and_then(|value| value.as_u64())
                        .zip(
                            registry
                                .extra
                                .get("input_media_fingerprint")
                                .and_then(|value| {
                                    serde_json::from_value::<opencap_core::MediaFingerprint>(
                                        value.clone(),
                                    )
                                    .ok()
                                }),
                        );
                    let current = transcript.run.as_ref().and_then(run_file_evidence);
                    if saved.is_none() || saved != current {
                        self.online_speaker_status =
                            "文件说话人检查点缺少匹配的媒体指纹或音轨，未合入当前逐字稿。".into();
                        return;
                    }
                }
                let current_online = transcript.speaker_turns.iter().all(|turn| {
                    turn.extra
                        .get("online_session_id")
                        .and_then(|value| value.as_str())
                        == Some(registry.session_id.as_str())
                });
                if !transcript.speaker_turns.is_empty() && !current_online {
                    self.online_speaker_status =
                        "当前是会后精修版本；在线说话人检查点单独保留。".into();
                    return;
                }
                let saved_revision = transcript
                    .extra
                    .get("online_speakers")
                    .and_then(|value| value.get("revision"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let same_session = transcript
                    .extra
                    .get("online_speakers")
                    .and_then(|value| value.get("session_id"))
                    .and_then(|value| value.as_str())
                    == Some(registry.session_id.as_str());
                let newer = !same_session || registry.revision > saved_revision;
                let mut controller = Controller::restore(base.to_path_buf(), registry);
                if newer {
                    match controller.apply(transcript) {
                        Ok(()) => {
                            controller.sync_needed = true;
                            transcript.primary_save_pending = true;
                        }
                        Err(error) => {
                            self.online_speaker_status =
                                format!("在线说话人检查点未应用：{error:#}");
                            return;
                        }
                    }
                }
                self.online_speaker_status = controller.status.clone();
                self.online_speakers = Some(controller);
            }
            Ok(None) => {
                self.online_speakers = None;
                self.online_speaker_status.clear();
            }
            Err(error) => {
                self.online_speaker_status =
                    format!("在线说话人检查点无法读取，当前稿件保留：{error:#}")
            }
        }
    }

    pub(super) fn start_online_speakers(&mut self) -> Option<crossbeam_channel::Sender<PcmAudio>> {
        self.start_online_speakers_with_base(None)
    }
    pub(super) fn start_file_online_speakers(&mut self, base: PathBuf, source: PathBuf) -> bool {
        if !self.config.online_speakers {
            return false;
        }
        if self
            .online_speakers
            .as_ref()
            .is_some_and(|controller| !controller.can_replace())
        {
            self.online_speaker_status =
                "上一场在线说话人结果仍在收尾或保存，本场跳过在线说话人。".into();
            return false;
        }
        let (Some(library), Some(model)) = (
            self.config.online_speaker_library.clone(),
            self.config.online_speaker_model.clone(),
        ) else {
            self.online_speaker_status =
                "尚未选择可信原生库和固定版本模型，本场跳过在线说话人。".into();
            return false;
        };
        match Controller::start_file(library, model, base, source) {
            Ok(controller) => {
                self.online_speaker_status = controller.status.clone();
                self.online_speakers = Some(controller);
                true
            }
            Err(error) => {
                self.online_speaker_status = format!("在线说话人启动失败，转录继续：{error:#}");
                false
            }
        }
    }
    fn start_online_speakers_with_base(
        &mut self,
        file_base: Option<PathBuf>,
    ) -> Option<crossbeam_channel::Sender<PcmAudio>> {
        if !self.config.online_speakers {
            return None;
        }
        if self
            .online_speakers
            .as_ref()
            .is_some_and(|controller| !controller.can_replace())
        {
            self.online_speaker_status =
                "上一场在线说话人结果仍在收尾或保存，本场跳过在线说话人。".into();
            return None;
        }
        let (Some(library), Some(model)) = (
            self.config.online_speaker_library.clone(),
            self.config.online_speaker_model.clone(),
        ) else {
            self.online_speaker_status =
                "尚未选择可信原生库和固定版本模型，本场跳过在线说话人。".into();
            return None;
        };
        match Controller::start(library, model, file_base) {
            Ok((controller, sender)) => {
                self.online_speaker_status = controller.status.clone();
                self.online_speakers = Some(controller);
                Some(sender)
            }
            Err(error) => {
                self.online_speaker_status = format!("在线说话人启动失败，转录继续：{error:#}");
                None
            }
        }
    }
    pub(super) fn poll_online_speakers(&mut self) -> bool {
        let Some(mut controller) = self.online_speakers.take() else {
            return false;
        };
        let changed = controller.poll(self);
        self.online_speaker_status = controller.status.clone();
        self.online_speakers = Some(controller);
        changed
    }
}
