use super::*;
use gpui::component::{
    input::{Input, Textarea, TextareaState},
    text::TextView,
};
use opencap_asr::{
    AiConfig, AiProvider, MinutesAction, MinutesEvent, MinutesFormat, MinutesHistory,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct AiWorkspace {
    pub config: AiConfig,
    url: Entity<InputState>,
    model: Entity<InputState>,
    key: Entity<InputState>,
    instruction: Entity<TextareaState>,
    pub stream: String,
    pub pending_request: String,
    pub events: Option<crossbeam_channel::Receiver<MinutesEvent>>,
    editor: Entity<TextareaState>,
    editing: bool,
    editor_base: Option<PathBuf>,
    draft_saved: String,
    pub editor_sync: bool,
    pub document_task: Option<crossbeam_channel::Receiver<anyhow::Result<MinutesHistory>>>,
    credential_for: Option<String>,
    key_origin: Option<String>,
    credential_status: String,
    pending_key: Option<(String, String)>,
    chat: Entity<ai_chat::ChatView>,
    pub chat_dirty: bool,
    chat_reset: bool,
    pub clear_composer: bool,
    ask_only: bool,
    history_expanded: bool,
    conversation_tab: bool,
    format: MinutesFormat,
    format_expanded: bool,
    format_guidance: Entity<TextareaState>,
    pub history: MinutesHistory,
    pub status: String,
    pub credentials_busy: bool,
    pub cancelled: Arc<AtomicBool>,
    pub progress: Option<crossbeam_channel::Receiver<String>>,
}
fn ai_path() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(path) = smoke::isolated_dir() {
        return path.join("ai.json");
    }
    config_path().with_file_name("ai.json")
}
fn format_path() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(path) = smoke::isolated_dir() {
        return path.join("minutes-format.json");
    }
    config_path().with_file_name("minutes-format.json")
}
impl AiWorkspace {
    pub fn new(window: &mut Window, cx: &mut Context<Recorder>) -> Self {
        let (config, status) = if ai_path().exists() {
            match std::fs::read(ai_path())
                .map_err(anyhow::Error::from)
                .and_then(|b| serde_json::from_slice::<AiConfig>(&b).map_err(Into::into))
            {
                Ok(c) => (c, "AI 配置已载入；密钥仅在使用时从系统凭据库读取".into()),
                Err(_) => (AiConfig::default(), "AI 配置读取失败，请重新保存".into()),
            }
        } else {
            (
                AiConfig::default(),
                "默认 DeepSeek 官方 Responses；只需填写 API Key 并保存，重启后仍可使用".into(),
            )
        };
        let format: MinutesFormat = std::fs::read(format_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            format_guidance: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("例如：按议题分段，使用‘会议议定’，最后列责任部门；只输出正文。")
                    .default_value(format.guidance.clone())
                    .auto_grow(2, 6)
            }),
            format,
            format_expanded: false,
            url: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("服务根地址，DeepSeek 为 https://api.deepseek.com")
                    .default_value(config.base_url.clone())
            }),
            model: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("填写服务提供的模型名")
                    .default_value(config.model.clone())
            }),
            key: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("API Key；留空使用已保存密钥")
                    .masked(true)
            }),
            instruction: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(
                        "给 AI 提修改建议，例如：第二段再简短些，按公司的历史纪要格式调整。",
                    )
                    .auto_grow(3, 8)
            }),
            stream: String::new(),
            pending_request: String::new(),
            events: None,
            editor: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("在这里编写或修改纪要正文，支持 Markdown。")
                    .auto_grow(16, 32)
            }),
            editing: false,
            editor_base: None,
            draft_saved: String::new(),
            editor_sync: true,
            document_task: None,
            credential_for: None,
            key_origin: None,
            credential_status: "尚未检查密钥".into(),
            pending_key: None,
            chat: cx.new(|_| ai_chat::ChatView::new()),
            chat_dirty: true,
            chat_reset: true,
            clear_composer: false,
            ask_only: false,
            history_expanded: false,
            conversation_tab: true,
            config,
            history: MinutesHistory::default(),
            status,
            credentials_busy: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: None,
        }
    }
}
impl Recorder {
    pub(super) fn sync_ai_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai.editor_base != self.transcript_output {
            self.persist_minutes_draft(cx);
            self.ai.editing = false;
            self.ai.editor_base = self.transcript_output.clone();
            self.ai.editor_sync = true;
        }
        if self.ai.editor_sync {
            let draft = self
                .ai
                .editor_base
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p.with_extension("summary.draft.md")).ok())
                .filter(|s| !s.is_empty());
            let text = draft.clone().unwrap_or_else(|| self.summary_text.clone());
            self.ai.draft_saved = draft.clone().unwrap_or_default();
            self.ai.editing = draft.is_some();
            if self.ai.editing {
                self.ai.status = "已恢复未保存草稿；保存正文与 Word 后可继续对话。".into();
                self.ai.conversation_tab = false;
            }
            self.ai
                .editor
                .update(cx, |s, cx| s.set_value(text, window, cx));
            self.ai.editor_sync = false;
        }
        if self.ai.clear_composer {
            self.ai
                .instruction
                .update(cx, |s, cx| s.set_value("", window, cx));
            self.ai.clear_composer = false;
        }
        if let Some((origin, key)) = self.ai.pending_key.take()
            && self
                .ai_credentials_config(cx)
                .is_ok_and(|c| c.credential_id() == origin)
        {
            self.ai.key.update(cx, |s, cx| {
                s.set_masked(true, window, cx);
                s.set_value(key, window, cx);
            });
            self.ai.key_origin = Some(origin);
        }
        if let Ok(config) = self.ai_credentials_config(cx) {
            let origin = config.credential_id();
            if self
                .ai
                .key_origin
                .as_ref()
                .is_some_and(|old| old != &origin)
            {
                self.ai.key.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.set_masked(true, window, cx);
                });
                self.ai.key_origin = None;
            }
            if self.settings
                && self.settings_page == settings::SettingsPage::Ai
                && self.ai.credential_for.as_ref() != Some(&origin)
                && !self.ai.credentials_busy
            {
                self.load_ai_key(cx);
            }
        }
        if self.ai.chat_dirty {
            self.ai.chat.update(cx, |view, cx| {
                view.replace(
                    &self.ai.history,
                    &self.ai.pending_request,
                    &self.ai.stream,
                    self.summary_task.is_some(),
                    self.ai.chat_reset,
                    cx,
                )
            });
            self.ai.chat_dirty = false;
            self.ai.chat_reset = false;
        }
    }
    pub(super) fn persist_minutes_draft(&mut self, cx: &App) -> bool {
        if !self.ai.editing {
            return false;
        }
        let Some(base) = &self.ai.editor_base else {
            return false;
        };
        let text = self.ai.editor.read(cx).value().to_string();
        if text == self.ai.draft_saved {
            return false;
        }
        match opencap_asr::atomic_write(&base.with_extension("summary.draft.md"), text.as_bytes()) {
            Ok(()) => self.ai.draft_saved = text,
            Err(_) => {
                self.ai.status = "草稿暂未保存，请检查会议目录是否可写。".into();
                return true;
            }
        }
        false
    }
    fn load_ai_key(&mut self, cx: &mut Context<Self>) {
        if self.ai.credentials_busy {
            return;
        }
        let Ok(config) = self.ai_credentials_config(cx) else {
            return;
        };
        let origin = config.credential_id();
        self.ai.credential_for = Some(origin.clone());
        self.ai.credential_status = "正在检查系统凭据库…".into();
        self.ai.credentials_busy = true;
        let task = cx.read_credentials(&origin);
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.credentials_busy = false;
                if this
                    .ai_credentials_config(cx)
                    .is_ok_and(|c| c.credential_id() == origin)
                {
                    match result {
                        Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                            Ok(key) => {
                                this.ai.pending_key = Some((origin, key));
                                this.ai.credential_status = "已载入已存密钥 · 修改后需保存".into();
                            }
                            Err(_) => {
                                this.ai.credential_status = "已存密钥格式无效，请重新填写".into()
                            }
                        },
                        Ok(None) => {
                            this.ai.credential_status = if config.is_local() {
                                "本机服务可不填密钥"
                            } else {
                                "当前服务未保存密钥"
                            }
                            .into()
                        }
                        Err(_) => {
                            this.ai.credential_status =
                                "系统凭据库读取失败，可重新载入或填写密钥".into()
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn save_manual_minutes(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.transcription_busy() {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let history = self.ai.history.clone();
        let markdown = self.ai.editor.read(cx).value().to_string();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.document_task = Some(rx);
        self.ai.status = "正在保存正文、Word 与修订版本…".into();
        std::thread::spawn(move || {
            let _ = tx.send(history.save_revision(&base, &markdown, "手动编辑正文"));
        });
        cx.notify();
    }
    fn import_minutes_file(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.transcription_busy() || self.ai.editing {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择已修改的纪要（Word / Markdown / TXT）".into()),
        });
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let history = self.ai.history.clone();
        cx.spawn(async move |entity, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = entity.update(cx, |this, cx| {
                if this.active()
                    || this.transcription_busy()
                    || this.transcript_output.as_ref() != Some(&base)
                    || this.ai.editing
                {
                    return;
                }
                let (tx, rx) = crossbeam_channel::bounded(1);
                this.ai.document_task = Some(rx);
                this.ai.status = "正在读取修改后的正文并保存版本…".into();
                std::thread::spawn(move || {
                    let result = opencap_asr::read_minutes_document(&path).and_then(|markdown| {
                        history.save_revision(&base, &markdown, "同步外部文件修改")
                    });
                    let _ = tx.send(result);
                });
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn poll_minutes_document(&mut self) -> bool {
        let result = self
            .ai
            .document_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("文档保存线程已退出")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            });
        let Some(result) = result else {
            return false;
        };
        self.ai.document_task = None;
        match result {
            Ok(history) => {
                self.summary_text = history.latest().into();
                self.ai.history = history;
                if let Some(base) = &self.transcript_output {
                    let _ = std::fs::remove_file(base.with_extension("summary.draft.md"));
                }
                self.ai.draft_saved.clear();
                self.ai.editor_sync = true;
                self.ai.editing = false;
                self.ai.chat_dirty = true;
                self.ai.status = "正文、Word 与修订版本已同步；可继续对话修改。".into();
                self.remember_meeting();
            }
            Err(e) => self.ai.status = format!("同步失败；编辑内容和已有版本保留：{e:#}"),
        }
        true
    }
    fn save_minutes_format(&self) -> anyhow::Result<()> {
        if let Some(parent) = format_path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        opencap_asr::atomic_write(&format_path(), &serde_json::to_vec_pretty(&self.ai.format)?)
    }
    fn choose_format_references(&mut self, cx: &mut Context<Self>) {
        if self.transcription_busy() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择历史纪要（Word、Markdown 或 TXT），仅用于参考内容格式".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await {
                let _ = entity.update(cx, |this, cx| {
                    let mut selected = this.ai.format.references.clone();
                    for path in paths {
                        let ext = path
                            .extension()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase();
                        if !["docx", "md", "txt"].contains(&ext.as_str()) {
                            this.ai.status =
                                "格式参考支持 Word (.docx)、Markdown 和 UTF-8 TXT".into();
                            cx.notify();
                            return;
                        }
                        if !selected.contains(&path) {
                            selected.push(path);
                        }
                    }
                    if selected.len() > 5 {
                        this.ai.status = "最多选择 5 个格式参考文件".into();
                    } else {
                        this.ai.format.references = selected;
                        this.ai.status = match this.save_minutes_format() {
                            Ok(()) => {
                                "已选择格式参考；点击生成时才会读取正文并发送到所选 AI 服务".into()
                            }
                            Err(e) => e.to_string(),
                        };
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
    pub(super) fn reset_minutes(&mut self) {
        self.ai_clock = task::TaskClock::default();
        self.ai.history = MinutesHistory::default();
        self.summary_text.clear();
        self.ai.stream.clear();
        self.ai.pending_request.clear();
        self.ai.editor_sync = true;
        self.ai.events = None;
        self.ai.chat_dirty = true;
        self.ai.chat_reset = true;
    }
    pub(super) fn load_minutes(&mut self) {
        if let Some(output) = &self.transcript_output {
            match MinutesHistory::load(output) {
                Ok(h) => {
                    self.summary_text = h.latest().to_string();
                    self.ai.history = h;
                    self.ai.editor_sync = true;
                    self.ai.chat_dirty = true;
                    self.ai.chat_reset = true;
                }
                Err(_) => {
                    self.ai.status = "已有纪要无法载入；文件保留，请检查保存目录".into();
                    self.reset_minutes();
                }
            }
        }
    }
    fn ai_credentials_config(&self, cx: &App) -> anyhow::Result<AiConfig> {
        // Credential identity depends on the service, even before a model is chosen.
        let config = AiConfig {
            provider: self.ai.config.provider,
            base_url: self.ai.url.read(cx).value().trim().into(),
            model: "credential-check".into(),
            background: false,
            deep_reasoning: false,
        };
        config.validate()?;
        Ok(config)
    }
    fn ai_config_from_inputs(&self, cx: &App) -> anyhow::Result<AiConfig> {
        let config = AiConfig {
            provider: self.ai.config.provider,
            base_url: self.ai.url.read(cx).value().trim().into(),
            model: self.ai.model.read(cx).value().trim().into(),
            background: self.ai.config.background,
            deep_reasoning: self.ai.config.deep_reasoning,
        };
        config.validate()?;
        Ok(config)
    }
    fn set_ai_preset(&mut self, config: AiConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.ai
            .url
            .update(cx, |s, cx| s.set_value(config.base_url.clone(), window, cx));
        self.ai
            .model
            .update(cx, |s, cx| s.set_value(config.model.clone(), window, cx));
        self.ai.key.update(cx, |s, cx| {
            s.set_masked(true, window, cx);
            s.set_value("", window, cx);
        });
        self.ai.config = config;
        self.ai.key_origin = None;
        self.ai.credential_for = None;
        self.ai.status = "已选服务预设；密钥留空可使用此前保存的密钥，填写新密钥后点击保存".into();
        cx.notify();
    }
    fn save_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai.credentials_busy {
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(c) => c,
            Err(e) => {
                self.ai.status = e.to_string();
                cx.notify();
                return;
            }
        };
        let save = (|| -> anyhow::Result<()> {
            if let Some(parent) = ai_path().parent() {
                std::fs::create_dir_all(parent)?;
            }
            opencap_asr::atomic_write(&ai_path(), &serde_json::to_vec_pretty(&config)?)
        })();
        if let Err(e) = save {
            self.ai.status = format!("配置保存失败：{e}");
            cx.notify();
            return;
        }
        self.ai.config = config.clone();
        let key = self.ai.key.read(cx).value().to_string();
        if key.trim().is_empty() {
            self.ai.status = "AI 配置已保存；保留已存密钥".into();
            cx.notify();
            return;
        }
        let task = cx.write_credentials(&config.credential_id(), "api-key", key.trim().as_bytes());
        self.ai.credentials_busy = true;
        self.ai
            .key
            .update(cx, |input, cx| input.set_masked(true, window, cx));
        self.ai.key_origin = Some(config.credential_id());
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.credentials_busy = false;
                this.ai.credential_status = if result.is_ok() {
                    "已保存密钥 · 可点眼睛查看".into()
                } else {
                    "密钥保存失败".into()
                };
                this.ai.credential_for = this.ai.key_origin.clone();
                this.ai.status = if result.is_ok() {
                    "配置和密钥已保存至系统凭据库".into()
                } else {
                    "配置已保存，但系统凭据库写入失败，请重新填写密钥".into()
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn request_minutes(&mut self, action: MinutesAction, cx: &mut Context<Self>) {
        if self.active()
            || self.transcription_busy()
            || self.ai.document_task.is_some()
            || !self.transcript.complete
        {
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(c) => c,
            Err(e) => {
                self.ai.status = e.to_string();
                self.settings = true;
                self.page_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
                return;
            }
        };
        if self.ai.editing {
            self.ai.status = "请先保存手动修改，再发送给 AI；这样会使用最新正文。".into();
            cx.notify();
            return;
        }
        let mut instruction = self.ai.instruction.read(cx).value().to_string();
        if action != MinutesAction::Generate && instruction.trim().is_empty() {
            self.ai.status = "请填写修改要求或问题".into();
            cx.notify();
            return;
        }
        if action == MinutesAction::Generate {
            let template = match self.minutes_style {
                1 => "阅读摘要：概括议题、主要结论和后续工作。",
                2 => {
                    "行动清单：明确决策、工作事项、责任部门或负责人、期限和待确认事项；未知信息不得编造。"
                }
                _ => {
                    "标准会议纪要正文：会议主题、按议题组织的讨论结论、会议议定事项、后续工作和待确认事项。非必要的缺失栏目省略。"
                }
            };
            instruction = format!("{template}\n用户补充要求：{instruction}");
        }
        self.ai.format.guidance = self.ai.format_guidance.read(cx).value().to_string();
        if let Err(e) = self.save_minutes_format() {
            self.ai.status = e.to_string();
            cx.notify();
            return;
        }
        let key = self.ai.key.read(cx).value().trim().to_string();
        if !key.is_empty() || (config.is_local() && config.provider == AiProvider::Ollama) {
            self.start_minutes(config, key, action, instruction);
            cx.notify();
            return;
        }
        self.ai.credentials_busy = true;
        self.ai.status = "正在读取系统凭据…".into();
        let task = cx.read_credentials(&config.credential_id());
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.credentials_busy = false;
                match result {
                    Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                        Ok(key) => this.start_minutes(config, key, action, instruction),
                        Err(_) => this.ai.status = "密钥格式无效，请重新保存".into(),
                    },
                    Ok(None) if config.is_local() => {
                        this.start_minutes(config, String::new(), action, instruction)
                    }
                    Ok(None) => {
                        this.ai.status = "尚未保存 API Key，请在 AI 配置中填写并保存".into();
                        this.settings = true;
                        this.page_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    Err(_) => this.ai.status = "系统凭据库读取失败，请重新填写并保存密钥".into(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn start_minutes(
        &mut self,
        config: AiConfig,
        key: String,
        action: MinutesAction,
        instruction: String,
    ) {
        let Some(output) = self.transcript_output.clone() else {
            return;
        };
        let transcript = self.transcript.clone();
        let history = self.ai.history.clone();
        let format = self.ai.format.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (progress_tx, progress_rx) = crossbeam_channel::bounded(256);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.ai.cancelled = cancelled.clone();
        self.ai.events = Some(progress_rx);
        self.ai.stream.clear();
        self.ai.pending_request = instruction.clone();
        self.ai.chat_dirty = true;
        self.ai_clock.start();
        self.summary_task = Some(rx);
        self.ai.status = format!("{} 正在处理；已有版本保留…", config.provider.label());
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::edit_minutes_with_events(
                &transcript,
                &config,
                &key,
                history,
                action,
                &instruction,
                &format,
                &output,
                &cancelled,
                |s| {
                    let _ = progress_tx.send(s);
                },
            ));
        });
    }
    pub(super) fn ai_settings_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.ai.credentials_busy;
        ui::card()
            .child(div().text_lg().child("AI 配置"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("deepseek-preset", "DeepSeek 官方 · 只填 Key", self.ai.config.is_deepseek())
                    .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.set_ai_preset(AiConfig::default(), window, cx))))
                .child(button("openai-preset", "OpenAI 官方", false).disabled(busy).on_click(cx.listener(|this, _, window, cx| {
                    this.set_ai_preset(AiConfig { provider: AiProvider::Responses,base_url:AiProvider::Responses.default_url().into(),model:String::new(),background:false,deep_reasoning:false }, window, cx);
                }))))
            .child(div().flex().flex_wrap().gap_2().children(AiProvider::ALL.into_iter().map(|p| {
                button(("ai-provider", p as usize), p.label(), self.ai.config.provider == p).disabled(busy).on_click(cx.listener(move |this, _, window, cx| {
                    if this.ai.config.provider == p { return; }
                    this.ai.config.provider = p; this.ai.config.background = false; this.ai.key_origin = None; this.ai.credential_for = None;
                    this.ai.url.update(cx, |s, cx| s.set_value(p.default_url(), window, cx));
                    this.ai.model.update(cx, |s, cx| s.set_value("", window, cx));
                    this.ai.key.update(cx, |s, cx| { s.set_masked(true, window, cx); s.set_value("", window, cx); });
                    this.ai.status = "请选择该服务的模型，填写密钥后保存".into(); cx.notify();
                }))
            })))
            .child(label("服务地址（填写根地址，自动追加协议路径）")).child(Input::new(&self.ai.url))
            .child(label("模型名")).child(Input::new(&self.ai.model))
            .child(label("API Key（保存到 macOS 钥匙串 / Windows 凭据库）")).child(Input::new(&self.ai.key).mask_toggle().disabled(busy))
            .child(label(self.ai.credential_status.clone()))
            .child(button("reload-ai-key", "载入已存密钥", false).ghost().disabled(busy).on_click(cx.listener(|this, _, _, cx| this.load_ai_key(cx))))
            .when(self.ai.config.provider == AiProvider::Responses && !self.ai.config.is_deepseek(), |d| d.child(
                button("responses-background", if self.ai.config.background { "服务端后台响应：开启" } else { "服务端后台响应：关闭" }, self.ai.config.background)
                    .on_click(cx.listener(|this, _, _, cx| { this.ai.config.background = !this.ai.config.background; cx.notify(); }))
            ))
            .child(div().flex().flex_wrap().gap_2()
                .child(primary_button("save-ai-config", "保存 AI 配置").disabled(busy).on_click(cx.listener(|this, _, window, cx| this.save_ai(window, cx))))
                .child(button("delete-ai-key", "删除已存密钥", false).disabled(busy).on_click(cx.listener(|this, _, window, cx| {
                    let config = match this.ai_config_from_inputs(cx) { Ok(c) => c, Err(e) => { this.ai.status = e.to_string(); cx.notify(); return; } };
                    let task = cx.delete_credentials(&config.credential_id()); this.ai.credentials_busy = true;
                    this.ai.key.update(cx, |s, cx| { s.set_masked(true, window, cx); s.set_value("", window, cx); });
                    cx.spawn(async move |entity, cx| { let result = task.await; let _ = entity.update(cx, |this, cx| { this.ai.credentials_busy = false; this.ai.credential_status = if result.is_ok() { "未保存密钥".into() } else { "密钥删除失败".into() }; this.ai.status = if result.is_ok() { "已删除当前服务的密钥".into() } else { "密钥删除失败或尚未保存".into() }; cx.notify(); }); }).detach();
                }))))
            .child(label("选择远程服务后，点击生成或提问会发送转录文字；音视频仍留在本机。后台响应要求服务支持 Responses background，并会在服务端暂存响应。"))
            .when(self.ai.config.is_deepseek(), |d| d.child(label("DeepSeek 使用应用后台任务；官方不支持服务端 background。密钥保存到系统凭据库，关闭和重启应用不会丢失。")))
            .when(self.ai.config.is_deepseek(), |d| d.child(button("ai-reasoning-mode", if self.ai.config.deep_reasoning { "深度思考：开启" } else { "普通整理：优先速度" }, self.ai.config.deep_reasoning).disabled(busy).on_click(cx.listener(|this, _, _, cx| {
                this.ai.config.deep_reasoning = !this.ai.config.deep_reasoning; cx.notify();
            }))))
            .child(label(self.ai.status.clone()))
            .when(self.ai_clock.has_run() || self.ai.history.versions.last().and_then(|v| v.elapsed_seconds).is_some(), |d| {
                let elapsed = if self.ai_clock.has_run() { self.ai_clock.elapsed() } else { task::saved_duration(self.ai.history.versions.last().and_then(|v| v.elapsed_seconds)) };
                d.child(label(format!("AI 已用时 {}", task::duration_text(elapsed))))
            })
    }
    fn minutes_format_options(&self, busy: bool, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(label("公司内容格式（可选）"))
            .child(Textarea::new(&self.ai.format_guidance).disabled(busy))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("choose-format-references", "添加历史文件", false)
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.choose_format_references(cx)),
                            ),
                    )
                    .when(!self.ai.format.references.is_empty(), |d| {
                        d.child(
                            button("clear-format-references", "清空参考", false)
                                .ghost()
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.ai.format.references.clear();
                                    let _ = this.save_minutes_format();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(label(
                "可选 1–5 个 Word / Markdown / TXT；只参考内容结构和措辞。",
            ))
            .children(self.ai.format.references.iter().map(|p| {
                label(
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
                .truncate()
            }))
            .child(
                button(
                    "minutes-evidence",
                    if self.ai.format.include_evidence {
                        "附来源与索引：开启"
                    } else {
                        "仅输出正文"
                    },
                    !self.ai.format.include_evidence,
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ai.format.include_evidence = !this.ai.format.include_evidence;
                    let _ = this.save_minutes_format();
                    cx.notify();
                })),
            )
    }
    pub(super) fn minutes_view(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let ready = self.transcript.complete && !self.transcript.text().is_empty();
        let has_minutes = !self.ai.history.latest().is_empty();
        let mut toolbar = div().flex().flex_col().gap_3();
        if let Some(output) = &self.transcript_output {
            let path = output.with_extension("summary.docx");
            let markdown_path = output.with_extension("summary.md");
            let word_base = output.clone();
            let folder = output
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf();
            toolbar = toolbar.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("open-minutes", "打开 Word 文档", false)
                            .disabled(!path.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&path);
                            }),
                    )
                    .child(
                        button("export-word", "导出纪要 Word…", false)
                            .disabled(busy || !has_minutes)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let path = word_base.with_extension("summary.docx");
                                if !path.exists()
                                    && let Err(e) = opencap_asr::export_minutes_docx(
                                        this.ai.history.latest(),
                                        &path,
                                    )
                                {
                                    this.ai.status = format!("Word 导出失败：{e}");
                                    cx.notify();
                                    return;
                                }
                                this.export_document(path, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("open-minutes-markdown", "打开纪要 Markdown", false)
                            .disabled(!markdown_path.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&markdown_path);
                            }),
                    )
                    .child(button("minutes-history", "会议文件夹", false).on_click(
                        move |_, _, _| {
                            let _ = opencap_platform::open_path(&folder);
                        },
                    )),
            );
        }

        let editor = ui::card()
            .gap_2()
            .min_w(px(0.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(ui::section_title("与 AI 一起改稿", ""))
                    .child(
                        button("open-ai-config", "配置", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings = true;
                                this.settings_page = settings::SettingsPage::Ai;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    ),
            )
            .child(label(format!(
                "{} · {}",
                self.ai.config.provider.label(),
                if self.ai.config.model.is_empty() {
                    "未配置模型"
                } else {
                    &self.ai.config.model
                }
            )))
            .child(
                div()
                    .h(px((height - 740.).clamp(110., 300.)))
                    .w_full()
                    .child(self.ai.chat.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("conversation-revise-mode", "修改正文", !self.ai.ask_only)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai.ask_only = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("conversation-ask-mode", "只问问题", self.ai.ask_only)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai.ask_only = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(Textarea::new(&self.ai.instruction).disabled(busy).w_full())
            .when(self.summary_task.is_none(), |d| {
                d.child(
                    primary_button(
                        "send-minutes-message",
                        if self.ai.ask_only {
                            "发送问题"
                        } else if has_minutes {
                            "发送修改要求"
                        } else {
                            "生成会议纪要"
                        },
                    )
                    .w_full()
                    .disabled(busy || !ready || self.ai.editing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let action = if this.ai.ask_only {
                            MinutesAction::Ask
                        } else if this.ai.history.latest().is_empty() {
                            MinutesAction::Generate
                        } else {
                            MinutesAction::Revise
                        };
                        this.request_minutes(action, cx);
                    })),
                )
            })
            .when(self.summary_task.is_some(), |d| {
                d.child(
                    button("cancel-summary", "停止生成", false)
                        .danger()
                        .w_full()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.cancelled.store(true, Ordering::Relaxed);
                            this.ai.status = "正在停止生成；已有版本保留…".into();
                            cx.notify();
                        })),
                )
            })
            .child(label(self.ai.status.clone()))
            .when(
                self.ai_clock.has_run()
                    || self
                        .ai
                        .history
                        .versions
                        .last()
                        .and_then(|v| v.elapsed_seconds)
                        .is_some(),
                |d| {
                    d.child(label(format!(
                        "AI 已用时 {}",
                        task::duration_text(if self.ai_clock.has_run() {
                            self.ai_clock.elapsed()
                        } else {
                            task::saved_duration(
                                self.ai
                                    .history
                                    .versions
                                    .last()
                                    .and_then(|v| v.elapsed_seconds),
                            )
                        })
                    )))
                },
            )
            .child(
                button(
                    "minutes-format-settings",
                    format!(
                        "模板与内容格式 · {} 个参考文件",
                        self.ai.format.references.len()
                    ),
                    self.ai.format_expanded,
                )
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ai.format_expanded = !this.ai.format_expanded;
                    this.ai.format.guidance = this.ai.format_guidance.read(cx).value().to_string();
                    if let Err(e) = this.save_minutes_format() {
                        this.ai.status = e.to_string();
                    }
                    cx.notify();
                })),
            )
            .when(self.ai.format_expanded, |d| {
                d.child(
                    div().flex().flex_wrap().gap_2().children(
                        ["正式纪要", "阅读摘要", "行动清单"]
                            .into_iter()
                            .enumerate()
                            .map(|(i, title)| {
                                button(("minutes-style", i), title, self.minutes_style == i)
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.minutes_style = i;
                                        cx.notify();
                                    }))
                            }),
                    ),
                )
                .child(self.minutes_format_options(busy, cx))
                .when(has_minutes, |d| {
                    d.child(
                        button("regenerate-minutes", "按模板重新生成", false)
                            .disabled(busy || !ready || self.ai.editing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.request_minutes(MinutesAction::Generate, cx)
                            })),
                    )
                })
            })
            .child(label("修改成功后自动保存正文和 Word，旧版本保留。"));
        let mut document = ui::card()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(440.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "会议纪要正文",
                        if self.ai.editing {
                            "手动修改中 · 保存后可继续与 AI 对话"
                        } else {
                            "每轮修改完成后同步到这里"
                        },
                    ))
                    .child(ui::badge(
                        format!("{} 个版本", self.ai.history.versions.len()),
                        MUTED,
                        BG,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "edit-minutes-body",
                            if self.ai.editing {
                                "放弃本次手动修改"
                            } else if has_minutes {
                                "手动编辑"
                            } else {
                                "自己编写纪要"
                            },
                            self.ai.editing,
                        )
                        .disabled(busy || self.transcript_output.is_none())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.ai.editor.update(cx, |s, cx| {
                                s.set_value(this.summary_text.clone(), window, cx)
                            });
                            if this.ai.editing {
                                if let Some(base) = &this.ai.editor_base {
                                    let _ = std::fs::remove_file(
                                        base.with_extension("summary.draft.md"),
                                    );
                                }
                                this.ai.draft_saved.clear();
                            }
                            this.ai.editing = !this.ai.editing;
                            cx.notify();
                        })),
                    )
                    .when(self.ai.editing, |d| {
                        d.child(
                            primary_button("save-minutes-body", "保存正文与 Word")
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_manual_minutes(cx)),
                                ),
                        )
                    })
                    .child(
                        button("sync-minutes-file", "同步文件修改…", false)
                            .disabled(busy || self.ai.editing || self.transcript_output.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.import_minutes_file(cx))),
                    )
                    .child(
                        button(
                            "toggle-minutes-versions",
                            "修订历史",
                            self.ai.history_expanded,
                        )
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.history_expanded = !this.ai.history_expanded;
                            cx.notify();
                        })),
                    ),
            );
        if self.ai.editing {
            document = document.child(Textarea::new(&self.ai.editor).disabled(busy).w_full());
        } else {
            let streaming =
                self.summary_task.is_some() && !self.ai.ask_only && !self.ai.stream.is_empty();
            let text = if streaming {
                self.ai.stream.clone()
            } else {
                self.summary_text.clone()
            };
            if streaming {
                document = document.child(label("正在生成预览 · 完整完成后自动保存"));
            }
            document = document.child(div().id("minutes-document-scroll").max_h(px(if ui::two_columns(width) { 660. } else { 420. })).overflow_y_scroll()
                .when(text.is_empty(), |d| d.child(div().py_8().flex().flex_col().gap_3()
                    .child(div().text_lg().child("从一份会议纪要开始"))
                    .child(label("在对话框中生成初稿，或自己编写正文。可以给修改建议、添加历史样式，直到内容满意。"))))
                .when(!text.is_empty(), |d| d.child(TextView::markdown("meeting-summary", text))));
        }
        if self.ai.history_expanded {
            let mut versions = div()
                .id("minutes-versions-scroll")
                .max_h(px(220.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_2();
            for version in self.ai.history.versions.iter().rev() {
                let markdown = version.markdown.clone();
                versions = versions.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            label(format!("v{} · {}", version.number, version.instruction))
                                .flex_1(),
                        )
                        .child(
                            button(
                                ("restore-minutes-version", version.number),
                                "恢复此版",
                                false,
                            )
                            .disabled(
                                busy || self.ai.editing || version.markdown == self.summary_text,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.ai.editor.update(cx, |s, cx| {
                                        s.set_value(markdown.clone(), window, cx)
                                    });
                                    this.ai.editing = true;
                                    this.save_manual_minutes(cx);
                                },
                            )),
                        ),
                );
            }
            document = document.child(versions);
        }
        let mut body = div().flex().items_start().gap_5().w_full();
        if ui::two_columns(width) {
            body = body
                .child(document)
                .child(editor.w(px(370.)).flex_shrink_0());
        } else {
            body = body
                .flex_col()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("minutes-compact-chat", "对话修改", self.ai.conversation_tab)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.ai.conversation_tab = true;
                                    cx.notify();
                                })),
                        )
                        .child(
                            button(
                                "minutes-compact-body",
                                "查看正文",
                                !self.ai.conversation_tab,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai.conversation_tab = false;
                                cx.notify();
                            })),
                        ),
                )
                .when(self.ai.conversation_tab, |d| d.child(editor.w_full()))
                .when(!self.ai.conversation_tab, |d| d.child(document.w_full()));
        }
        div().flex().flex_col().gap_4().child(toolbar).child(body)
    }
}
