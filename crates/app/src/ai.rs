mod artifact;
#[cfg(feature = "ui-smoke")]
mod checks;
mod correction;
#[path = "ai/settings.rs"]
mod global_settings;
mod models;
mod view;
use super::*;
#[cfg(feature = "ui-smoke")]
pub(super) use checks::{chat_benchmark, correction_benchmark};
use gpui::component::{
    input::{Input, InputEvent, Textarea, TextareaState},
    resizable::resizable_panel,
    text::TextView,
};
use opencap_asr::{
    AiConfig, AiProvider, MinutesAction, MinutesEvent, MinutesFormat, MinutesHistory,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn read_minutes_draft(base: &std::path::Path) -> anyhow::Result<Option<String>> {
    use std::io::Read;
    const LIMIT: u64 = 2_000_000;
    let file = match std::fs::File::open(base.with_extension("summary.draft.md")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(file.metadata()?.is_file(), "纪要草稿不是普通文件");
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "纪要草稿超过 2 MB");
    Ok(Some(String::from_utf8(bytes)?))
}

/// Load a current or legacy ai.json. Removed protocols are mapped to a
/// supported one with a visible notice; the original bytes are kept as a
/// one-time backup and the migrated file is written atomically.
fn load_ai_config_migrating(path: &std::path::Path) -> anyhow::Result<(AiConfig, String)> {
    let bytes = std::fs::read(path)?;
    if let Ok(config) = serde_json::from_slice::<AiConfig>(&bytes) {
        return Ok((
            config,
            "AI 配置已载入；密钥仅在使用时从系统凭据库读取".into(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let provider = value
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("Anthropic");
    let (provider, notice) = match provider {
        "Anthropic" => (AiProvider::Anthropic, None),
        "Responses" => (AiProvider::Responses, None),
        "ChatCompletions" => (
            AiProvider::Responses,
            Some("旧 Chat Completions 配置已改为 OpenAI Responses；请确认服务支持 /responses"),
        ),
        "Ollama" => (
            AiProvider::Responses,
            Some("旧 Ollama 配置已移除；请填写支持 Responses 的服务根地址"),
        ),
        _other => (
            AiProvider::Anthropic,
            Some("未知旧协议已改为 Anthropic Messages；请核对服务地址"),
        ),
    };
    let base_url = value
        .get("base_url")
        .and_then(|value| value.as_str())
        .unwrap_or(AiProvider::Anthropic.default_url())
        .to_owned();
    let model = value
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    let config = AiConfig {
        provider,
        base_url,
        model,
    };
    let backup = path.with_extension("json.bak-v3");
    if !backup.exists() {
        let _ = std::fs::write(&backup, &bytes);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&config) {
        let _ = opencap_asr::atomic_write(path, &json);
    }
    let message = notice
        .map(|notice| format!("{notice}；旧配置已备份为 ai.json.bak-v3"))
        .unwrap_or_else(|| "AI 配置已迁移并载入".into());
    Ok((config, message))
}

type ModelFetch = (
    AiConfig,
    crossbeam_channel::Receiver<Result<Vec<String>, String>>,
);

pub(super) struct AiWorkspace {
    #[cfg(feature = "ui-smoke")]
    send_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    #[cfg(feature = "ui-smoke")]
    pub(super) correction_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
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
    draft_exists: bool,
    draft_recovery: Option<String>,
    pub(super) draft_recovery_busy: bool,
    pub editor_sync: bool,
    pub document_task: Option<crossbeam_channel::Receiver<anyhow::Result<MinutesHistory>>>,
    document_task_base: Option<PathBuf>,
    pub(super) minutes_reload_busy: bool,
    credential_for: Option<String>,
    key_origin: Option<String>,
    credential_status: String,
    pending_key: Option<(String, String)>,
    model_list_open: bool,
    model_list: Vec<String>,
    model_list_status: String,
    model_list_service: Option<(AiProvider, String)>,
    model_fetch: Option<ModelFetch>,
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
    pub config_status: String,
    pub confirm_delete_key: bool,
    pub format_advanced_open: bool,
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
    pub(super) fn relocate_meeting(&mut self, old: &std::path::Path, new: &std::path::Path) {
        if self.editor_base.as_deref() == Some(old) {
            self.editor_base = Some(new.to_path_buf());
            self.file_preview = None;
        }
    }
    pub fn new(window: &mut Window, cx: &mut Context<Recorder>) -> Self {
        let (config, status) = if ai_path().exists() {
            match load_ai_config_migrating(&ai_path()) {
                Ok((config, notice)) => (config, notice),
                Err(_) => (AiConfig::default(), "AI 配置读取失败，请重新保存".into()),
            }
        } else {
            (
                AiConfig::default(),
                "默认 Anthropic Messages；填写兼容服务地址、模型和 API Key 后保存".into(),
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
            #[cfg(feature = "ui-smoke")]
            correction_bounds: Rc::new(std::cell::Cell::new(None)),
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
            draft_exists: false,
            draft_recovery: None,
            draft_recovery_busy: false,
            editor_sync: true,
            document_task: None,
            document_task_base: None,
            minutes_reload_busy: false,
            credential_for: None,
            key_origin: None,
            credential_status: "尚未检查密钥".into(),
            pending_key: None,
            model_list_open: false,
            model_list: Vec::new(),
            model_list_status: String::new(),
            model_list_service: None,
            model_fetch: None,
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
            config_status: String::new(),
            confirm_delete_key: false,
            format_advanced_open: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: None,
        }
    }
}
impl Recorder {
    pub(super) fn sync_ai_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ai.chat.update(cx, |chat, cx| {
            chat.set_compact(f32::from(window.viewport_size().height) < 620., cx);
        });
        if self
            .ai
            .model_list_service
            .as_ref()
            .is_some_and(|(provider, url)| {
                *provider != self.ai.config.provider || url != self.ai.url.read(cx).value().trim()
            })
        {
            self.ai.model_list_open = false;
            self.ai.model_list.clear();
            self.ai.model_fetch = None;
            self.ai.model_list_service = None;
        }
        if self.ai.editor_base != self.transcript_output {
            self.persist_minutes_draft(cx);
            self.ai.editing = false;
            self.ai.editor_base = self.transcript_output.clone();
            self.ai.draft_recovery = None;
            self.ai.editor_sync = true;
        }
        if self.ai.editor_sync {
            let draft = self
                .ai
                .editor_base
                .as_ref()
                .map(|base| read_minutes_draft(base));
            let draft = match draft {
                Some(Ok(value)) => value,
                Some(Err(error)) => {
                    self.ai.status = format!("无法读取纪要草稿；原文件保留：{error:#}");
                    None
                }
                None => None,
            };
            self.ai.draft_exists = draft.is_some();
            self.ai.draft_saved = draft.clone().unwrap_or_default();
            self.ai.editing = draft.as_ref().is_some_and(|text| !text.is_empty());
            let text = draft
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| self.summary_text.clone());
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
        if self.ai.draft_recovery.as_deref() == Some(text.as_str()) {
            return false;
        }
        if text == self.ai.draft_saved {
            return false;
        }
        let result = opencap_core::managed_meeting_activity(base).and_then(|_activity| {
            let _guard = opencap_core::meeting_metadata_guard(base)?;
            let expected = self.ai.draft_exists.then_some(self.ai.draft_saved.as_str());
            let current = read_minutes_draft(base)?;
            anyhow::ensure!(
                current.as_deref() == expected,
                "纪要草稿已被其他操作修改；当前输入仍在编辑框中"
            );
            opencap_asr::atomic_write(&base.with_extension("summary.draft.md"), text.as_bytes())
        });
        match result {
            Ok(()) => {
                self.ai.draft_saved = text;
                self.ai.draft_exists = true;
                self.ai.draft_recovery = None;
            }
            Err(error) => {
                self.ai.status = format!("草稿暂未保存：{error:#}");
                return true;
            }
        }
        false
    }
    fn remove_minutes_draft_if_unchanged(&mut self, base: &std::path::Path) -> anyhow::Result<()> {
        let _activity = opencap_core::managed_meeting_activity(base)?;
        let _guard = opencap_core::meeting_metadata_guard(base)?;
        let current = read_minutes_draft(base)?;
        let expected = self.ai.draft_exists.then_some(self.ai.draft_saved.as_str());
        anyhow::ensure!(
            current.as_deref() == expected,
            "另一处的纪要草稿已变化，未删除"
        );
        if current.is_some() {
            std::fs::remove_file(base.with_extension("summary.draft.md"))?;
            self.ai.draft_saved.clear();
            self.ai.draft_exists = false;
            opencap_core::sync_parent_directory(&base.with_extension("summary.draft.md"))?;
        }
        self.ai.draft_saved.clear();
        self.ai.draft_exists = false;
        self.ai.draft_recovery = None;
        Ok(())
    }
    fn save_minutes_draft_recovery(&mut self, cx: &mut Context<Self>) {
        if !self.ai.editing || self.active() || self.minutes_task_busy() || self.quit_pending {
            return;
        }
        let Some(base) = self.ai.editor_base.clone() else {
            return;
        };
        let text = self.ai.editor.read(cx).value().to_string();
        if text.len() > 2_000_000 {
            self.ai.status = "当前编辑超过 2 MB，无法保存恢复副本。".into();
            cx.notify();
            return;
        }
        self.ai.draft_recovery_busy = true;
        self.ai.status = "正在另存编辑恢复副本…".into();
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let task_text = text.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _activity = opencap_core::managed_meeting_activity(&task_base)?;
                    let path = task_base.with_extension(format!(
                        "summary.recovery-{}.json",
                        opencap_core::new_stable_id()
                    ));
                    let payload =
                        serde_json::json!({"kind": "manual_minutes_draft", "text": task_text});
                    opencap_core::atomic_create_bytes(
                        &path,
                        &serde_json::to_vec_pretty(&payload)?,
                    )?;
                    let warning = path.file_name().and_then(|name| {
                        opencap_core::register_meeting_asset(
                            &task_base,
                            "minutes_recovery",
                            opencap_core::AssetOwnership::Managed,
                            std::path::Path::new(name),
                        )
                        .err()
                        .map(|error| format!("资产登记失败：{error:#}"))
                    });
                    Ok::<_, anyhow::Error>((path, warning))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.draft_recovery_busy = false;
                match result {
                    Ok((path, warning)) => {
                        if this.ai.editing
                            && this.ai.editor_base.as_ref() == Some(&base)
                            && this.ai.editor.read(cx).value().as_str() == text
                        {
                            this.ai.draft_recovery = Some(text);
                        }
                        let warning = warning
                            .map(|value| format!("；{value}"))
                            .unwrap_or_default();
                        this.ai.status = format!("编辑已另存恢复副本：{}{warning}", path.display());
                    }
                    Err(error) => {
                        this.ai.status = format!("恢复副本保存失败，编辑仍在窗口中：{error:#}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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
        // Try the normalized service identity first, then legacy provider+URL
        // identities so a protocol/URL migration does not lose the key.
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
        if self.active() || self.minutes_task_busy() || self.quit_pending {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let history = self.ai.history.clone();
        let markdown = self.ai.editor.read(cx).value().to_string();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.document_task = Some(rx);
        self.ai.document_task_base = Some(base.clone());
        self.ai.status = "正在保存正文，再生成导出文件…".into();
        if let Err(error) = std::thread::Builder::new()
            .name("opencap-minutes-save".into())
            .spawn(move || {
                let _ = tx.send(history.save_revision(&base, &markdown, "手动编辑正文"));
            })
        {
            self.ai.document_task = None;
            self.ai.document_task_base = None;
            self.ai.status = format!("无法启动保存任务，编辑内容保留，可重试：{error}");
        }
        cx.notify();
    }
    fn retry_minutes_exports(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.minutes_task_busy() || self.quit_pending || self.ai.editing {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let history = self.ai.history.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.document_task = Some(rx);
        self.ai.document_task_base = Some(base.clone());
        self.ai.status = "正在根据已保存正文重试导出…".into();
        if let Err(error) = std::thread::Builder::new()
            .name("opencap-minutes-export".into())
            .spawn(move || {
                let _ = tx.send(history.retry_exports(&base));
            })
        {
            self.ai.document_task = None;
            self.ai.document_task_base = None;
            self.ai.status = format!("无法启动导出任务，已保存正文保留，可重试：{error}");
        }
        cx.notify();
    }
    fn backup_word_and_retry_exports(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.minutes_task_busy() || self.quit_pending || self.ai.editing {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let history = self.ai.history.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.ai.document_task = Some(rx);
        self.ai.document_task_base = Some(base.clone());
        self.ai.status = "正在备份现有 Word，再重试导出已保存正文…".into();
        if let Err(error) = std::thread::Builder::new()
            .name("opencap-minutes-backup".into())
            .spawn(move || {
                let _ = tx.send(history.backup_word_and_retry(&base));
            })
        {
            self.ai.document_task = None;
            self.ai.document_task_base = None;
            self.ai.status = format!("无法启动备份任务，现有文件保留，可重试：{error}");
        }
        cx.notify();
    }
    fn reload_minutes_history(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.minutes_task_busy() || self.quit_pending {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        if self.persist_minutes_draft(cx) {
            self.ai.status = "当前编辑草稿尚未保存，未重新载入。".into();
            cx.notify();
            return;
        }
        let baseline = match serde_json::to_vec(&self.ai.history) {
            Ok(value) => value,
            Err(error) => {
                self.ai.status = format!("无法核对当前纪要版本：{error}");
                cx.notify();
                return;
            }
        };
        self.ai.minutes_reload_busy = true;
        self.ai.status = "正在后台读取已保存的纪要历史…".into();
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _activity = opencap_core::managed_meeting_activity(&task_base)?;
                    let _guard = opencap_core::meeting_metadata_guard(&task_base)?;
                    MinutesHistory::load(&task_base)
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.minutes_reload_busy = false;
                if this.quit_pending
                    || this.active()
                    || this.minutes_task_busy()
                    || this.transcript_output.as_ref() != Some(&base)
                    || !serde_json::to_vec(&this.ai.history).is_ok_and(|value| value == baseline)
                {
                    this.ai.status = "会议或纪要状态已变化，未应用重新载入的历史。".into();
                } else {
                    match result {
                        Ok(history) => {
                            this.summary_text = history.latest().into();
                            this.ai.history = history;
                            this.ai.artifact_version = None;
                            this.ai.file_preview = None;
                            this.ai.chat_dirty = true;
                            this.ai.chat_reset = true;
                            if this.ai.editing {
                                // Keep the active editor and its draft exactly as they are.
                                this.ai.editor_sync = false;
                                this.ai.status =
                                    "已载入最新纪要历史；当前编辑和草稿保留，请核对后再保存。"
                                        .into();
                            } else {
                                this.ai.editor_sync = true;
                                this.ai.status =
                                    "已重新载入纪要历史；若文件再次变化，保存时会重新检查。".into();
                            }
                        }
                        Err(error) => {
                            this.ai.status = format!("无法重新载入纪要，当前内容保留：{error:#}")
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn import_minutes_file(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.minutes_task_busy() || self.quit_pending || self.ai.editing {
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
        cx.spawn(async move |entity, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = entity.update(cx, |this, cx| {
                if this.active()
                    || this.minutes_task_busy()
                    || this.quit_pending
                    || this.transcript_output.as_ref() != Some(&base)
                    || this.ai.editing
                {
                    return;
                }
                let history = this.ai.history.clone();
                let (tx, rx) = crossbeam_channel::bounded(1);
                this.ai.document_task = Some(rx);
                this.ai.document_task_base = Some(base.clone());
                this.ai.status = "正在读取修改后的正文并保存版本…".into();
                if let Err(error) = std::thread::Builder::new()
                    .name("opencap-minutes-import".into())
                    .spawn(move || {
                        let result =
                            opencap_asr::read_minutes_document(&path).and_then(|markdown| {
                                history.save_revision(&base, &markdown, "同步外部文件修改")
                            });
                        let _ = tx.send(result);
                    })
                {
                    this.ai.document_task = None;
                    this.ai.document_task_base = None;
                    this.ai.status = format!("无法启动导入任务，已有正文保留，可重试：{error}");
                }
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
        let task_base = self.ai.document_task_base.take();
        if task_base.as_ref() != self.transcript_output.as_ref() {
            if result.is_ok()
                && let Some(base) = task_base
            {
                self.meeting_scan_extra.insert(base);
                self.notes_index_pending = Some(std::time::Instant::now());
            }
            self.ai.status = "先前会议的文档任务已结束；当前会议内容未更改。".into();
            return true;
        }
        match result {
            Ok(history) => {
                let mut status = history.export_warning.clone().unwrap_or_else(|| {
                    if history.exports_pending() {
                        "正文已保存；派生文件可在文件与版本中重试导出。".into()
                    } else {
                        "正文和导出文件已保存；可继续对话修改。".into()
                    }
                });
                if let Some(path) = self
                    .transcript_output
                    .as_deref()
                    .and_then(|base| history.last_word_backup_path(base))
                {
                    status.push_str(&format!("；最近 Word 备份：{}", path.display()));
                }
                self.summary_text = history.latest().into();
                self.ai.history = history;
                let cleanup_error = self
                    .transcript_output
                    .clone()
                    .and_then(|base| self.remove_minutes_draft_if_unchanged(&base).err());
                self.ai.editor_sync = true;
                self.ai.editing = false;
                self.ai.artifact_version = None;
                self.ai.file_preview = None;
                self.ai.chat_dirty = true;
                self.ai.status = if let Some(error) = cleanup_error {
                    format!("{status}；草稿未清理：{error:#}")
                } else {
                    status
                };
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
        if self.quit_pending {
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
                    if h.exports_pending() {
                        self.ai.status = h
                            .export_warning
                            .clone()
                            .unwrap_or_else(|| "纪要正文已载入；可在文件与版本中重试导出。".into());
                    }
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
        };
        config.validate()?;
        Ok(config)
    }
    fn ai_config_from_inputs(&self, cx: &App) -> anyhow::Result<AiConfig> {
        let config = AiConfig {
            provider: self.ai.config.provider,
            base_url: self.ai.url.read(cx).value().trim().into(),
            model: self.ai.model.read(cx).value().trim().into(),
        };
        config.validate()?;
        Ok(config)
    }
    pub(super) fn enable_live_notes(&mut self, cx: &mut Context<Self>) {
        if self.live_notes.is_some()
            || self.live_notes_credentials_busy
            || self.live_notes_preparing
        {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            self.live_notes_status = "会议保存位置尚未就绪。".into();
            cx.notify();
            return;
        };
        let Some(run_id) = self.transcript.run.as_ref().map(|run| run.run_id.clone()) else {
            self.live_notes_status = "转写运行身份尚未就绪。".into();
            cx.notify();
            return;
        };
        let meeting_id = match opencap_core::meeting_manifest(&base) {
            Ok(Some(manifest)) => manifest.meeting_id,
            Ok(None) => {
                self.live_notes_status = "此会议尚无身份清单，暂不能开启增量纪要。".into();
                cx.notify();
                return;
            }
            Err(error) => {
                self.live_notes_status = format!("无法读取会议身份：{error:#}");
                cx.notify();
                return;
            }
        };
        if self
            .transcript
            .run
            .as_ref()
            .and_then(|run| run.meeting_id.as_deref())
            != Some(meeting_id.as_str())
        {
            self.live_notes_status =
                "转写运行尚未绑定当前会议或会议身份不一致；请先核对运行信息。".into();
            cx.notify();
            return;
        }
        let config = match self.ai_config_from_inputs(cx) {
            Ok(config) => config,
            Err(error) => {
                self.live_notes_status = format!("请先配置 AI 服务：{error:#}");
                cx.notify();
                return;
            }
        };
        let key = self.ai.key.read(cx).value().trim().to_string();
        if !key.is_empty() || config.is_local() {
            self.activate_live_notes(base, meeting_id, run_id, config, key, cx);
            cx.notify();
            return;
        }
        self.live_notes_credentials_busy = true;
        self.live_notes_status = "正在读取所选服务的系统凭据…".into();
        let task = cx.read_credentials(&config.credential_id());
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.live_notes_credentials_busy = false;
                if !this.ai_config_from_inputs(cx).is_ok_and(|current| {
                    current.credential_id() == config.credential_id()
                        && current.model == config.model
                        && current.provider == config.provider
                        && current.base_url == config.base_url
                }) {
                    this.live_notes_status = "AI 服务设置已变化，本轮未开始发送。".into();
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Some((_, bytes))) => match String::from_utf8(bytes) {
                        Ok(key) if !key.trim().is_empty() => {
                            this.activate_live_notes(base, meeting_id, run_id, config, key, cx)
                        }
                        _ => this.live_notes_status = "所选 AI 密钥无效，请重新保存。".into(),
                    },
                    Ok(None) => this.live_notes_status = "所选 AI 服务没有保存密钥。".into(),
                    Err(_) => this.live_notes_status = "无法读取系统凭据，请检查 AI 设置。".into(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn activate_live_notes(
        &mut self,
        base: PathBuf,
        meeting_id: String,
        run_id: String,
        config: AiConfig,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if self.live_notes.is_some()
            || self.live_notes_preparing
            || self.quit_pending
            || self.transcript_output.as_ref() != Some(&base)
            || self.transcript.run.as_ref().map(|run| run.run_id.as_str()) != Some(run_id.as_str())
            || self
                .transcript
                .run
                .as_ref()
                .and_then(|run| run.meeting_id.as_deref())
                != Some(meeting_id.as_str())
            || !opencap_core::meeting_manifest(&base).is_ok_and(|manifest| {
                manifest.is_some_and(|manifest| manifest.meeting_id == meeting_id.as_str())
            })
            || self.asr.as_ref().is_none_or(|asr| asr.finished())
        {
            self.live_notes_status = "转写状态已变化，未开始发送；请重新选择。".into();
            return;
        }
        self.live_notes_preparing = true;
        self.live_notes_prepare_cancel =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.live_notes_prepare_cancel.clone();
        self.live_notes_status = "正在后台准备增量纪要…".into();
        let transcript = self.transcript.clone();
        let expected_config = config.clone();
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let task_meeting = meeting_id.clone();
            let task_run = run_id.clone();
            let task_cancelled = cancelled.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::live_notes::Controller::new(
                            task_base,
                            task_meeting,
                            task_run,
                            config,
                            key,
                            &transcript,
                            &task_cancelled,
                        )
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("增量纪要准备线程异常退出")))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.live_notes_preparing = false;
                // Preparing may have archived the previous current document,
                // even if the user has since switched to another view.
                this.meeting_scan_extra.insert(base.clone());
                this.notes_index_pending = Some(std::time::Instant::now());
                let current = !cancelled.load(std::sync::atomic::Ordering::Acquire)
                    && !this.quit_pending
                    && this.live_notes.is_none()
                    && this.transcript_output.as_ref() == Some(&base)
                    && this.transcript.run.as_ref().is_some_and(|run| {
                        run.run_id == run_id
                            && run.meeting_id.as_deref() == Some(meeting_id.as_str())
                    })
                    && this.ai_config_from_inputs(cx).is_ok_and(|config| {
                        config.credential_id() == expected_config.credential_id()
                            && config.model == expected_config.model
                            && config.provider == expected_config.provider
                            && config.base_url == expected_config.base_url
                    });
                match result {
                    Ok(controller) if current => {
                        this.live_notes = Some(controller);
                        this.live_notes_status.clear();
                    }
                    Ok(_) => {
                        this.live_notes_status =
                            "纪要准备期间会议或服务已变化；未发送请求，已保存的归档保留。".into()
                    }
                    Err(error) => {
                        this.live_notes_status =
                            format!("无法开启增量纪要，已有文件保留：{error:#}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
            || self.transcript_write_busy()
            || self.ai.document_task.is_some()
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
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
        if !key.is_empty() || (config.is_local()) {
            self.start_minutes(config, key, action, instruction);
            cx.notify();
            return;
        }
        let base = self.transcript_output.clone();
        let baseline = match serde_json::to_vec(&self.transcript) {
            Ok(value) => value,
            Err(error) => {
                self.ai.status = format!("无法核对当前逐字稿：{error}");
                cx.notify();
                return;
            }
        };
        let history = match serde_json::to_vec(&self.ai.history) {
            Ok(value) => value,
            Err(error) => {
                self.ai.status = format!("无法核对当前纪要版本：{error}");
                cx.notify();
                return;
            }
        };
        self.ai.credentials_busy = true;
        self.ai.status = "正在读取系统凭据…".into();
        let task = cx.read_credentials(&config.credential_id());
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.credentials_busy = false;
                let unchanged = !this.quit_pending
                    && !this.active()
                    && !this.transcript_write_busy()
                    && !this.minutes_task_busy()
                    && !this.ai.editing
                    && !this.transcript.primary_save_pending
                    && this.transcript_output == base
                    && super::transcription::transcript_matches_snapshot(
                        &this.transcript,
                        &baseline,
                    )
                    && serde_json::to_vec(&this.ai.history).is_ok_and(|value| value == history)
                    && this.ai_config_from_inputs(cx).is_ok_and(|current| {
                        current.credential_id() == config.credential_id()
                            && current.model == config.model
                    });
                if !unchanged {
                    this.ai.status =
                        "读取凭据期间会议、稿件、纪要或服务设置已变化；输入内容保留，请重新发送。"
                            .into();
                    cx.notify();
                    return;
                }
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
        if self.quit_pending
            || self.active()
            || self.transcript_write_busy()
            || self.ai.editing
            || self.summary_task.is_some()
            || self.ai.document_task.is_some()
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
        {
            self.ai.status = "当前任务尚未结束或逐字稿尚未保存；输入内容保留，请稍后发送。".into();
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            self.ai.status = "会议保存位置不可用，未发送；输入内容保留".into();
            return;
        };
        let pending = serde_json::json!({"request":instruction,"artifact":action != MinutesAction::Ask,"draft":self.ai.submitted_composer});
        let request_save = opencap_core::managed_meeting_activity(&output).and_then(|_activity| {
            opencap_asr::atomic_write(
                &output.with_extension("summary.request.json"),
                pending.to_string().as_bytes(),
            )
        });
        if let Err(e) = request_save {
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
        self.summary_task_base = Some(output.clone());
        self.ai.status = format!("{} 正在处理；已有版本保留…", config.provider.label());
        if let Err(error) = std::thread::Builder::new()
            .name("opencap-minutes-request".into())
            .spawn(move || {
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
                        let mut pending = s;
                        loop {
                            if cancelled.load(Ordering::Acquire) {
                                break;
                            }
                            match progress_tx
                                .send_timeout(pending, std::time::Duration::from_millis(100))
                            {
                                Ok(())
                                | Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                    break;
                                }
                                Err(crossbeam_channel::SendTimeoutError::Timeout(event)) => {
                                    pending = event
                                }
                            }
                        }
                    },
                ));
            })
        {
            self.summary_task = None;
            self.summary_task_base = None;
            self.ai.events = None;
            self.ai.clear_composer = false;
            self.ai.status = format!("无法启动纪要任务，请重试；输入和已保存请求保留：{error}");
        }
    }
}
