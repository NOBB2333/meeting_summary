mod artifact;
#[cfg(feature = "ui-smoke")]
mod checks;
mod correction;
#[path = "ai/settings.rs"]
mod global_settings;
mod view;
use super::*;
#[cfg(feature = "ui-smoke")]
pub(super) use checks::chat_benchmark;
use gpui::component::{
    input::{Input, InputEvent, Textarea, TextareaState},
    resizable::{h_resizable, resizable_panel},
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
    #[cfg(feature = "ui-smoke")]
    send_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    pub config: AiConfig,
    url: Entity<InputState>,
    model: Entity<InputState>,
    key: Entity<InputState>,
    pub(super) instruction: Entity<TextareaState>,
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
    submitted_composer: String,
    pub correction_task:
        Option<crossbeam_channel::Receiver<anyhow::Result<opencap_asr::Transcript>>>,
    ask_only: bool,
    document_tools_open: bool,
    conversation_tab: bool,
    artifact_open: bool,
    artifact_version: Option<u64>,
    stream_artifact: bool,
    file_preview: Option<(String, Entity<text_document::TextDocument>)>,
    format: MinutesFormat,
    format_guidance: Entity<TextareaState>,
    terminology: Entity<TextareaState>,
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
        let chat = cx.new(|_| ai_chat::ChatView::new());
        cx.subscribe(&chat, |this, _, event, cx| {
            let ai_chat::ChatEvent::OpenArtifact(number) = event;
            this.ai.artifact_version = (*number != 0).then_some(*number);
            this.ai.file_preview = None;
            this.ai.artifact_open = true;
            this.ai.document_tools_open = false;
            this.ai.conversation_tab = false;
            cx.notify();
        })
        .detach();
        let instruction = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("输入要求，继续完善会议纪要…")
                .submit_on_enter(true)
                .auto_grow(2, 4)
        });
        cx.subscribe(&instruction, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                let action = if this.ai.ask_only {
                    MinutesAction::Ask
                } else if this.ai.history.latest().is_empty() {
                    MinutesAction::Generate
                } else {
                    MinutesAction::Revise
                };
                this.request_minutes(action, cx);
            }
        })
        .detach();
        Self {
            #[cfg(feature = "ui-smoke")]
            send_bounds: Rc::new(std::cell::Cell::new(None)),
            terminology: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("每行一条，例如：\n欧盆盖 => OpenCap\n统信 UOS\n张伟")
                    .default_value(format.terminology.clone())
                    .auto_grow(3, 6)
            }),
            format_guidance: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("例如：按议题分段，使用‘会议议定’，最后列责任部门；只输出正文。")
                    .default_value(format.guidance.clone())
                    .auto_grow(2, 6)
            }),
            format,
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
            instruction,
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
            chat,
            chat_dirty: true,
            chat_reset: true,
            clear_composer: false,
            submitted_composer: String::new(),
            correction_task: None,
            ask_only: false,
            document_tools_open: false,
            conversation_tab: true,
            artifact_open: true,
            artifact_version: None,
            stream_artifact: false,
            file_preview: None,
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
            if self.ai.instruction.read(cx).value().as_str() == self.ai.submitted_composer {
                self.ai
                    .instruction
                    .update(cx, |s, cx| s.set_value("", window, cx));
            }
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
                    ai_chat::ReplyState {
                        busy: self.summary_task.is_some(),
                        reset: self.ai.chat_reset,
                        artifact: self.ai.stream_artifact,
                    },
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
                self.ai.artifact_version = None;
                self.ai.file_preview = None;
                self.ai.chat_dirty = true;
                self.ai.status = "正文、Word 与修订版本已同步；可继续对话修改。".into();
                self.remember_meeting();
            }
            Err(e) => self.ai.status = format!("同步失败；编辑内容和已有版本保留：{e:#}"),
        }
        true
    }
    fn save_minutes_format(&self) -> anyhow::Result<()> {
        self.ai.format.terminology_prompt()?;
        anyhow::ensure!(
            self.ai.format.guidance.chars().count() <= 4000,
            "内容格式要求最多 4000 字"
        );
        anyhow::ensure!(
            self.ai.format.references.len() <= 5,
            "最多选择 5 个历史格式文件"
        );
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
        self.ai.artifact_version = None;
        self.ai.document_tools_open = false;
        self.ai.stream_artifact = false;
        self.ai.file_preview = None;
        self.ai.conversation_tab = true;
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
                    if let Ok(bytes) = std::fs::read(output.with_extension("summary.request.json"))
                        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    {
                        let request = value["request"].as_str().unwrap_or_default();
                        if !self
                            .ai
                            .history
                            .conversation
                            .last()
                            .is_some_and(|turn| turn.request == request)
                        {
                            self.ai.pending_request = request.into();
                            self.ai.stream_artifact = value["artifact"].as_bool().unwrap_or(false);
                            self.ai.status = "上次发送尚未完成，要求已保留；可重试本轮。".into();
                        }
                    }
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
        self.request_minutes_with_instruction(action, None, cx);
    }
    fn request_minutes_with_instruction(
        &mut self,
        action: MinutesAction,
        instruction: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.active()
            || self.summary_task.is_some()
            || self.ai.credentials_busy
            || self.transcription_busy()
            || self.ai.document_task.is_some()
            || !self.transcript.complete
        {
            self.ai.status = "当前任务尚未完成或转录未就绪，输入内容保留，请稍后发送。".into();
            cx.notify();
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(c) => c,
            Err(e) => {
                self.ai.status = e.to_string();
                self.settings = true;
                self.settings_page = settings::SettingsPage::Ai;
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
        self.ai.submitted_composer = self.ai.instruction.read(cx).value().to_string();
        let mut instruction =
            instruction.unwrap_or_else(|| self.ai.instruction.read(cx).value().to_string());
        if action != MinutesAction::Generate && instruction.trim().is_empty() {
            self.ai.status = "请填写修改要求或问题".into();
            cx.notify();
            return;
        }
        if action == MinutesAction::Generate {
            let template = self.ai.format.style.instruction();
            instruction = format!("{template}\n用户补充要求：{instruction}");
        }
        self.ai.format.guidance = self.ai.format_guidance.read(cx).value().to_string();
        self.ai.format.terminology = self.ai.terminology.read(cx).value().to_string();
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
                        this.settings_page = settings::SettingsPage::Ai;
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
            self.ai.status = "会议保存位置不可用，未发送；输入内容保留".into();
            return;
        };
        let pending = serde_json::json!({"request":instruction,"artifact":action != MinutesAction::Ask,"draft":self.ai.submitted_composer});
        if let Err(e) = opencap_asr::atomic_write(
            &output.with_extension("summary.request.json"),
            pending.to_string().as_bytes(),
        ) {
            self.ai.status = format!("无法保存本轮请求，尚未发送：{e}");
            return;
        }
        self.ai.clear_composer = true;
        self.ai.conversation_tab = true;
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
        self.ai.stream_artifact = action != MinutesAction::Ask;
        if self.ai.stream_artifact {
            self.ai.document_tools_open = false;
            self.ai.artifact_open = true;
            self.ai.artifact_version = None;
            self.ai.file_preview = None;
        }
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
}
