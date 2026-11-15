//! Separate full-transcript proofreading tasks from minutes conversations.
use super::*;

impl Recorder {
    pub(crate) fn request_transcript_correction(&mut self, cx: &mut Context<Self>) {
        if self.active()
            || self.transcript_write_busy()
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
        {
            self.transcription_status = "请等待当前任务完成，再校正全文".into();
            cx.notify();
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(config) => config,
            Err(e) => {
                self.ai.status = e.to_string();
                self.settings = true;
                self.settings_page = settings::SettingsPage::Ai;
                cx.notify();
                return;
            }
        };
        self.ai.format.terminology = self.ai.terminology.read(cx).value().to_string();
        if let Err(e) = self.save_minutes_format() {
            self.ai.status = e.to_string();
            cx.notify();
            return;
        }
        let instruction = self.ai.instruction.read(cx).value().to_string();
        let key = self.ai.key.read(cx).value().trim().to_string();
        if !key.is_empty() || (config.is_local() && config.provider == AiProvider::Ollama) {
            self.start_transcript_correction(config, key, instruction);
        } else {
            let base = self.transcript_output.clone();
            let baseline = match serde_json::to_vec(&self.transcript) {
                Ok(value) => value,
                Err(error) => {
                    self.ai.status = format!("无法核对当前逐字稿：{error}");
                    cx.notify();
                    return;
                }
            };
            self.ai.credentials_busy = true;
            self.ai.status = "正在读取全文校正的 AI 凭据…".into();
            let task = cx.read_credentials(&config.credential_id());
            cx.spawn(async move |entity, cx| {
                let result = task.await;
                let _ = entity.update(cx, |this, cx| {
                    this.ai.credentials_busy = false;
                    if this.quit_pending
                        || this.active()
                        || this.transcript_write_busy()
                        || this.transcript.primary_save_pending
                        || this.transcript_output != base
                        || !super::super::transcription::transcript_matches_snapshot(
                            &this.transcript,
                            &baseline,
                        )
                        || !this.ai_config_from_inputs(cx).is_ok_and(|current| {
                            current.credential_id() == config.credential_id()
                                && current.model == config.model
                                && current.background == config.background
                                && current.deep_reasoning == config.deep_reasoning
                        })
                    {
                        this.ai.status =
                            "读取凭据期间会议或逐字稿已变化，请重新发起全文校正。".into();
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                            Ok(key) => this.start_transcript_correction(config, key, instruction),
                            Err(_) => this.ai.status = "密钥无效，请重新保存".into(),
                        },
                        Ok(None) if config.is_local() => {
                            this.start_transcript_correction(config, String::new(), instruction)
                        }
                        _ => {
                            this.ai.status = "请先填写并保存 AI 密钥，再校正全文".into();
                            this.settings = true;
                            this.settings_page = settings::SettingsPage::Ai;
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }
    fn start_transcript_correction(&mut self, config: AiConfig, key: String, instruction: String) {
        if self.quit_pending
            || self.active()
            || self.transcript_write_busy()
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
        {
            self.ai.status = "当前任务尚未结束或逐字稿尚未保存，请稍后重试全文校正。".into();
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            self.ai.status = "没有可保存的转写文件".into();
            return;
        };
        let Some(expected) = self
            .transcript_disk_baseline
            .as_ref()
            .filter(|(base, _)| base == &output)
            .map(|(_, digest)| digest.clone())
        else {
            self.ai.status = "逐字稿缺少可靠磁盘基线；请重新加载后再校正。".into();
            return;
        };
        let transcript = self.transcript.clone();
        let terms = self.ai.format.terminology.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.ai.cancelled = cancelled.clone();
        self.ai.events = Some(event_rx);
        self.ai.correction_task = Some(rx);
        self.ai_clock.start();
        self.ai.status = "正在校正转写全文 · 原始识别文字保留，完成后同步文件".into();
        self.transcription_status = self.ai.status.clone();
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::correct_transcript_guarded(
                &transcript,
                &config,
                &key,
                &terms,
                &instruction,
                &output,
                &cancelled,
                &expected,
                |event| {
                    let _ = event_tx.send(event);
                },
            ));
        });
    }
    pub(crate) fn restore_transcript_original(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.transcript_write_busy() || self.transcript.primary_save_pending {
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            return;
        };
        let Some(expected) = self
            .transcript_disk_baseline
            .as_ref()
            .filter(|(base, _)| base == &output)
            .map(|(_, digest)| digest.clone())
        else {
            self.ai.status = "逐字稿缺少可靠磁盘基线；请重新加载后再恢复原文。".into();
            cx.notify();
            return;
        };
        let transcript = self.transcript.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.correction_task = Some(rx);
        self.ai_clock.start();
        self.ai.status = "正在恢复原始识别文字并同步文件…".into();
        std::thread::spawn(move || {
            let _ = tx.send(transcript.restore_recognized_text_guarded(&output, &expected));
        });
        cx.notify();
    }
}
