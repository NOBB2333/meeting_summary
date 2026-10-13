use super::*;
use gpui::component::{input::Input, text::TextView};
use opencap_asr::{AiConfig, AiProvider, MinutesAction, MinutesHistory};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct AiWorkspace {
    pub config: AiConfig,
    url: Entity<InputState>,
    model: Entity<InputState>,
    key: Entity<InputState>,
    instruction: Entity<InputState>,
    pub history: MinutesHistory,
    pub status: String,
    pub credentials_busy: bool,
    pub cancelled: Arc<AtomicBool>,
    pub progress: Option<crossbeam_channel::Receiver<String>>,
    document_scroll: ScrollHandle,
}
fn ai_path() -> PathBuf {
    config_path().with_file_name("ai.json")
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
        Self {
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
                InputState::new(window, cx)
                    .placeholder("例如：按正式会议纪要格式改写，单独列出帮扶订单")
            }),
            config,
            history: MinutesHistory::default(),
            status,
            credentials_busy: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: None,
            document_scroll: ScrollHandle::new(),
        }
    }
}
impl Recorder {
    pub(super) fn reset_minutes(&mut self) {
        self.ai.history = MinutesHistory::default();
        self.summary_text.clear();
    }
    pub(super) fn load_minutes(&mut self) {
        if let Some(output) = &self.transcript_output {
            match MinutesHistory::load(output) {
                Ok(h) => {
                    self.summary_text = h.latest().to_string();
                    self.ai.history = h;
                }
                Err(_) => {
                    self.ai.status = "已有纪要无法载入；文件保留，请检查保存目录".into();
                    self.reset_minutes();
                }
            }
        }
    }
    fn ai_config_from_inputs(&self, cx: &App) -> anyhow::Result<AiConfig> {
        let config = AiConfig {
            provider: self.ai.config.provider,
            base_url: self.ai.url.read(cx).value().trim().into(),
            model: self.ai.model.read(cx).value().trim().into(),
            background: self.ai.config.background,
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
        self.ai.key.update(cx, |s, cx| s.set_value("", window, cx));
        self.ai.config = config;
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
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.ai.credentials_busy = false;
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
        if self.active() || self.transcription_busy() || !self.transcript.complete {
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
        let mut instruction = self.ai.instruction.read(cx).value().to_string();
        if action != MinutesAction::Generate && instruction.trim().is_empty() {
            self.ai.status = "请填写修改要求或问题".into();
            cx.notify();
            return;
        }
        if action == MinutesAction::Generate {
            let template = match self.minutes_style {
                1 => {
                    "采用阅读摘要格式：会议概况、全文摘要、章节速览、说话人总结、待办与未决事项。章节与说话人总结均保留来源段号及时间；未做说话人分析则明确说明。"
                }
                2 => {
                    "采用行动清单格式：简要背景、明确决策、待办表格（事项、负责人、截止时间、状态、来源）、风险与待确认事项、说话人总结。不可从发言人编号推断负责人姓名。"
                }
                _ => {
                    "采用正式会议纪要格式：会议概况、全文摘要、章节速览、说话人总结、明确决策、待办表格、风险与未决事项。保留来源段号与时间。"
                }
            };
            instruction = format!("{template}\n用户补充要求：{instruction}");
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
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.ai.cancelled = cancelled.clone();
        self.ai.progress = Some(progress_rx);
        self.summary_task = Some(rx);
        self.ai.status = format!("{} 正在处理；已有版本保留…", config.provider.label());
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::edit_minutes(
                &transcript,
                &config,
                &key,
                history,
                action,
                &instruction,
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
        div().p_4().rounded_lg().bg(rgb(SURFACE)).flex().flex_col().gap_3()
            .child(div().text_lg().child("AI 配置"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("deepseek-preset", "DeepSeek 官方 · 只填 Key", self.ai.config.is_deepseek())
                    .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.set_ai_preset(AiConfig::default(), window, cx))))
                .child(button("openai-preset", "OpenAI 官方", false).disabled(busy).on_click(cx.listener(|this, _, window, cx| {
                    this.set_ai_preset(AiConfig { provider: AiProvider::Responses,base_url:AiProvider::Responses.default_url().into(),model:String::new(),background:false }, window, cx);
                }))))
            .child(div().flex().flex_wrap().gap_2().children(AiProvider::ALL.into_iter().map(|p| {
                button(("ai-provider", p as usize), p.label(), self.ai.config.provider == p).disabled(busy).on_click(cx.listener(move |this, _, window, cx| {
                    if this.ai.config.provider == p { return; }
                    this.ai.config.provider = p; this.ai.config.background = false;
                    this.ai.url.update(cx, |s, cx| s.set_value(p.default_url(), window, cx));
                    this.ai.model.update(cx, |s, cx| s.set_value("", window, cx));
                    this.ai.key.update(cx, |s, cx| s.set_value("", window, cx));
                    this.ai.status = "请选择该服务的模型，填写密钥后保存".into(); cx.notify();
                }))
            })))
            .child(label("服务地址（填写根地址，自动追加协议路径）")).child(Input::new(&self.ai.url))
            .child(label("模型名")).child(Input::new(&self.ai.model))
            .child(label("API Key（保存到 macOS 钥匙串 / Windows 凭据库）")).child(Input::new(&self.ai.key))
            .when(self.ai.config.provider == AiProvider::Responses && !self.ai.config.is_deepseek(), |d| d.child(
                button("responses-background", if self.ai.config.background { "服务端后台响应：开启" } else { "服务端后台响应：关闭" }, self.ai.config.background)
                    .on_click(cx.listener(|this, _, _, cx| { this.ai.config.background = !this.ai.config.background; cx.notify(); }))
            ))
            .child(div().flex().gap_2()
                .child(button("save-ai-config", "保存 AI 配置", false).disabled(busy).on_click(cx.listener(|this, _, window, cx| this.save_ai(window, cx))))
                .child(button("delete-ai-key", "删除已存密钥", false).disabled(busy).on_click(cx.listener(|this, _, window, cx| {
                    let config = match this.ai_config_from_inputs(cx) { Ok(c) => c, Err(e) => { this.ai.status = e.to_string(); cx.notify(); return; } };
                    let task = cx.delete_credentials(&config.credential_id()); this.ai.credentials_busy = true;
                    this.ai.key.update(cx, |s, cx| s.set_value("", window, cx));
                    cx.spawn(async move |entity, cx| { let result = task.await; let _ = entity.update(cx, |this, cx| { this.ai.credentials_busy = false; this.ai.status = if result.is_ok() { "已删除当前服务的密钥".into() } else { "密钥删除失败或尚未保存".into() }; cx.notify(); }); }).detach();
                }))))
            .child(label("选择远程服务后，点击生成或提问会发送转录文字；音视频仍留在本机。后台响应要求服务支持 Responses background，并会在服务端暂存响应。"))
            .when(self.ai.config.is_deepseek(), |d| d.child(label("DeepSeek 使用应用后台任务；官方不支持服务端 background。密钥保存到系统凭据库，关闭和重启应用不会丢失。")))
            .child(label(self.ai.status.clone()))
    }
    pub(super) fn minutes_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let ready = self.transcript.complete && !self.transcript.text().is_empty();
        let has_minutes = !self.ai.history.latest().is_empty();
        let mut view = div()
            .p_4()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_lg().child("AI 会议纪要"))
                    .child(
                        button("open-ai-config", "AI 配置", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.settings = true;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(label(format!(
                "当前服务：{} · {} · 已保存 {} 个版本",
                self.ai.config.provider.label(),
                if self.ai.config.model.is_empty() {
                    "未配置模型"
                } else {
                    &self.ai.config.model
                },
                self.ai.history.versions.len()
            )))
            .child(label(
                "手动生成：点击下方按钮后才调用 AI。生成成功会自动保存独立的纪要 Word；可继续提问、修改，每次保留版本。",
            ))
            .child(label(if self.transcript.speaker_turns.is_empty() { "说话人尚未分析：若需要按说话人总结，请先到逐字稿页运行本地分析。" } else { "说话人标签会送入 AI；摘要按已识别编号组织，不自动推断姓名。" }))
            .child(div().flex().flex_wrap().gap_2().children(["正式纪要", "阅读摘要", "行动清单"].into_iter().enumerate().map(|(i,title)|
                button(("minutes-style",i),title,self.minutes_style==i).disabled(busy).on_click(cx.listener(move |this,_,_,cx| { this.minutes_style=i; cx.notify(); })))))
            .child(Input::new(&self.ai.instruction))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("generate-summary", "生成 AI 纪要与 Word", true)
                            .disabled(busy || !ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.request_minutes(MinutesAction::Generate, cx)
                            })),
                    )
                    .child(
                        button("revise-summary", "按要求修改纪要", false)
                            .disabled(busy || !ready || !has_minutes)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.request_minutes(MinutesAction::Revise, cx)
                            })),
                    )
                    .child(
                        button("ask-summary", "只提问", false)
                            .disabled(busy || !ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.request_minutes(MinutesAction::Ask, cx)
                            })),
                    )
                    .when(self.summary_task.is_some(), |d| {
                        d.child(button("cancel-summary", "取消 AI 任务", false).danger().on_click(
                            cx.listener(|this, _, _, cx| {
                                this.ai.cancelled.store(true, Ordering::Relaxed);
                                this.ai.status =
                                    "已请求取消；已有版本保留，等待当前请求结束…".into();
                                cx.notify();
                            }),
                        ))
                    }),
            )
            .child(label(self.ai.status.clone()));
        if let Some(output) = &self.transcript_output {
            let path = output.with_extension("summary.docx");
            let markdown_path = output.with_extension("summary.md");
            let word_base = output.clone();
            let folder = output
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf();
            view = div()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("open-minutes", "打开 Word 文档", false)
                                .disabled(!path.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&path);
                                }),
                        )
                        .child(
                            button("export-word", "重新导出 Word", false)
                                .disabled(busy || !has_minutes)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let path = word_base.with_extension("summary.docx");
                                    match opencap_asr::export_minutes_docx(
                                        this.ai.history.latest(),
                                        &path,
                                    ) {
                                        Ok(()) => {
                                            this.ai.status =
                                                format!("Word 已保存：{}", path.display());
                                            let _ = opencap_platform::open_path(&path);
                                        }
                                        Err(e) => {
                                            this.ai.status =
                                                format!("Word 导出失败；原纪要保留：{e}")
                                        }
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            button("open-minutes-markdown", "打开文字稿", false)
                                .disabled(!markdown_path.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&markdown_path);
                                }),
                        )
                        .child(button("minutes-history", "打开版本目录", false).on_click(
                            move |_, _, _| {
                                let _ = opencap_platform::open_path(&folder);
                            },
                        )),
                )
                .child(view);
        }
        if !self.summary_text.is_empty() {
            view = view.child(div().h(px(240.)).child(widgets::scrollable(
                "minutes-document-scroll",
                div().child(TextView::markdown(
                    "meeting-summary",
                    self.summary_text.clone(),
                )),
                &self.ai.document_scroll,
            )));
        }
        for (i, turn) in self
            .ai
            .history
            .conversation
            .iter()
            .enumerate()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            view = view.child(label(format!("你：{}", turn.request)));
            if i + 1 == self.ai.history.conversation.len() && turn.reply != self.summary_text {
                view = view.child(TextView::markdown(("ai-reply", i), turn.reply.clone()));
            }
        }
        view
    }
}
