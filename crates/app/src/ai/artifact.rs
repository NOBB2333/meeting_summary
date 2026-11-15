//! Document reading and tools occupy the same bounded pane, never stack beneath it.
use super::*;

fn minutes_recovery_files(base: &std::path::Path) -> Vec<PathBuf> {
    let Some(name) = base.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{name}.summary.recovery-");
    let folder = base.parent().unwrap_or(std::path::Path::new("."));
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .ok()
        .into_iter()
        .flatten()
        .take(5000)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            let Some(id) = name
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".json"))
            else {
                return false;
            };
            let bytes = id.as_bytes();
            bytes.len() == 36
                && bytes.iter().enumerate().all(|(i, byte)| {
                    if [8, 13, 18, 23].contains(&i) {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        })
        .map(|entry| entry.path())
        .collect();
    files.sort_by(|a, b| {
        let modified = |path: &PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        modified(b).cmp(&modified(a)).then_with(|| a.cmp(b))
    });
    files.truncate(20);
    files
}

impl Recorder {
    pub(super) fn minutes_artifact_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.minutes_task_busy();
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
                            if let Some(base) = this.ai.editor_base.clone()
                                && let Err(error) = this.remove_minutes_draft_if_unchanged(&base)
                            {
                                this.ai.status = format!("当前编辑已放弃；其他草稿保留：{error:#}");
                            }
                            this.ai.draft_saved.clear();
                            this.ai.draft_exists = false;
                            this.ai.draft_recovery = None;
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
                    .child(
                        button("save-minutes-draft-recovery", "另存恢复副本", false)
                            .ghost()
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.save_minutes_draft_recovery(cx)),
                            ),
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
                                    if !path.exists()
                                        || (this.ai.artifact_version.is_none()
                                            && this.ai.history.exports_pending())
                                    {
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
        let busy = self.active() || self.minutes_task_busy();
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
            let word_exists = word.is_file();
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
                                .disabled(!word_exists)
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
                )
                .child(
                    button("retry-minutes-exports", "重试导出已保存正文", false)
                        .disabled(busy || self.ai.editing || self.ai.history.versions.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| this.retry_minutes_exports(cx))),
                );
            tools = tools.when(word_exists && self.ai.history.exports_pending(), |d| {
                d.child(label("如现有 Word 含需保留的修改，请先打开或导入核对。备份操作会保留原文件副本，再用已保存正文重试导出。"))
                    .child(button("backup-word-retry", "备份现有 Word 并重试", false)
                        .disabled(busy || self.ai.editing || self.ai.history.versions.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| this.backup_word_and_retry_exports(cx))))
            });
            if let Some(backup) = self.ai.history.last_word_backup_path(base) {
                tools = tools.child(label(format!(
                    "最近 Word 备份：{}（可在会议文件夹查看）",
                    backup.display()
                )));
            }
            tools = tools.child(
                button("reload-minutes-history", "重新载入已保存历史", false)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.reload_minutes_history(cx))),
            );
            let recoveries = minutes_recovery_files(base);
            if !recoveries.is_empty() {
                tools = tools.child(label("未提交的纪要恢复副本 · 请先核对（最多显示 20 份）"));
                for (index, path) in recoveries.into_iter().enumerate() {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    tools = tools.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(label(name).flex_1().min_w(px(0.)).truncate())
                            .child(
                                button(("preview-minutes-recovery", index), "查看并复制", false)
                                    .ghost()
                                    .disabled(self.ai.editing)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ai.file_preview = Some((
                                            format!("纪要恢复副本 {}", index + 1),
                                            this.create_text_document(path.clone(), window, cx),
                                        ));
                                        this.ai.document_tools_open = false;
                                        cx.notify();
                                    })),
                            ),
                    );
                }
            }
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
                .disabled(busy || self.ai.editing || !self.transcript.is_complete())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.ai.document_tools_open = false;
                    this.request_minutes(MinutesAction::Generate, cx);
                })),
        )
    }
}
