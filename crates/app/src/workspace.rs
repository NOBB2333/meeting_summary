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
    div()
        .p_4()
        .rounded_lg()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(0x313640))
        .flex()
        .flex_col()
        .gap_3()
}
pub(super) fn heading(title: &str, description: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(label(description.to_string()))
}
impl Recorder {
    pub(super) fn workspace_header(&self, status: &str, cx: &Context<Self>) -> Div {
        div()
            .flex_shrink_0()
            .px_6()
            .py_4()
            .bg(rgb(0x191c22))
            .border_b_1()
            .border_color(rgb(0x313640))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_3()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child("OpenCap"),
                            )
                            .child(label("录制 · 转写 · 会议纪要")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if self.active() { RED } else { 0x84bca8 }))
                            .child(status.to_string()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "record-tab",
                            "●  录制",
                            !self.settings && !self.transcription_tab,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcription_tab = false;
                            this.settings = false;
                            if this.targets.is_empty() {
                                this.refresh_targets();
                            }
                            this.page_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "transcription-tab",
                            "▤  会议文件",
                            !self.settings && self.transcription_tab,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcription_tab = true;
                            this.settings = false;
                            this.meeting_page = MeetingPage::Library;
                            this.refresh_meetings();
                            this.close_targets();
                            this.audio_picker = None;
                            this.page_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        })),
                    )
                    .child(button("settings", "⚙  设置 / AI", self.settings).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.settings = true;
                            this.close_targets();
                            this.audio_picker = None;
                            this.page_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        }),
                    )),
            )
    }
    pub(super) fn active_recording_bar(&self, cx: &Context<Self>) -> Div {
        div()
            .p_3()
            .bg(rgb(0x191c22))
            .border_t_1()
            .border_color(rgb(0x313640))
            .flex()
            .items_center()
            .gap_3()
            .child(div().text_color(rgb(RED)).child(format!(
                "●  录制中  {}",
                time_text(self.stats.duration_seconds)
            )))
            .child(
                button(
                    "compact-pause",
                    if self.stats.state == RecordingState::Paused {
                        "继续"
                    } else {
                        "暂停"
                    },
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_pause();
                    cx.notify();
                })),
            )
            .child(
                button("compact-stop", "■ 停止并保存", false)
                    .danger()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_record(cx);
                    })),
            )
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
            let text = self
                .notes_base
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p.with_extension("notes.txt")).ok())
                .unwrap_or_default();
            self.notes_saved = text.clone();
            self.notes.update(cx, |s, cx| s.set_value(text, window, cx));
            self.notes_status = "笔记在本机自动保存；Word 通过下方按钮导出。".into();
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
    pub(super) fn meeting_workspace(&self, cx: &Context<Self>) -> Div {
        if self.meeting_page == MeetingPage::Library {
            return self.library_view(cx);
        }
        let title = self.transcript.title();
        let mut view = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        button("back-meetings", "← 会议文件", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.meeting_page = MeetingPage::Library;
                                this.refresh_meetings();
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        (MeetingPage::Transcript, "逐字稿"),
                        (MeetingPage::Minutes, "AI 会议纪要"),
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
            )
            .child(label(
                "逐字稿保留原话；AI 纪要单独生成并保留版本；个人笔记独立保存。",
            ));
        if let Some(base) = &self.transcript_output {
            let folder = base.parent().unwrap_or(Path::new(".")).to_path_buf();
            let open = folder.clone();
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div().flex().gap_2().child(
                            button("meeting-location", "在 Finder / 文件管理器中查看", false)
                                .ghost()
                                .on_click(move |_, _, _| {
                                    let _ = opencap_platform::open_path(&open);
                                }),
                        ),
                    )
                    .child(label(format!("保存位置：{}", folder.display()))),
            );
        }
        view.child(match self.meeting_page {
            MeetingPage::Transcript => self.transcription_view(cx),
            MeetingPage::Minutes => self.minutes_view(cx),
            MeetingPage::Notes => self.notes_view(cx),
            MeetingPage::Library => unreachable!(),
        })
    }
    fn library_view(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let mut view = div().flex().flex_col().gap_4()
            .child(heading("会议文件", "录制和导入的结果都在这里。选择一场会议，查看逐字稿、生成 AI 纪要或导出 Word。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("import-meeting", "+ 导入音频 / 视频", true).disabled(busy).on_click(cx.listener(|this,_,_,cx| this.choose_transcription_file(false,cx))))
                .child(button("refresh-meetings", "刷新列表", false).on_click(cx.listener(|this,_,_,cx| { this.refresh_meetings(); cx.notify(); })))
                .child(button("library-folder", "打开录制目录", false).ghost().on_click(cx.listener(|this,_,_,_| { let _ = opencap_platform::open_path(&this.config.output_dir); }))))
            .child(card().child(div().text_sm().child("生成方式：本地转录自动保存，AI 纪要需要手动生成。"))
                .child(label("开启实时转录后，声音和视频的转写流程相同。停止录制 → 查看逐字稿 → 分析说话人（可选）→ 生成 AI 纪要。")))
            .when(self.transcription_busy(), |d| d.child(card().child(label(self.transcription_status.clone())).child(button("current-task", "查看当前任务", false).on_click(cx.listener(|this,_,_,cx| { this.meeting_page=MeetingPage::Transcript; cx.notify(); })))));
        if self.meetings.is_empty() {
            view = view.child(
                card()
                    .child("还没有已保存的会议")
                    .child(label("可以先录制，也可以导入已有的 M4A、WAV、MP3 或视频。")),
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
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(meeting.title()),
                    )
                    .child(label(meeting.status()))
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
                                    true,
                                )
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
        card()
            .child(heading("个人笔记", "这里的内容由你填写，独立于 AI 纪要。"))
            .child(Textarea::new(&self.notes).disabled(self.transcript_output.is_none()))
            .child(label(self.notes_status.clone()))
            .child(
                button("export-notes-word", "导出笔记 Word", true)
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
                                    this.notes_status =
                                        format!("笔记 Word 已保存：{}", path.display());
                                    let _ = opencap_platform::open_path(&path);
                                }
                                Err(e) => this.notes_status = format!("导出失败：{e}"),
                            }
                        }
                        cx.notify();
                    })),
            )
    }
}
