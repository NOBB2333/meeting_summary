//! Opt-in developer UI regression harness. Exercises real views without touching saved settings.
use super::*;
use anyhow::Result;

pub fn output_dir() -> Option<PathBuf> {
    let mut args = std::env::args();
    args.find(|arg| arg == "--ui-smoke")?;
    args.next().map(PathBuf::from)
}
/// Manual visual review shares the same isolated configuration as the harness.
pub fn isolated_dir() -> Option<PathBuf> {
    let mut args = std::env::args();
    args.find(|arg| arg == "--ui-smoke" || arg == "--ui-preview")?;
    args.next().map(PathBuf::from)
}
pub fn preview_size() -> Option<Size<Pixels>> {
    let mut args = std::env::args();
    args.find(|arg| arg == "--ui-size")?;
    let value = args.next()?;
    let (width, height) = value.split_once('x')?;
    Some(size(
        px(width.parse::<f32>().ok()?.max(620.)),
        px(height.parse::<f32>().ok()?.max(480.)),
    ))
}
pub fn preview_source() -> Option<PathBuf> {
    isolated_dir()?;
    let mut args = std::env::args();
    args.find(|arg| arg == "--ui-open")?;
    args.next().map(PathBuf::from)
}

/// Exercise the actual GPUI list, and record how many different rows it asked
/// to lay out at each position. This is not an FPS or recording benchmark.
pub fn scroll_benchmark(
    main: WindowHandle<gpui::base::Root>,
    entity: Entity<Recorder>,
    cx: &mut App,
) {
    let Some(path) = isolated_dir().filter(|_| std::env::args().any(|a| a == "--ui-scroll-bench"))
    else {
        return;
    };
    cx.spawn(async move |cx| {
        let result = async {
            for _ in 0..20 {
                if entity.read_with(cx, |r, cx| r.transcript_view.read(cx).state.viewport_bounds().size.height > px(0.)) { break; }
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
            let count = entity.read_with(cx, |r, _| r.transcript.segments.len());
            anyhow::ensure!(count > 0, "load a transcript with --ui-open first");
            let mut samples = Vec::new();
            for width in [1200., 620.] {
                main.update(cx, |_, window, _| window.resize(size(px(width), px(820.))))?;
                futures_timer::Delay::new(Duration::from_millis(100)).await;
                for index in [0, count / 4, count / 2, count.saturating_sub(1)] {
                    entity.update(cx, |r, cx| {
                        r.transcript_view.update(cx, |view, cx| {
                            view.measured_rows.borrow_mut().clear();
                            view.state.scroll_to(ListOffset { item_ix: index, offset_in_item: px(0.) });
                            cx.notify();
                        });
                    });
                    for _ in 0..10 {
                        futures_timer::Delay::new(Duration::from_millis(100)).await;
                        if entity.read_with(cx, |r, cx| !r.transcript_view.read(cx).measured_rows.borrow().is_empty()) { break; }
                    }
                    let measured = entity.read_with(cx, |r, cx| r.transcript_view.read(cx).measured_rows.borrow().len());
                    anyhow::ensure!(measured > 0 && measured < count.min(100), "list laid out {measured}/{count} rows");
                    samples.push(serde_json::json!({"width":width,"index":index,"distinct_rows_requested":measured}));
                }
            }
            std::fs::write(path.join("scroll-benchmark.json"), serde_json::to_vec_pretty(&serde_json::json!({"passed":true,"source_rows":count,"samples":samples,"definition":"distinct row layout requests per programmed scroll position; not FPS"}))?)?;
            Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = result { let _ = std::fs::write(path.join("scroll-benchmark-error.txt"), format!("{error:#}")); }
    }).detach();
}
struct Sample {
    index: usize,
}
impl Render for Sample {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_5()
            .bg(rgb([0x133047, 0x363029, 0x253c30, 0x352741][self.index % 4]))
            .text_color(rgb(0xffffff))
            .font_family(UI_FONT)
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_2xl()
                    .child(format!("窗口预览样本 {}", self.index + 1)),
            )
            .child("这是真实窗口内容 · 中文 / Aa 0123456789")
            .children((0..6).map(|i| {
                div()
                    .h(px(20.))
                    .w(px(150. + i as f32 * 25.))
                    .bg(rgb(0x7097af))
            }))
    }
}
async fn shot(hwnd: isize, path: PathBuf) -> Result<()> {
    shot_target(
        CaptureTarget::Window(opencap_platform::capture_window_id(hwnd)),
        path,
    )
    .await
}
async fn shot_target(target: CaptureTarget, path: PathBuf) -> Result<()> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let config = RecordingConfig {
                target,
                output_dir: path,
                encoder: Some("libx264".into()),
                codec: Codec::H264,
                system_audio: false,
                microphone: false,
                exclude_app: false,
                quality: 18,
                ..Default::default()
            };
            let mut session = RecordingSession::start(config)?;
            let deadline = std::time::Instant::now();
            while session.stats().state == RecordingState::Starting
                && deadline.elapsed() < Duration::from_secs(15)
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            anyhow::ensure!(
                session.stats().state == RecordingState::Recording,
                "Snapshot capture failed: {:?}",
                session.stats().error
            );
            std::thread::sleep(Duration::from_millis(600));
            session.command(RecordingCommand::Stop);
            session.join();
            anyhow::ensure!(
                session.stats().state == RecordingState::Finished,
                "Snapshot failed"
            );
            Ok(())
        })();
        let _ = tx.send(result);
    });
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                anyhow::bail!("Snapshot worker exited")
            }
            Err(_) => futures_timer::Delay::new(Duration::from_millis(100)).await,
        }
    }
}
pub fn run(
    main: WindowHandle<gpui::base::Root>,
    recorder: Entity<Recorder>,
    path: PathBuf,
    cx: &mut App,
) {
    #[cfg(any(windows, target_os = "linux"))]
    let exercise_microphone = true;
    #[cfg(target_os = "macos")]
    let exercise_microphone = opencap_platform::microphone_permission_granted();
    let mut sources = Vec::new();
    let mut sample_windows = Vec::new();
    for index in 0..23 {
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(
                            px(40. + (index % 4) as f32 * 35.),
                            px(60. + (index % 4) as f32 * 35.),
                        ),
                        size(px(380.), px(270.)),
                    ))),
                    titlebar: Some(TitlebarOptions {
                        title: Some(format!("OpenCap UI sample {}", index + 1).into()),
                        ..Default::default()
                    }),
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Sample { index }),
            )
            .unwrap();
        let hwnd = window.update(cx, |_, w, _| native_handle(w)).unwrap();
        sample_windows.push(window);
        sources.push(TargetInfo {
            target: CaptureTarget::Window(opencap_platform::capture_window_id(hwnd)),
            name: format!("预览样本 {}", index + 1),
            width: 380,
            height: 270,
        });
    }
    main.update(cx, |_, w, _| w.activate_window()).unwrap();
    cx.spawn(async move |cx| {
        let result: Result<()> = async {
            std::fs::create_dir_all(&path)?;
            futures_timer::Delay::new(Duration::from_secs(1)).await;
            let hwnd = recorder.read_with(cx,|r,_|r.hwnd);
            shot(hwnd,path.join("01-main")).await?;
            recorder.update(cx,|r,cx| {
                r.mode=1; r.targets=sources.clone(); r.choosing_target=true;
                r.config.target=sources[0].target.clone();
                for source in &sources { if let CaptureTarget::Window(id)=source.target {
                    r.previews.insert(id,Rc::new(opencap_platform::WindowPreview::new(hwnd,id).unwrap()));
                }}
                cx.notify();
            });
            futures_timer::Delay::new(Duration::from_millis(500)).await;
            shot(hwnd,path.join("02-previews")).await?;
            main.update(cx, |_,w,_| { w.zoom_window(); })?;
            futures_timer::Delay::new(Duration::from_millis(500)).await;
            let wide_columns = main.update(cx,|_,w,_| widgets::target_columns(ui::content_width(f32::from(w.viewport_size().width))-40.))?;
            anyhow::ensure!(wide_columns > 2,"Maximized picker is still limited to two columns");
            shot(hwnd,path.join("02b-maximized-23-targets")).await?;
            main.update(cx,|_,w,_| { w.zoom_window(); })?;
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            main.update(cx,|_,w,cx| { w.resize(size(px(620.),px(700.))); recorder.update(cx,|r,cx| { r.page_scroll.set_offset(point(px(0.),px(0.))); cx.notify(); }); })?;
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            shot(hwnd,path.join("02c-narrow-targets")).await?;
            recorder.update(cx,|r,cx| { r.page_scroll.scroll_to_bottom(); cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            shot(hwnd,path.join("02d-last-target")).await?;
            recorder.update(cx,|r,cx| { r.targets_expanded=false; r.page_scroll.set_offset(point(px(0.),px(0.))); r.target_scroll.set_offset(point(px(0.),px(-170.))); cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            shot(hwnd,path.join("03-scrolled-previews")).await?;
            main.update(cx,|_,w,cx| { w.resize(size(px(620.),px(480.))); recorder.update(cx,|r,cx| { r.close_targets(); r.settings=true; r.settings_page=settings::SettingsPage::Recording; cx.notify(); }); })?;
            futures_timer::Delay::new(Duration::from_millis(500)).await;
            recorder.update(cx,|r,cx| { r.page_scroll.scroll_to_bottom(); cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            shot(hwnd,path.join("04-small-settings")).await?;
            main.update(cx,|_,w,cx| { w.resize(size(px(960.),px(850.))); recorder.update(cx,|r,cx| { r.settings=false; r.page_scroll.set_offset(point(px(0.),px(0.))); r.config.target=sources[0].target.clone(); r.config.border_mode=BorderMode::Always; r.toggle_record(cx); }); })?;
            let start=std::time::Instant::now();
            loop {
                let state=recorder.read_with(cx,|r,_|r.stats.state);
                if state==RecordingState::Recording {break;}
                anyhow::ensure!(state!=RecordingState::Failed && start.elapsed()<Duration::from_secs(20),"UI start failed");
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
            recorder.update(cx,|r,cx| { r.toggle_audio(AudioKind::System); if exercise_microphone { r.toggle_audio(AudioKind::Microphone); } cx.notify(); });
            futures_timer::Delay::new(Duration::from_secs(2)).await;
            anyhow::ensure!(recorder.read_with(cx,|r,_|r.preview_image.is_some() && r.preview_pts>0),"Recording preview did not receive encoder frames");
            anyhow::ensure!(recorder.read_with(cx,|r,_|r.indicator.as_ref().is_some_and(|i|i.visible())),"Always-on border did not appear");
            sample_windows[0].update(cx, |_,w,_|w.minimize_window())?;
            futures_timer::Delay::new(Duration::from_millis(600)).await;
            anyhow::ensure!(recorder.read_with(cx,|r,_|r.indicator.as_ref().is_none_or(|i|!i.visible())),"Border remained after minimizing target");
            sample_windows[0].update(cx, |_,w,_|w.activate_window())?;
            main.update(cx,|_,w,_|w.activate_window())?;
            recorder.update(cx,|r,cx| { r.config.border_mode=BorderMode::OnStart; r.boundary_started=Some(std::time::Instant::now()-Duration::from_secs(4)); r.sync_indicator(); cx.notify(); });
            anyhow::ensure!(recorder.read_with(cx,|r,_|r.indicator.is_none()),"Timed border remained after timeout");
            recorder.update(cx,|r,cx| { r.config.border_mode=BorderMode::Never; r.sync_indicator(); cx.notify(); });
            anyhow::ensure!(recorder.read_with(cx,|r,_|r.indicator.is_none()),"Disabled border remained");
            recorder.update(cx,|r,cx| { r.toggle_pause(); cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(500)).await;
            recorder.update(cx,|r,cx| { r.choose_device(AudioKind::System,r.devices.iter().find(|d|d.kind==AudioKind::System).map(|d|d.id.clone())); if exercise_microphone { r.toggle_audio(AudioKind::Microphone); } cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            recorder.update(cx,|r,cx| { r.toggle_pause(); cx.notify(); });
            futures_timer::Delay::new(Duration::from_secs(1)).await;
            // Do not run a second capture while recording. Capture the paused UI after stopping.
            recorder.update(cx,|r,cx| r.toggle_record(cx));
            let stop=std::time::Instant::now();
            loop {
                let state=recorder.read_with(cx,|r,_|r.stats.state);
                if state==RecordingState::Finished {break;}
                anyhow::ensure!(state!=RecordingState::Failed && stop.elapsed()<Duration::from_secs(15),"UI stop failed");
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
            shot(hwnd,path.join("05-finished")).await?;
            recorder.update(cx,|r,cx| { r.audio_picker=Some(AudioKind::System); r.page_scroll.set_offset(point(px(0.),px(-190.))); cx.notify(); });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            shot(hwnd,path.join("06-audio-picker")).await?;
            let float_hwnd=recorder.read_with(cx,|r,_|r.floating_hwnd);
            anyhow::ensure!(!opencap_platform::native_window_visible(float_hwnd),"Floating controls unexpectedly appeared by default");
            recorder.update(cx,|r,cx| { r.config.show_floating_controls=true; opencap_platform::show_controls_without_activation(r.floating_hwnd,true); if let Some(w)=r.floating { let _=w.update(cx,|_,_,cx|cx.notify()); } });
            futures_timer::Delay::new(Duration::from_millis(300)).await;
            let region = opencap_platform::window_region(float_hwnd)?;
            if let CaptureTarget::Region { rect, .. } = region { anyhow::ensure!(rect.width > rect.height * 5 && rect.height <= 250,"Toolbar has incorrect size: {rect:?}"); }
            shot_target(region,path.join("07-floating-controls")).await?;
            let stats=recorder.read_with(cx,|r,_|r.stats.clone());
            std::fs::write(path.join("ui-smoke.json"),serde_json::to_vec_pretty(&serde_json::json!({"passed":true,"stats":stats,"microphone_exercised":exercise_microphone,"maximized_columns":wide_columns,"scenarios":["23 real native-window previews","responsive expanded grid and last target","compact scrolling","small-window settings","live encoder preview","minimized target hides border","timed and disabled borders","UI start/pause/resume/stop","live audio and device switch"]}))?)?;
            Ok(())
        }.await;
        if let Err(e)=result { let _=std::fs::write(path.join("ui-smoke-error.txt"),format!("{e:#}")); }
        cx.update(|cx|cx.quit());
    }).detach();
}
