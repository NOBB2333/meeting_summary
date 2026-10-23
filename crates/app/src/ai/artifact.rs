//! Document reading and tools occupy the same bounded pane, never stack beneath it.
use super::*;

impl Recorder {
    pub(super) fn minutes_artifact_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let title = self
            .ai
            .file_preview
            .as_ref()
            .map(|(title, _)| title.clone())
            .unwrap_or_else(|| {
                self.ai
                    .artifact_version
                    .map(|n| format!("纪要 · v{n}"))
                    .unwrap_or_else(|| "纪要正文".into())
            });
        let mut view = div()
            .w_full()
            .h_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(SURFACE))
            .rounded_lg()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .min_w(px(0.))
                            .truncate()
                            .child(title),
                    )
                    .child(
                        button(
                            "toggle-minutes-versions",
                            if self.ai.document_tools_open {
                                "返回正文"
                            } else {
                                "文件与版本"
                            },
                            self.ai.document_tools_open,
                        )
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.document_tools_open = !this.ai.document_tools_open;
                            cx.notify();
                        })),
                    ),
            );
        if self.ai.document_tools_open {
            return view.child(self.minutes_document_tools(cx));
        }
        let mut tabs = div().flex().flex_wrap().gap_1().flex_shrink_0().child(
            button(
                "artifact-summary-tab",
                "纪要",
                self.ai.file_preview.is_none(),
            )
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.ai.file_preview = None;
                this.ai.artifact_version = None;
                cx.notify();
            })),
        );
        if let Some(base) = &self.transcript_output {
            for (id, title, ext) in [
                ("artifact-transcript-tab", "逐字稿", "transcript.txt"),
                ("artifact-notes-tab", "笔记", "notes.txt"),
            ] {
                let path = base.with_extension(ext);
                tabs = tabs.child(
                    button(
                        id,
                        title,
                        self.ai
                            .file_preview
                            .as_ref()
                            .is_some_and(|(s, _)| s == title),
                    )
                    .ghost()
                    .disabled(!path.exists() || self.ai.editing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ai.file_preview = Some((
                            title.into(),
                            this.create_text_document(path.clone(), window, cx),
                        ));
                        cx.notify();
                    })),
                );
            }
        }
        view = view.child(tabs);
        if let Some((_, preview)) = &self.ai.file_preview {
            return view.child(div().flex_1().min_h(px(0.)).child(preview.clone()));
        }
        let selected_old = self.ai.artifact_version.is_some_and(|number| {
            self.ai
                .history
                .versions
                .last()
                .is_none_or(|v| v.number != number)
        });
        view = view.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .child(
                    button(
                        "edit-minutes-body",
                        if self.ai.editing {
                            "放弃编辑"
                        } else if self.summary_text.is_empty() {
                            "自己编写"
                        } else {
                            "编辑正文"
                        },
                        self.ai.editing,
                    )
                    .disabled(busy || self.transcript_output.is_none() || selected_old)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.ai.editing {
                            if let Some(base) = &this.ai.editor_base {
                                let _ =
                                    std::fs::remove_file(base.with_extension("summary.draft.md"));
                            }
                            this.ai.draft_saved.clear();
                        }
                        this.ai.editor.update(cx, |s, cx| {
                            s.set_value(this.summary_text.clone(), window, cx)
                        });
                        this.ai.editing = !this.ai.editing;
                        cx.notify();
                    })),
                )
                .when(self.ai.editing, |d| {
                    d.child(
                        primary_button("save-minutes-body", "保存")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.save_manual_minutes(cx))),
                    )
                })
                .when(!self.ai.editing, |d| {
                    d.child(
                        button("export-word", "导出 Word…", false)
                            .disabled(self.summary_text.is_empty() || busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(base) = &this.transcript_output {
                                    let extension = this
                                        .ai
                                        .artifact_version
                                        .map(|n| format!("summary.v{n:03}.docx"))
                                        .unwrap_or_else(|| "summary.docx".into());
                                    let path = base.with_extension(extension);
                                    if !path.exists() {
                                        let text = this
                                            .ai
                                            .artifact_version
                                            .and_then(|n| {
                                                this.ai
                                                    .history
                                                    .versions
                                                    .iter()
                                                    .find(|v| v.number == n)
                                            })
                                            .map(|v| v.markdown.as_str())
                                            .unwrap_or(&this.summary_text);
                                        if let Err(e) =
                                            opencap_asr::export_minutes_docx(text, &path)
                                        {
                                            this.ai.status = format!("导出失败：{e}");
                                            cx.notify();
                                            return;
                                        }
                                    }
                                    this.export_document(path, cx);
                                }
                            })),
                    )
                }),
        );
        if self.ai.editing {
            return view.child(
                Textarea::new(&self.ai.editor)
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .disabled(busy),
            );
        }
        let streaming = self.ai.artifact_version.is_none()
            && self.ai.stream_artifact
            && !self.ai.pending_request.is_empty()
            && !self.ai.stream.is_empty();
        let text = if streaming {
            self.ai.stream.clone()
        } else {
            self.ai
                .artifact_version
                .and_then(|n| self.ai.history.versions.iter().find(|v| v.number == n))
                .map(|v| v.markdown.clone())
                .unwrap_or_else(|| self.summary_text.clone())
        };
        view.child(
            div()
                .id("minutes-document-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .when(text.is_empty(), |d| {
                    d.child(
                        div()
                            .py_8()
                            .child(label("在左侧发送要求生成纪要，或点击“自己编写”。")),
                    )
                })
                .when(!text.is_empty(), |d| {
                    d.child(
                        TextView::markdown("meeting-summary", text)
                            .selectable(true)
                            .scrollable(false),
                    )
                }),
        )
    }
    fn minutes_document_tools(&self, cx: &Context<Self>) -> Stateful<Div> {
        let busy = self.active() || self.transcription_busy();
        let mut tools = div()
            .id("minutes-tools-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        if let Some(base) = &self.transcript_output {
            let word = base.with_extension("summary.docx");
            let markdown = base.with_extension("summary.md");
            let folder = base
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf();
            tools = tools
                .child(label("文件操作"))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            button("open-minutes", "打开 Word", false)
                                .disabled(!word.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&word);
                                }),
                        )
                        .child(
                            button("open-minutes-markdown", "打开 Markdown", false)
                                .disabled(!markdown.exists())
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&markdown);
                                }),
                        )
                        .child(button("minutes-history", "会议文件夹", false).on_click(
                            move |_, _, _| {
                                let _ = opencap_platform::open_path(&folder);
                            },
                        )),
                )
                .child(
                    button("sync-minutes-file", "导入外部修改…", false)
                        .disabled(busy || self.ai.editing)
                        .on_click(cx.listener(|this, _, _, cx| this.import_minutes_file(cx))),
                );
        }
        tools = tools.child(label(format!(
            "保存的版本 · {}",
            self.ai.history.versions.len()
        )));
        for version in self.ai.history.versions.iter().rev() {
            let number = version.number;
            let markdown = version.markdown.clone();
            tools = tools.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .child(
                        label(format!(
                            "v{number} · {}",
                            ai_chat::display_request(&version.instruction)
                        ))
                        .truncate(),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(("preview-minutes-version", number), "查看", false)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ai.artifact_version = Some(number);
                                        this.ai.file_preview = None;
                                        this.ai.document_tools_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button(
                                    ("restore-minutes-version", number),
                                    "恢复为当前版本",
                                    false,
                                )
                                .disabled(
                                    busy || self.ai.editing
                                        || version.markdown == self.summary_text,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.ai.editor.update(cx, |s, cx| {
                                            s.set_value(markdown.clone(), window, cx)
                                        });
                                        this.ai.editing = true;
                                        this.save_manual_minutes(cx);
                                        this.ai.document_tools_open = false;
                                    },
                                )),
                            ),
                    ),
            );
        }
        tools.child(
            button("regenerate-minutes", "按全局模板重新生成", false)
                .disabled(busy || self.ai.editing || !self.transcript.complete)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ai.document_tools_open = false;
                    this.request_minutes(MinutesAction::Generate, cx);
                })),
        )
    }
}
