use super::*;
use gpui::component::input::Textarea;
use std::{collections::BTreeSet, path::Path};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MeetingPage {
    Library,
    Transcript,
    Minutes,
    Notes,
}
#[derive(Clone)]
pub(super) struct MeetingFile {
    pub base: PathBuf,
    pub media: Option<PathBuf>,
    pub modified: std::time::SystemTime,
    complete: Option<bool>,
    title: String,
    preview: String,
    details: String,
}
impl MeetingFile {
    fn age(&self) -> String {
        let hours = self.modified.elapsed().unwrap_or_default().as_secs() / 3_600;
        if hours == 0 {
            return "最近更新".into();
        }
        if hours < 24 {
            return format!("{hours} 小时前");
        }
        match hours / 24 {
            1 => "1 天前".into(),
            n => format!("{n} 天前"),
        }
    }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn status(&self) -> String {
        [
            match self.complete {
                Some(true) => "逐字稿已完成",
                Some(false) => "逐字稿部分保存 · 可重新转录",
                None => "待转录 / 转写数据未能读取",
            },
            if self.base.with_extension("summary.md").exists() {
                "AI 纪要已生成"
            } else {
                "AI 纪要未生成"
            },
        ]
        .join("  ·  ")
    }
}
fn index_path() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(path) = smoke::isolated_dir() {
        return path.join("meetings.json");
    }
    config_path().with_file_name("meetings.json")
}
/// Scan only application artifacts, plus explicitly remembered imports. No media decoding.
pub(super) fn discover(root: &Path) -> Vec<MeetingFile> {
    fn scan(dir: &Path, depth: usize, bases: &mut BTreeSet<PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten().take(2000) {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                scan(&path, depth - 1, bases);
            } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                for suffix in [".transcript.json", ".recording.json"] {
                    if let Some(stem) = name.strip_suffix(suffix) {
                        bases.insert(path.with_file_name(stem));
                    }
                }
            }
        }
    }
    let mut bases = std::fs::read(index_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<PathBuf>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    scan(root, 3, &mut bases);
    let mut files = Vec::new();
    for base in bases {
        let transcript = base.with_extension("transcript.json");
        let report = base.with_extension("recording.json");
        let artifact = if transcript.exists() {
            &transcript
        } else {
            &report
        };
        let Ok(metadata) = std::fs::metadata(artifact) else {
            continue;
        };
        let saved = if std::fs::metadata(&transcript).is_ok_and(|m| m.len() < 16_000_000) {
            std::fs::read(&transcript)
                .ok()
                .and_then(|b| serde_json::from_slice::<opencap_asr::Transcript>(&b).ok())
        } else {
            None
        };
        let complete = saved.as_ref().map(|t| t.complete && !t.cancelled);
        let media = [base.with_extension("mp4"), base.with_extension("m4a")]
            .into_iter()
            .find(|p| p.exists())
            .or_else(|| saved.as_ref().and_then(|t| t.source.clone()));
        let (title, preview, details) = meeting_details(&base, media.as_deref(), saved.as_ref());
        files.push(MeetingFile {
            base,
            media,
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            complete,
            title,
            preview,
            details,
        });
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files
}
fn meeting_details(
    base: &Path,
    media: Option<&Path>,
    transcript: Option<&opencap_asr::Transcript>,
) -> (String, String, String) {
    let summary = std::fs::metadata(base.with_extension("summary.md"))
        .ok()
        .filter(|m| m.len() <= 2_000_000)
        .and_then(|_| std::fs::read_to_string(base.with_extension("summary.md")).ok())
        .unwrap_or_default();
    let fallback = media
        .unwrap_or(base)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let title = opencap_asr::meeting_name(base).unwrap_or_else(|| fallback.into_owned());
    let plain = summary
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .filter(|line| !line.is_empty() && !line.starts_with("| ---"))
        .collect::<Vec<_>>()
        .join(" ");
    let plain = if plain.is_empty() {
        transcript
            .map(|t| {
                t.segments
                    .iter()
                    .filter(|s| s.finalized)
                    .map(|s| s.text.as_str())
                    .take(5)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    } else {
        plain
    };
    let preview: String = plain.chars().take(140).collect();
    let preview = if preview.is_empty() {
        "尚未转录，完成后可在这里预览会议内容".into()
    } else if plain.chars().count() > 140 {
        format!("{preview}…")
    } else {
        preview
    };
    let report: serde_json::Value = std::fs::metadata(base.with_extension("recording.json"))
        .ok()
        .filter(|m| m.len() < 4_000_000)
        .and_then(|_| std::fs::read(base.with_extension("recording.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let seconds = report["stats"]["duration_seconds"]
        .as_f64()
        .filter(|s| s.is_finite() && *s > 0.)
        .or_else(|| transcript.and_then(|t| t.source_duration_ms.map(|ms| ms as f64 / 1000.)));
    let duration = seconds
        .map(|s| {
            format!(
                "时长 {}",
                task::duration_text(Duration::from_millis((s * 1000.) as u64))
            )
        })
        .unwrap_or_else(|| {
            transcript
                .and_then(|t| t.segments.last())
                .map(|s| {
                    format!(
                        "内容约 {}",
                        task::duration_text(Duration::from_millis(s.end_ms))
                    )
                })
                .unwrap_or_else(|| "时长待获取".into())
        });
    let size = media
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| {
            let bytes = m.len() as f64;
            if bytes >= 1_000_000_000. {
                format!("{:.2} GB", bytes / 1_000_000_000.)
            } else if bytes >= 1_000_000. {
                format!("{:.1} MB", bytes / 1_000_000.)
            } else {
                format!("{:.1} KB", bytes / 1000.)
            }
        })
        .unwrap_or_else(|| "原文件不在当前位置".into());
    (title, preview, format!("{duration}  ·  {size}"))
}
fn card() -> Div {
    ui::card()
}
pub(super) fn heading(title: &str, description: &str) -> Div {
    ui::heading(title, description)
}
fn copy_export(
    source: &Path,
    destination: &Path,
    protected_media: Option<&Path>,
) -> anyhow::Result<()> {
    if let Some(media) = protected_media
        && let (Ok(media), Ok(target)) = (media.canonicalize(), destination.canonicalize())
    {
        anyhow::ensure!(
            media != target,
            "请为文档选择新位置，原录音或视频不会被覆盖"
        );
    }
    let bytes = std::fs::read(source)?;
    opencap_asr::atomic_write(destination, &bytes)
}

impl Recorder {
    fn begin_rename(&mut self, base: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let title = opencap_asr::meeting_name(&base)
            .or_else(|| {
                self.meetings
                    .iter()
                    .find(|m| m.base == base)
                    .map(|m| m.title())
            })
            .unwrap_or_else(|| self.transcript.title());
        self.rename_input
            .update(cx, |s, cx| s.set_value(title, window, cx));
        self.rename_base = Some(base);
        cx.notify();
    }
    fn rename_editor(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(gpui::component::input::Input::new(&self.rename_input).flex_1())
            .child(
                primary_button("save-meeting-name", "保存名称").on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(base) = this.rename_base.clone() {
                            match opencap_asr::rename_meeting(
                                &base,
                                &this.rename_input.read(cx).value(),
                            ) {
                                Ok(()) => {
                                    if this.transcript_output.as_ref() == Some(&base) {
                                        this.meeting_title =
                                            opencap_asr::meeting_name(&base).unwrap_or_default();
                                    }
                                    this.rename_base = None;
                                    this.refresh_meetings();
                                }
                                Err(e) => this.message = format!("重命名失败：{e:#}"),
                            }
                            cx.notify();
                        }
                    },
                )),
            )
            .child(
                button("cancel-meeting-name", "取消", false)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.rename_base = None;
                        cx.notify();
                    })),
            )
    }
    /// Save an independent document copy. Opening an existing document is a separate action.
    pub(super) fn export_document(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        let folder = source.parent().unwrap_or(Path::new("."));
        let kind = if source.to_string_lossy().ends_with("summary.docx") {
            "纪要"
        } else if source.to_string_lossy().ends_with("notes.docx") {
            "笔记"
        } else {
            "逐字稿"
        };
        let title = if self.meeting_title.is_empty() {
            self.transcript.title()
        } else {
            self.meeting_title.clone()
        };
        let suggested = opencap_asr::document_filename(&title, kind);
        let prompt = cx.prompt_for_new_path(folder, Some(&suggested));
        let protected_media = self.transcript.source.clone();
        cx.spawn(async move |entity, cx| {
            let selected = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                error => {
                    let _ = entity.update(cx, |this, cx| {
                        this.message = format!("无法选择导出位置：{error:?}");
                        cx.notify();
                    });
                    return;
                }
            };
            let mut destination = selected;
            if destination.extension().is_none() {
                destination.set_extension("docx");
            }
            let saved = destination.clone();
            let result = cx
                .background_executor()
                .spawn(
                    async move { copy_export(&source, &destination, protected_media.as_deref()) },
                )
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.message = match result {
                    Ok(()) => format!("已导出副本：{}", saved.display()),
                    Err(error) => format!("导出失败：{error:#}"),
                };
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn refresh_meetings(&mut self) {
        self.meetings = discover(&self.config.output_dir);
        let paths: Vec<_> = self.meetings.iter().map(|m| &m.base).collect();
        if let Ok(bytes) = serde_json::to_vec(&paths) {
            if let Some(parent) = index_path().parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::read(index_path()).ok().as_ref() != Some(&bytes) {
                let _ = opencap_asr::atomic_write(&index_path(), &bytes);
            }
        }
    }
    pub(super) fn remember_meeting(&mut self) {
        if let Some(base) = &self.transcript_output {
            let mut paths: Vec<PathBuf> = self.meetings.iter().map(|m| m.base.clone()).collect();
            if !paths.contains(base) {
                paths.push(base.clone());
            }
            if let Some(parent) = index_path().parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec(&paths) {
                let _ = opencap_asr::atomic_write(&index_path(), &bytes);
            }
        }
        self.refresh_meetings();
    }
    pub(super) fn sync_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notes_base != self.transcript_output {
            self.persist_notes(cx);
            self.notes_base = self.transcript_output.clone();
            self.meeting_title = self
                .transcript_output
                .as_deref()
                .and_then(opencap_asr::meeting_name)
                .unwrap_or_else(|| self.transcript.title());
            let text = self
                .notes_base
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p.with_extension("notes.txt")).ok())
                .unwrap_or_default();
            self.notes_saved = text.clone();
            self.notes.update(cx, |s, cx| s.set_value(text, window, cx));
            self.notes_status = "笔记在本机自动保存；点击“导出笔记 Word”更新文档。".into();
        }
    }
    pub(super) fn persist_notes(&mut self, cx: &App) {
        let Some(base) = &self.notes_base else {
            return;
        };
        let text = self.notes.read(cx).value().to_string();
        if text == self.notes_saved {
            return;
        }
        match opencap_asr::atomic_write(&base.with_extension("notes.txt"), text.as_bytes()) {
            Ok(()) => {
                self.notes_saved = text;
                self.notes_status = "笔记已自动保存到本机。Word 需点击导出以更新。".into();
            }
            Err(e) => self.notes_status = format!("笔记保存失败：{e}"),
        }
    }
    pub(super) fn meeting_header(&self, cx: &Context<Self>) -> Div {
        let mut header = div()
            .flex_shrink_0()
            .px_6()
            .py_4()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        button("back-meetings", "返回列表", false)
                            .icon(gpui::component::IconName::ArrowLeft)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.meeting_page = MeetingPage::Library;
                                this.refresh_meetings();
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(if self.meeting_title.is_empty() {
                                self.transcript.title()
                            } else {
                                self.meeting_title.clone()
                            }),
                    )
                    .child(
                        button("rename-current-meeting", "重命名", false)
                            .ghost()
                            .disabled(self.active())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(base) = this.transcript_output.clone() {
                                    this.begin_rename(base, window, cx);
                                }
                            })),
                    ),
            )
            .child(
                div().flex().flex_wrap().items_center().gap_2().children(
                    [
                        (MeetingPage::Transcript, "逐字稿"),
                        (MeetingPage::Minutes, "AI 纪要"),
                        (MeetingPage::Notes, "个人笔记"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, (page, title))| {
                        button(("meeting-section", i), title, self.meeting_page == page).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.meeting_page = page;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            }),
                        )
                    }),
                ),
            );
        if let Some(base) = &self.transcript_output {
            let folder = base.parent().unwrap_or(Path::new(".")).to_path_buf();
            let open = folder.clone();
            let copy_folder = folder.clone();
            header = header.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        label(format!("会议文件夹：{}", folder.display()))
                            .flex_1()
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .child(
                        button("copy-meeting-location", "复制路径", false)
                            .ghost()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_folder.display().to_string(),
                                ))
                            }),
                    )
                    .child(
                        button("meeting-location", "打开会议文件夹", false)
                            .icon(gpui::component::IconName::FolderOpen)
                            .ghost()
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&open);
                            }),
                    ),
            );
        }
        if let (Some(source), Some(base)) = (&self.transcript.source, &self.transcript_output)
            && source.parent() != base.parent()
            && let Some(folder) = source.parent()
        {
            let original_folder = folder.to_path_buf();
            header = header.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        label(format!("原文件留在：{}", source.display()))
                            .flex_1()
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .child(
                        button("original-media-location", "打开原文件位置", false)
                            .ghost()
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&original_folder);
                            }),
                    ),
            );
        }
        if self.rename_base.is_some() && self.rename_base == self.transcript_output {
            header = header.child(self.rename_editor(cx));
        }
        header
    }
    pub(super) fn meeting_workspace(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        match self.meeting_page {
            MeetingPage::Library => self.library_view(cx),
            MeetingPage::Transcript => self.transcription_view(width, cx),
            MeetingPage::Minutes => self.minutes_view(width, height, cx),
            MeetingPage::Notes => self.notes_view(cx),
        }
    }
    fn library_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let mut view = div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(heading(
                        "会议文件",
                        "每一场会议的原文、纪要和笔记，集中在这里。",
                    ))
                    .child(
                        primary_button("import-meeting", "导入音频 / 视频")
                            .icon(gpui::component::IconName::Plus)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_transcription_file(false, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(label(format!(
                        "共 {} 场会议 · 最近更新优先",
                        self.meetings.len()
                    )))
                    .child(
                        button("refresh-meetings", "刷新", false)
                            .icon(gpui::component::IconName::RefreshCw)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_meetings();
                                cx.notify();
                            })),
                    ),
            )
            .when(self.transcription_busy(), |d| {
                d.child(
                    card()
                        .bg(rgb(SOFT))
                        .child(ui::section_title(
                            "正在处理会议",
                            &self.transcription_status,
                        ))
                        .child(button("current-task", "查看当前任务", false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.meeting_page = MeetingPage::Transcript;
                                cx.notify();
                            }),
                        )),
                )
            });
        if self.meetings.is_empty() {
            view = view.child(
                card()
                    .py_8()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .size(px(64.))
                            .rounded_xl()
                            .bg(rgb(SOFT))
                            .text_color(rgb(ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(gpui::component::IconName::FolderOpen).size(px(28.))),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("让下一场会议留下记录"),
                    )
                    .child(label("录制一场会议，或导入已有音频和视频。"))
                    .child(
                        primary_button("empty-record", "开始一次录制").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.transcription_tab = false;
                                this.settings = false;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        for (i, meeting) in self.meetings.iter().enumerate().take(100) {
            let rename_base = meeting.base.clone();
            let transcript = meeting.base.with_extension("transcript.json");
            let exists = transcript.exists();
            let media = meeting.media.clone();
            let playable = media.as_ref().is_some_and(|p| p.exists());
            let selected = transcript.clone();
            let source = media.clone();
            let folder = meeting
                .base
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();
            view = view.child(
                card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(40.))
                                    .flex_shrink_0()
                                    .rounded_lg()
                                    .bg(rgb(SOFT))
                                    .text_color(rgb(ACCENT))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(ui::icon(gpui::component::IconName::FileText)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(meeting.title()),
                                    )
                                    .child(label(meeting.status())),
                            )
                            .child(label(meeting.age())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(meeting.preview.clone()),
                    )
                    .child(label(meeting.details.clone()))
                    .when(self.rename_base.as_ref() == Some(&meeting.base), |d| {
                        d.child(self.rename_editor(cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                button(("rename-meeting", i), "重命名", false)
                                    .ghost()
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.begin_rename(rename_base.clone(), window, cx)
                                    })),
                            )
                            .child(
                                button(
                                    ("open-meeting", i),
                                    if exists {
                                        "查看逐字稿 / 纪要"
                                    } else {
                                        "转录并查看"
                                    },
                                    false,
                                )
                                .text_color(rgb(ACCENT))
                                .disabled(busy || (!exists && !playable))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if exists {
                                            this.start_file_transcription(
                                                selected.clone(),
                                                false,
                                                false,
                                            );
                                        } else if let Some(source) = &source {
                                            this.start_file_transcription(
                                                source.clone(),
                                                false,
                                                false,
                                            );
                                        }
                                        this.meeting_page = MeetingPage::Transcript;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button(("play-meeting", i), "播放原音 / 视频", false)
                                    .disabled(!playable)
                                    .on_click(move |_, _, _| {
                                        if let Some(path) = &media {
                                            let _ = opencap_platform::open_path(path);
                                        }
                                    }),
                            )
                            .child(
                                button(("meeting-folder", i), "会议文件夹", false)
                                    .ghost()
                                    .on_click(move |_, _, _| {
                                        let _ = opencap_platform::open_path(&folder);
                                    }),
                            ),
                    ),
            );
        }
        view.child(label(format!(
            "录制目录：{}",
            self.config.output_dir.display()
        )))
    }
    fn notes_view(&self, cx: &Context<Self>) -> Div {
        let export = primary_button("export-notes-word", "导出笔记 Word…")
            .disabled(self.transcript_output.is_none())
            .on_click(cx.listener(|this, _, _, cx| {
                this.persist_notes(cx);
                if let Some(base) = &this.transcript_output {
                    let md = format!(
                        "# {} · 个人笔记\n\n{}",
                        opencap_asr::escape_markdown(&this.transcript.title()),
                        opencap_asr::escape_markdown(&this.notes.read(cx).value())
                    );
                    let path = base.with_extension("notes.docx");
                    match opencap_asr::export_minutes_docx(&md, &path) {
                        Ok(()) => {
                            this.export_document(path, cx);
                        }
                        Err(e) => this.notes_status = format!("导出失败：{e}"),
                    }
                }
                cx.notify();
            }));
        card()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "个人笔记",
                        "记录自己的观察与补充，内容自动保存到本机。",
                    ))
                    .child(export),
            )
            .child(Textarea::new(&self.notes).disabled(self.transcript_output.is_none()))
            .child(label(self.notes_status.clone()))
    }
}

#[cfg(test)]
mod export_tests {
    use super::{copy_export, meeting_details};
    #[test]
    fn meeting_preview_uses_saved_body_duration_and_actual_media_size() {
        let dir =
            std::env::temp_dir().join(format!("opencap-library-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("generic");
        let media = base.with_extension("m4a");
        std::fs::write(&media, vec![0; 2_000_000]).unwrap();
        let transcript = opencap_asr::Transcript {
            source_duration_ms: Some(1_200_000),
            segments: vec![opencap_asr::Segment {
                id: 0,
                revision: 1,
                start_ms: 0,
                end_ms: 10000,
                text: "客户反馈与下周安排。".into(),
                finalized: true,
            }],
            ..Default::default()
        };
        let (_, preview, details) = meeting_details(&base, Some(&media), Some(&transcript));
        assert!(preview.contains("客户反馈"));
        assert!(details.contains("20 分"));
        assert!(details.contains("2.0 MB"));
        opencap_asr::rename_meeting(&base, "产品评审").unwrap();
        std::fs::write(
            base.with_extension("summary.md"),
            "# 新产品主题\n\n会议议定分批发布。",
        )
        .unwrap();
        let (title, preview, _) = meeting_details(&base, Some(&media), Some(&transcript));
        assert_eq!(title, "产品评审");
        assert!(preview.contains("分批发布"));
        assert!(!preview.contains("客户反馈"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn exporting_keeps_the_local_document_and_refuses_to_replace_original_media() {
        let dir = std::env::temp_dir().join(format!(
            "opencap-export-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("original.m4a");
        let document = dir.join("transcript.docx");
        let copy = dir.join("shared-copy.docx");
        std::fs::write(&original, b"unchanged audio").unwrap();
        std::fs::write(&document, b"document bytes").unwrap();
        copy_export(&document, &copy, Some(&original)).unwrap();
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&document).unwrap()
        );
        assert!(copy_export(&document, &original, Some(&original)).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"unchanged audio");
        assert_eq!(std::fs::read(&document).unwrap(), b"document bytes");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
