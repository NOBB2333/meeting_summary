use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use opencap_core::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xfixes::ConnectionExt as _, xproto::*},
    rust_connection::RustConnection,
};

pub(super) fn connect() -> Result<(RustConnection, usize)> {
    x11rb::connect(None).context("无法连接 X11 桌面，请检查 DISPLAY 和当前会话")
}
pub(super) fn atom(c: &RustConnection, name: &str) -> Result<u32> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
#[derive(Clone, Copy)]
pub(super) struct Monitor {
    pub root: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}
pub(super) fn monitors(c: &RustConnection, screen: usize) -> Vec<Monitor> {
    let s = &c.setup().roots[screen];
    if let Ok(cookie) = c.randr_get_monitors(s.root, true)
        && let Ok(reply) = cookie.reply()
        && !reply.monitors.is_empty()
    {
        return reply
            .monitors
            .into_iter()
            .map(|m| Monitor {
                root: s.root,
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
            })
            .collect();
    }
    vec![Monitor {
        root: s.root,
        x: 0,
        y: 0,
        width: s.width_in_pixels,
        height: s.height_in_pixels,
    }]
}
pub fn targets() -> Result<Vec<TargetInfo>> {
    let (c, screen) = connect()?;
    let mut result: Vec<_> = monitors(&c, screen)
        .iter()
        .enumerate()
        .map(|(i, m)| TargetInfo {
            target: CaptureTarget::Monitor(i),
            name: format!("显示器 {}", i + 1),
            width: m.width.into(),
            height: m.height.into(),
        })
        .collect();
    let root = c.setup().roots[screen].root;
    let client_list = c
        .get_property(
            false,
            root,
            atom(&c, "_NET_CLIENT_LIST_STACKING")?,
            AtomEnum::WINDOW,
            0,
            4096,
        )?
        .reply()?;
    let windows: Vec<_> = client_list
        .value32()
        .map(|v| v.collect())
        .unwrap_or_else(|| {
            c.query_tree(root)
                .ok()
                .and_then(|r| r.reply().ok())
                .map(|r| r.children)
                .unwrap_or_default()
        });
    for window in windows.into_iter().rev() {
        let item = (|| -> Result<TargetInfo> {
            let attrs = c.get_window_attributes(window)?.reply()?;
            anyhow::ensure!(
                attrs.map_state == MapState::VIEWABLE && !attrs.override_redirect,
                "窗口不可见"
            );
            let pid = c
                .get_property(
                    false,
                    window,
                    atom(&c, "_NET_WM_PID")?,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )?
                .reply()?;
            anyhow::ensure!(
                pid.value32().and_then(|mut p| p.next()) != Some(std::process::id()),
                "忽略自身窗口"
            );
            let mut title = c
                .get_property(
                    false,
                    window,
                    atom(&c, "_NET_WM_NAME")?,
                    atom(&c, "UTF8_STRING")?,
                    0,
                    1024,
                )?
                .reply()?
                .value;
            if title.is_empty() {
                title = c
                    .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)?
                    .reply()?
                    .value;
            }
            anyhow::ensure!(!title.is_empty(), "无标题窗口");
            let g = c.get_geometry(window)?.reply()?;
            Ok(TargetInfo {
                target: CaptureTarget::Window(window as isize),
                name: String::from_utf8_lossy(&title).into_owned(),
                width: g.width.into(),
                height: g.height.into(),
            })
        })();
        if let Ok(item) = item {
            result.push(item);
        }
    }
    Ok(result)
}
pub fn capture_vendor(_: &CaptureTarget) -> Option<u32> {
    None
}
pub fn capture_window_id(id: isize) -> isize {
    id
}
pub fn window_region(id: isize) -> Result<CaptureTarget> {
    let (c, screen) = connect()?;
    let window = u32::try_from(id)?;
    let g = c.get_geometry(window)?.reply()?;
    let point = c.translate_coordinates(window, g.root, 0, 0)?.reply()?;
    for (index, m) in monitors(&c, screen).into_iter().enumerate() {
        let x = i32::from(point.dst_x) - i32::from(m.x);
        let y = i32::from(point.dst_y) - i32::from(m.y);
        if x >= 0
            && y >= 0
            && x + i32::from(g.width) <= i32::from(m.width)
            && y + i32::from(g.height) <= i32::from(m.height)
        {
            return Ok(CaptureTarget::Region {
                monitor: index,
                rect: Rect {
                    x: x as u32,
                    y: y as u32,
                    width: g.width.into(),
                    height: g.height.into(),
                },
            });
        }
    }
    anyhow::bail!("窗口超出单个显示器，请选择整个屏幕或框选区域")
}

fn frame(
    c: &RustConnection,
    screen: usize,
    target: &CaptureTarget,
    cursor: bool,
    pts_us: i64,
) -> Result<VideoFrame> {
    let (drawable, x, y, width, height, global_x, global_y) = match target {
        CaptureTarget::Window(id) => {
            let id = u32::try_from(*id)?;
            anyhow::ensure!(
                c.get_window_attributes(id)?.reply()?.map_state == MapState::VIEWABLE,
                "录制窗口已关闭或最小化"
            );
            let g = c.get_geometry(id)?.reply()?;
            let p = c.translate_coordinates(id, g.root, 0, 0)?.reply()?;
            (id, 0, 0, g.width, g.height, p.dst_x, p.dst_y)
        }
        CaptureTarget::Monitor(index) | CaptureTarget::Region { monitor: index, .. } => {
            let m = *monitors(c, screen).get(*index).context("显示器已移除")?;
            let r = match target {
                CaptureTarget::Region { rect, .. } => *rect,
                _ => Rect {
                    x: 0,
                    y: 0,
                    width: m.width.into(),
                    height: m.height.into(),
                },
            };
            anyhow::ensure!(
                r.width > 0
                    && r.height > 0
                    && r.x + r.width <= u32::from(m.width)
                    && r.y + r.height <= u32::from(m.height),
                "录制区域超出显示器"
            );
            let x = i16::try_from(i32::from(m.x) + i32::try_from(r.x)?)?;
            let y = i16::try_from(i32::from(m.y) + i32::try_from(r.y)?)?;
            (m.root, x, y, r.width as u16, r.height as u16, x, y)
        }
    };
    let image = c
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            x,
            y,
            width,
            height,
            u32::MAX,
        )?
        .reply()?;
    anyhow::ensure!(
        c.setup()
            .pixmap_formats
            .iter()
            .any(|f| f.depth == image.depth && f.bits_per_pixel == 32),
        "X11 录屏需要 24/32 位真彩色桌面"
    );
    let stride = usize::from(width) * 4;
    let mut data = image.data;
    anyhow::ensure!(
        data.len() == stride * usize::from(height),
        "X11 返回了不完整的画面"
    );
    for p in data.chunks_exact_mut(4) {
        if c.setup().image_byte_order == ImageOrder::MSB_FIRST {
            p.reverse();
        }
        p[3] = 255;
    }
    if cursor
        && let Ok(cookie) = c.xfixes_get_cursor_image()
        && let Ok(pointer) = cookie.reply()
    {
        let left = i32::from(pointer.x) - i32::from(pointer.xhot) - i32::from(global_x);
        let top = i32::from(pointer.y) - i32::from(pointer.yhot) - i32::from(global_y);
        for py in 0..pointer.height as usize {
            for px in 0..pointer.width as usize {
                let dx = left + px as i32;
                let dy = top + py as i32;
                if dx < 0 || dy < 0 || dx >= i32::from(width) || dy >= i32::from(height) {
                    continue;
                }
                let rgba = pointer.cursor_image[py * pointer.width as usize + px];
                let a = rgba >> 24;
                let offset = dy as usize * stride + dx as usize * 4;
                // XFixes cursor pixels are premultiplied ARGB.
                for (channel, shift) in [0, 8, 16].into_iter().enumerate() {
                    data[offset + channel] = (((rgba >> shift) & 255)
                        + u32::from(data[offset + channel]) * (255 - a) / 255)
                        .min(255) as u8;
                }
            }
        }
    }
    Ok(VideoFrame {
        width: width.into(),
        height: height.into(),
        stride,
        pts_us,
        data: Arc::new(data),
    })
}
pub struct LinuxCapture {
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    warnings: Vec<String>,
}
impl LinuxCapture {
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}
impl CaptureBackend for LinuxCapture {
    fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        Ok(())
    }
}
impl Drop for LinuxCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn start_capture(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<VideoFrame>,
    rx: Receiver<VideoFrame>,
    errors: Sender<String>,
    dropped: Arc<AtomicU64>,
) -> Result<LinuxCapture> {
    anyhow::ensure!(
        std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland"),
        "当前 Linux 录屏使用 X11；请登录 Deepin X11 会话。Wayland 仍可使用仅录音和文稿功能。"
    );
    anyhow::ensure!(
        !config.exclude_app,
        "X11 不支持排除本应用，请在录制偏好中关闭“隐藏本应用”，或选择目标应用窗口录制。"
    );
    let (c, screen) = connect()?;
    let _ = c
        .xfixes_query_version(5, 0)
        .ok()
        .and_then(|r| r.reply().ok());
    let first = frame(&c, screen, &config.target, config.cursor, clock.now_us())?;
    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (worker_stop, worker_finished) = (stop.clone(), finished.clone());
    let target = config.target.clone();
    let cursor = config.cursor;
    let interval = Duration::from_secs_f64(1. / f64::from(config.max_fps.max(1)));
    let worker = thread::Builder::new()
        .name("opencap-x11".into())
        .spawn(move || {
            let mut next = Some(first);
            while !worker_stop.load(Ordering::Acquire) {
                let began = Instant::now();
                if !clock.snapshot().1 {
                    let result = next
                        .take()
                        .map_or_else(|| frame(&c, screen, &target, cursor, clock.now_us()), Ok);
                    match result {
                        Ok(frame) => {
                            if tx.is_full() {
                                let _ = rx.try_recv();
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            if tx.try_send(frame).is_err() {
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            let _ = errors.try_send(format!("X11 采集结束：{e:#}"));
                            break;
                        }
                    }
                }
                while !worker_stop.load(Ordering::Acquire) && began.elapsed() < interval {
                    thread::sleep(
                        interval
                            .saturating_sub(began.elapsed())
                            .min(Duration::from_millis(20)),
                    );
                }
            }
            worker_finished.store(true, Ordering::Release);
        })?;
    Ok(LinuxCapture {
        stop,
        finished,
        worker: Some(worker),
        warnings: vec![],
    })
}
