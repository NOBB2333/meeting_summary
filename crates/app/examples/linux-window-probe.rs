//! Minimal native window for isolating Linux event-loop and graphics failures.
use gpui_kit::{self as gpui, *};

struct Probe;
impl Render for Probe {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        eprintln!(
            "probe render: {:?}, {:?}",
            window.viewport_size(),
            window.visibility()
        );
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0xf4f7fb))
            .text_color(rgb(0x173659))
            .text_2xl()
            .child("OpenCap Linux window probe · 窗口绘制检查")
    }
}

fn main() {
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("{}: {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Logger = Logger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
    gpui::application()
        .with_assets(gpui::assets::Assets)
        .run(|cx| {
            gpui::init(cx);
            let bounds = Bounds::centered(None, size(px(1200.), px(820.)), cx);
            gpui::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("OpenCap".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Probe),
            )
            .expect("create probe window");
            cx.activate(true);
        });
}
