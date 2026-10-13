//! Shared presentation. Business state and commands live in Recorder.
use super::*;
use gpui::component::{Icon, IconName, Theme, ThemeMode};

pub const BG: u32 = 0xf5f7fb;
pub const SURFACE: u32 = 0xffffff;
pub const TEXT: u32 = 0x202c41;
pub const MUTED: u32 = 0x617087;
pub const BORDER: u32 = 0xdce3ed;
pub const ACCENT: u32 = 0x365edc;
pub const SOFT: u32 = 0xeaf0ff;
pub const RED: u32 = 0xd83b4c;
pub const SUCCESS: u32 = 0x167553;

pub fn init_theme(cx: &mut App) {
    Theme::change(ThemeMode::Light, None, cx);
    Theme::update(cx, |theme| {
        theme.font_family = UI_FONT.into();
        theme.font_size = px(14.);
        theme.radius = px(8.);
        theme.radius_lg = px(12.);
        theme.background = rgb(BG).into();
        theme.foreground = rgb(TEXT).into();
        theme.border = rgb(BORDER).into();
        theme.primary = rgb(ACCENT).into();
        theme.primary_hover = rgb(0x2b50c6).into();
        theme.primary_active = rgb(0x2445ad).into();
        theme.primary_foreground = rgb(SURFACE).into();
        theme.button_primary = rgb(ACCENT).into();
        theme.button_primary_hover = rgb(0x2b50c6).into();
        theme.button_primary_active = rgb(0x2445ad).into();
        theme.button_primary_foreground = rgb(SURFACE).into();
        theme.button_danger = rgb(RED).into();
        theme.button_danger_hover = rgb(0xc3293e).into();
        theme.button_danger_active = rgb(0xad2436).into();
        theme.button_danger_foreground = rgb(SURFACE).into();
        theme.button_foreground = rgb(TEXT).into();
        theme.muted = rgb(BG).into();
        theme.muted_foreground = rgb(MUTED).into();
        theme.accent = rgb(SOFT).into();
        theme.accent_foreground = rgb(ACCENT).into();
        theme.danger = rgb(RED).into();
        theme.shadow = false;
    });
}

/// Selected options are intentionally quieter than an action button.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, active: bool) -> Button {
    Button::new(id)
        .label(label)
        .outline()
        .selected(active)
        .h(px(36.))
        .px_3()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .when(active, |b| {
            b.bg(rgb(SOFT))
                .text_color(rgb(ACCENT))
                .border_color(rgb(0xbacaf8))
        })
}

pub fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id)
        .label(label)
        .primary()
        .h(px(40.))
        .px_4()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn label(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
}

pub fn icon(name: IconName) -> Icon {
    Icon::new(name).size(px(18.))
}

pub fn nav_button(
    id: impl Into<ElementId>,
    title: &'static str,
    name: IconName,
    selected: bool,
) -> Button {
    Button::new(id)
        .accessibility_label(title)
        .ghost()
        .selected(selected)
        .h(px(42.))
        .w_full()
        .px_3()
        .when(selected, |b| b.bg(rgb(SOFT)).text_color(rgb(ACCENT)))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .child(icon(name))
                .child(title),
        )
}

pub fn card() -> Div {
    div()
        .w_full()
        .min_w(px(0.))
        .p_5()
        .rounded_xl()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .gap_4()
}

pub fn heading(title: &str, description: &str) -> Div {
    div()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(label(description.to_owned()))
}

pub fn section_title(title: &str, description: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .when(!description.is_empty(), |d| {
            d.child(label(description.to_owned()))
        })
}

pub fn setting_row(title: &str, description: &str, control: impl IntoElement) -> Div {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .child(section_title(title, description).flex_1().min_w(px(180.)))
        .child(control)
}

pub fn badge(text: impl Into<SharedString>, color: u32, background: u32) -> Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(background))
        .text_xs()
        .text_color(rgb(color))
        .child(text.into())
}

pub fn task_progress(
    id: &'static str,
    title: &str,
    status: &str,
    clock: &task::TaskClock,
    fraction: Option<f64>,
) -> Div {
    use gpui::component::progress::Progress;
    div()
        .p_4()
        .rounded_lg()
        .bg(rgb(SOFT))
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(section_title(title, status).flex_1().min_w(px(0.)))
                .child(
                    div()
                        .font_family(MONO_FONT)
                        .text_2xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(ACCENT))
                        .child(fraction.map_or_else(
                            || "处理中".to_owned(),
                            |p| format!("{:.0}%", p.clamp(0., 1.) * 100.),
                        )),
                ),
        )
        .child(
            Progress::new(id)
                .accessibility_label(title.to_owned())
                .color(rgb(ACCENT))
                .value((fraction.unwrap_or(0.) * 100.) as f32)
                .loading(fraction.is_none()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_between()
                .gap_2()
                .child(label(format!(
                    "已用时 {}",
                    task::duration_text(clock.elapsed())
                )))
                .child(label(clock.remaining().map_or_else(
                    || {
                        if fraction.is_some() {
                            "估计剩余：正在计算".into()
                        } else {
                            "当前阶段无法可靠估计剩余时间".into()
                        }
                    },
                    |d| format!("估计剩余 {}", task::duration_text(d)),
                ))),
        )
}

pub fn sidebar_width(viewport: f32) -> f32 {
    if viewport >= 900. { 208. } else { 0. }
}
pub fn content_width(viewport: f32) -> f32 {
    (viewport - sidebar_width(viewport) - 56.).max(1.)
}
pub fn two_columns(content: f32) -> bool {
    content >= 820.
}
