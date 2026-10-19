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
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= 16_000_000,
        "文本超过 16 MB，请使用外部编辑器打开"
    );
    Ok(std::fs::read_to_string(path)?)
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
