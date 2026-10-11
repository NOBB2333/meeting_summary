use gpui::{prelude::*, *};

/// Card widths stay readable; gaps and the outer scrollbar are accounted by the caller.
pub fn target_columns(width: f32) -> usize {
    ((width + 12.) / (220. + 12.)).floor().max(1.) as usize
}

pub fn scrollable(id: &'static str, content: impl IntoElement, scroll: &ScrollHandle) -> Div {
    let paint_scroll = scroll.clone();
    let click_scroll = scroll.clone();
    let drag_scroll = scroll.clone();
    div()
        .flex()
        .size_full()
        .min_h(px(0.))
        .min_w(px(0.))
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(content),
        )
        .child(
            div()
                .id((id, 1usize))
                .w(px(10.))
                .h_full()
                .flex_shrink_0()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |event, window, _| {
                    scroll_at(&click_scroll, event.position.y, window)
                })
                .on_mouse_move(move |event, window, _| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        scroll_at(&drag_scroll, event.position.y, window);
                    }
                })
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            let max = f32::from(paint_scroll.max_offset().height);
                            if max <= 0. {
                                return;
                            }
                            let height = f32::from(bounds.size.height);
                            let thumb = (height * height / (height + max)).max(28.).min(height);
                            let top = (-f32::from(paint_scroll.offset().y) / max).clamp(0., 1.)
                                * (height - thumb);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(bounds.left() + px(2.), bounds.top() + px(top)),
                                    size(px(5.), px(thumb)),
                                ),
                                rgb(0x555961),
                            ));
                        },
                    )
                    .size_full(),
                ),
        )
}
fn scroll_at(scroll: &ScrollHandle, y: Pixels, window: &mut Window) {
    let bounds = scroll.bounds();
    let max = f32::from(scroll.max_offset().height);
    let height = f32::from(bounds.size.height);
    if height <= 0. || max <= 0. {
        return;
    }
    let thumb = (height * height / (height + max)).max(28.).min(height);
    let ratio =
        ((f32::from(y - bounds.top()) - thumb / 2.) / (height - thumb).max(1.)).clamp(0., 1.);
    scroll.set_offset(point(px(0.), px(-ratio * max)));
    window.refresh();
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
