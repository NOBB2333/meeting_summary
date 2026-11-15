//! Separate full-transcript proofreading tasks from minutes conversations.
use super::*;

impl Recorder {
    fn correction_notice(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.transcription_status = message.clone();
        self.ai.status = message;
    }

    pub(crate) fn request_transcript_correction(&mut self, cx: &mut Context<Self>) {
        if self.active()
            || self.transcript_write_busy()
            || self.ai.credentials_busy
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
        {
            self.correction_notice("请等待录制、逐字稿保存和当前校正完成后再试。");
            cx.notify();
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(config) => config,
            Err(e) => {
                self.correction_notice(format!("AI 服务配置不完整：{e}"));
                self.ai.config_status = self.ai.status.clone();
                self.settings = true;
                self.settings_page = settings::SettingsPage::Ai;
                cx.notify();
                return;
            }
        };
        self.ai.format.terminology = self.ai.terminology.read(cx).value().to_string();
        if let Err(e) = self.save_minutes_format() {
            self.correction_notice(format!("无法保存校正词表：{e}"));
            cx.notify();
            return;
        }
        // The minutes chat composer is unrelated to proofreading.
        let instruction = String::new();
        let key = if self
            .ai
            .key_origin
            .as_ref()
            .is_some_and(|origin| origin != &config.credential_id())
        {
            String::new()
        } else {
            self.ai.key.read(cx).value().trim().to_string()
        };
        if !key.is_empty() || (config.is_local()) {
            self.start_transcript_correction(config, key, instruction);
        } else {
            let base = self.transcript_output.clone();
            let baseline = match serde_json::to_vec(&self.transcript) {
                Ok(value) => value,
                Err(error) => {
                    self.correction_notice(format!("无法核对当前逐字稿：{error}"));
                    cx.notify();
                    return;
                }
            };
            self.ai.credentials_busy = true;
            self.correction_notice("正在读取全文校正的 AI 凭据…");
            let tasks: Vec<_> = config
                .credential_aliases()
                .iter()
                .map(|id| cx.read_credentials(id))
                .collect();
            cx.spawn(async move |entity, cx| {
                let mut result = Ok(None);
                for task in tasks {
                    result = task.await;
                    if matches!(&result, Ok(Some(_))) {
                        break;
                    }
                }
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
                                && current.provider == config.provider
                                && current.base_url == config.base_url
                        })
                    {
                        this.correction_notice(
                            "读取凭据期间会议或逐字稿已变化，请重新发起全文校正。",
                        );
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                            Ok(key) => this.start_transcript_correction(config, key, instruction),
                            Err(_) => this.correction_notice("密钥无效，请重新保存。"),
                        },
                        Ok(None) if config.is_local() => {
                            this.start_transcript_correction(config, String::new(), instruction)
                        }
                        _ => {
                            this.correction_notice("请先填写并保存 AI 密钥，再校正全文。");
                            this.ai.config_status = this.ai.status.clone();
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
            self.correction_notice("当前任务尚未结束或逐字稿尚未保存，请稍后重试全文校正。");
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            self.correction_notice("没有可保存的逐字稿文件，请先打开一场会议。");
            return;
        };
        let Some(expected) = self
            .transcript_disk_baseline
            .as_ref()
            .filter(|(base, _)| base == &output)
            .map(|(_, digest)| digest.clone())
        else {
            self.correction_notice("逐字稿缺少可靠磁盘基线；请重新加载后再校正。");
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
        self.correction_notice("正在校正转写全文 · 原始识别文字保留，完成后同步文件。");
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
            self.correction_notice("逐字稿缺少可靠磁盘基线；请重新加载后再恢复原文。");
            cx.notify();
            return;
        };
        let transcript = self.transcript.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.correction_task = Some(rx);
        self.ai_clock.start();
        self.correction_notice("正在恢复原始识别文字并同步文件…");
        std::thread::spawn(move || {
            let _ = tx.send(transcript.restore_recognized_text_guarded(&output, &expected));
        });
        cx.notify();
    }
}
