use gpui::{prelude::*, *};

/// Card widths stay readable; gaps and the outer scrollbar are accounted by the caller.
pub fn target_columns(width: f32) -> usize {
    ((width + 12.) / (220. + 12.)).floor().max(1.) as usize
}

pub fn scrollable(id: &'static str, content: impl IntoElement, scroll: &ScrollHandle) -> Div {
    use gpui::component::scroll::ScrollableElement;
    div().size_full().min_h(px(0.)).min_w(px(0.)).child(
        div()
            .id(id)
            .size_full()
            .min_w(px(0.))
            .overflow_y_scroll()
            .track_scroll(scroll)
            .child(content)
            .vertical_scrollbar(scroll),
    )
}

pub fn preview_view(preview: std::rc::Rc<opencap_platform::WindowPreview>) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let scale = window.scale_factor();
            let rect = |b: Bounds<Pixels>| {
                [
                    f32::from(b.left()) * scale,
                    f32::from(b.top()) * scale,
                    f32::from(b.right()) * scale,
                    f32::from(b.bottom()) * scale,
                ]
                .map(|v| v.round() as i32)
            };
            let _ = preview.place(rect(bounds), rect(window.content_mask().bounds));
        },
        |_, _, _, _| (),
    )
    .size_full()
}
