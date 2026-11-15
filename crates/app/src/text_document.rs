//! Plain-text artifacts stay inside the application; system associations are optional.
use super::*;
use gpui::component::input::{Textarea, TextareaState};

pub(super) struct TextDocument {
    path: PathBuf,
    input: Entity<TextareaState>,
    pending: Option<anyhow::Result<String>>,
    status: String,
}
impl TextDocument {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            input: cx.new(|cx| TextareaState::new(window, cx).searchable(true)),
            pending: None,
            status: "正在读取文件…".into(),
        }
    }
}
pub(super) fn read_text(path: &std::path::Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut file = opencap_core::open_regular_file_for_read(path)?;
    let before = file.metadata()?;
    anyhow::ensure!(
        before.len() <= 16_000_000,
        "文本超过 16 MB，请使用外部编辑器打开"
    );
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    (&mut file).take(16_000_001).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16_000_000,
        "文本超过 16 MB，请使用外部编辑器打开"
    );
    let after = file.metadata()?;
    anyhow::ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && same_file::Handle::from_file(opencap_core::open_regular_file_for_read(path)?)?
                == identity,
        "文件在读取期间发生变化，请重新打开"
    );
    let text = String::from_utf8(bytes)?;
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(".transcript.run-"))
    {
        let transcript: opencap_asr::Transcript = serde_json::from_str(&text)?;
        let mut readable = if transcript
            .extra
            .get("history_snapshot_kind")
            .and_then(|v| v.as_str())
            == Some("manual_recovery_current_draft")
        {
            String::from(
                "当前稿恢复副本 · 只读\n此副本可能包含识别后的修改，不能作为原始识别快照。\n\n",
            )
        } else {
            String::from("历史识别版本 · 只读\n\n")
        };
        if let Some(run) = &transcript.run {
            readable.push_str(&format!(
                "运行：{}\n模型：{}\n状态：{}\n\n",
                run.run_id, run.model_id, run.state
            ));
        } else {
            readable.push_str("本版本未记录识别配置。\n\n");
        }
        let timeline = opencap_asr::SpeakerTimeline::new(&transcript);
        for segment in &transcript.segments {
            readable.push_str(&format!(
                "[{}–{}] {}\n{}\n\n",
                opencap_asr::timestamp(segment.start_ms, '.'),
                opencap_asr::timestamp(segment.end_ms, '.'),
                timeline.label(segment.start_ms, segment.end_ms),
                segment.text
            ));
        }
        Ok(readable)
    } else if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(".summary.recovery-"))
    {
        let value: serde_json::Value = serde_json::from_str(&text)?;
        if value.get("kind").and_then(|kind| kind.as_str()) == Some("manual_minutes_draft") {
            let draft = value
                .get("text")
                .and_then(|text| text.as_str())
                .ok_or_else(|| anyhow::anyhow!("恢复文件缺少草稿"))?;
            return Ok(format!(
                "手工纪要草稿恢复副本 · 只读\n请核对后复制需要保留的内容；不会自动覆盖当前版本。\n\n{draft}"
            ));
        }
        anyhow::ensure!(
            value.get("kind").and_then(|kind| kind.as_str()) == Some("uncommitted_minutes_result"),
            "恢复文件类型不正确"
        );
        let history: opencap_asr::MinutesHistory = serde_json::from_value(
            value
                .get("history")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("恢复文件缺少纪要内容"))?,
        )?;
        let mut readable = String::from(
            "未提交的纪要恢复副本 · 只读\n请核对后复制需要保留的内容；不会自动覆盖当前版本。\n\n",
        );
        if let Some(turn) = history.conversation.last() {
            readable.push_str(&format!(
                "请求：\n{}\n\n生成回复：\n{}\n",
                turn.request, turn.reply
            ));
        } else if !history.latest().is_empty() {
            readable.push_str(history.latest());
        } else {
            readable.push_str("此恢复副本没有可显示的正文或回复。");
        }
        Ok(readable)
    } else {
        Ok(text)
    }
}
impl Render for TextDocument {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(result) = self.pending.take() {
            match result {
                Ok(text) => {
                    self.status = "只读预览 · 可拖选、全选和复制；Ctrl+F / ⌘F 搜索".into();
                    self.input.update(cx, |s, cx| s.set_value(text, window, cx));
                }
                Err(error) => self.status = format!("文件无法预览：{error:#}"),
            }
        }
        let input = self.input.clone();
        ui::card()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .h_full()
            .min_h(px(0.))
            .w_full()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        label(self.path.display().to_string())
                            .flex_1()
                            .min_w(px(0.))
                            .truncate(),
                    )
                    .child(button("copy-text-document", "复制全文", false).on_click(
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                input.read(cx).value().to_string(),
                            ))
                        },
                    )),
            )
            .child(label(self.status.clone()))
            .child(
                Textarea::new(&self.input)
                    .readonly(true)
                    .w_full()
                    .flex_1()
                    .min_h(px(160.)),
            )
    }
}
impl Recorder {
    pub(super) fn open_history_text_document(
        &mut self,
        base: PathBuf,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let document = cx.new(|cx| TextDocument::new(path.clone(), window, cx));
        let reader = document.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let meeting_id =
                        super::transcription::verify_transcript_history_member(&base, &path)?;
                    let text = read_text(&path)?;
                    anyhow::ensure!(
                        super::transcription::verify_transcript_history_member(&base, &path)?
                            == meeting_id,
                        "读取期间历史版本的会议身份发生变化，请刷新列表"
                    );
                    Ok::<_, anyhow::Error>(text)
                })
                .await;
            reader.update(cx, |this, cx| {
                this.pending = Some(result);
                cx.notify();
            });
        })
        .detach();
        self.text_document = Some(document);
    }
    pub(super) fn create_text_document(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextDocument> {
        let document = cx.new(|cx| TextDocument::new(path.clone(), window, cx));
        let reader = document.clone();

        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_text(&path) })
                .await;
            reader.update(cx, |this, cx| {
                this.pending = Some(result);
                cx.notify();
            });
        })
        .detach();
        document
    }
    pub(super) fn open_text_document(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text_document = Some(self.create_text_document(path, window, cx));
    }
}
