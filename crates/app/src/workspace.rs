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
        self.media
            .as_ref()
            .unwrap_or(&self.base)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
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
            .or_else(|| saved.and_then(|t| t.source));
        files.push(MeetingFile {
            base,
            media,
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            complete,
        });
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files
}
fn card() -> Div {
    ui::card()
}
pub(super) fn heading(title: &str, description: &str) -> Div {
    ui::heading(title, description)
}
impl Recorder {
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
                            .child(self.transcript.title()),
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
            header = header.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        label(format!("保存位置：{}", folder.display()))
                            .flex_1()
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .child(
                        button("meeting-location", "查看文件", false)
                            .icon(gpui::component::IconName::FolderOpen)
                            .ghost()
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&open);
                            }),
                    ),
            );
        }
        header
    }
    pub(super) fn meeting_workspace(&self, width: f32, cx: &Context<Self>) -> Div {
        match self.meeting_page {
            MeetingPage::Library => self.library_view(cx),
            MeetingPage::Transcript => self.transcription_view(width, cx),
            MeetingPage::Minutes => self.minutes_view(width, cx),
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
                                if this.targets.is_empty() {
                                    this.refresh_targets();
                                }
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        for (i, meeting) in self.meetings.iter().enumerate().take(100) {
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
                            .flex()
                            .flex_wrap()
                            .gap_2()
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
                                button(("meeting-folder", i), "打开文件夹", false)
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
        let export = primary_button("export-notes-word", "导出笔记 Word")
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
                            this.notes_status = format!("笔记 Word 已保存：{}", path.display());
                            let _ = opencap_platform::open_path(&path);
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
