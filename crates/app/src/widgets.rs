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

/// Let a pane handle the wheel first, then pass it to the page at its edge.
pub fn pane_scrollable(id: &'static str, content: impl IntoElement, scroll: &ScrollHandle) -> Div {
    let handle = scroll.clone();
    div()
        .size_full()
        .min_h(px(0.))
        .on_scroll_wheel(move |event, _, cx| {
            let delta = event.delta.pixel_delta(px(20.)).y;
            let offset = handle.offset().y;
            let max = handle.max_offset().y;
            if (delta > px(0.) && offset < px(0.)) || (delta < px(0.) && offset > -max) {
                cx.stop_propagation();
            }
        })
        .child(scrollable(id, content, scroll))
}

/// Scroll the inner chooser without moving either surrounding workspace.
pub fn contained_scrollable(
    id: &'static str,
    content: impl IntoElement,
    scroll: &ScrollHandle,
) -> Div {
    div()
        .size_full()
        .min_h(px(0.))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(scrollable(id, content, scroll))
}

/// Fit the real image without cropping or fixing the preview to a pixel height.
pub fn image_aspect(image: Option<&std::sync::Arc<RenderImage>>) -> f32 {
    image
        .map(|image| image.size(0))
        .filter(|size| size.width.0 > 0 && size.height.0 > 0)
        .map(|size| size.width.0 as f32 / size.height.0 as f32)
        .unwrap_or(16. / 9.)
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

#[cfg(test)]
mod tests {
    use super::image_aspect;
    use gpui::RenderImage;
    use std::sync::Arc;

    #[test]
    fn preview_preserves_landscape_and_portrait_aspect() {
        let image = |width, height| {
            Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::new(width, height),
            )]))
        };
        assert!((image_aspect(Some(&image(160, 90))) - 16. / 9.).abs() < 0.001);
        assert!((image_aspect(Some(&image(90, 160))) - 9. / 16.).abs() < 0.001);
        assert!((image_aspect(None) - 16. / 9.).abs() < 0.001);
    }
}
