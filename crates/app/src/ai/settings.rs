//! Global AI configuration, vocabulary and templates.
use super::*;

impl Recorder {
    pub(crate) fn ai_settings_view(&self, cx: &Context<Self>) -> Div {
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
            .child(label("API Key（保存到系统安全凭据库）")).child(Input::new(&self.ai.key).mask_toggle().disabled(busy))
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
}

impl Recorder {
    pub(crate) fn vocabulary_settings_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        div().flex().flex_col().gap_4()
            .child(ui::card().child(ui::section_title("全局专业词表", "保存一次，所有会议的全文校正与 AI 纪要共用"))
                .child(label("每行一个标准词，或“误识别词 => 正确词”。可填写公司、人名、中英文缩写和行业术语。"))
                .child(Textarea::new(&self.ai.terminology).disabled(busy).w_full())
                .child(primary_button("save-minutes-keywords", "保存全局词表").disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ai.format.terminology = this.ai.terminology.read(cx).value().to_string();
                        this.ai.status = match this.save_minutes_format() { Ok(()) => "全局词表已保存，所有会议都会使用。".into(), Err(e) => e.to_string() }; cx.notify();
                    })))
                .child(label("词表是大模型校对时的提示；不会把词表中的内容当作会议事实。打开逐字稿后可执行“AI 校正全文”。")))
            .child(ui::card().child(ui::section_title("识别语言", "下次文件识别及实时整句校正生效"))
                .child(div().flex().flex_wrap().gap_2().children(TranscriptionLanguage::ALL.into_iter().map(|language| {
                    button(("asr-language", language as usize), language.label(), self.config.transcription_language == language).disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| { this.config.transcription_language = language; this.save(); cx.notify(); }))
                })))
                .child(label("中英混说建议中文为主，纯英文选择英文；自动包含中英日韩粤。语言提示不能完全排除误判，历史文字可通过 AI 全文校正处理。")))
            .child(ui::card().child(ui::section_title("流式中文热词", "只影响实时中文识别；英文专业词请放到上方全局词表"))
                .child(Input::new(&self.hotwords_input).disabled(busy))
                .child(button("apply-hotwords", "保存识别热词", false).disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| { this.config.transcription_hotwords = this.hotwords_input.read(cx).value().to_string(); this.save(); this.ai.status = "实时识别热词已保存，下次转录生效。".into(); cx.notify(); }))))
            .child(label(self.ai.status.clone()))
    }
    pub(crate) fn template_settings_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        ui::card()
            .child(ui::section_title(
                "全局纪要模板",
                "所有会议共用；本场临时要求可直接在对话中提出",
            ))
            .child(div().flex().flex_wrap().gap_2().children(
                opencap_asr::MinutesStyle::ALL.into_iter().map(|style| {
                    button(
                        ("minutes-style", style as usize),
                        style.label(),
                        self.ai.format.style == style,
                    )
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ai.format.style = style;
                        cx.notify();
                    }))
                }),
            ))
            .child(self.minutes_format_options(busy, cx))
            .child(
                primary_button("save-minutes-template", "保存全局模板")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ai.format.guidance =
                            this.ai.format_guidance.read(cx).value().to_string();
                        this.ai.status = match this.save_minutes_format() {
                            Ok(()) => "全局纪要模板已保存。".into(),
                            Err(e) => e.to_string(),
                        };
                        cx.notify();
                    })),
            )
            .child(label(self.ai.status.clone()))
    }
}
