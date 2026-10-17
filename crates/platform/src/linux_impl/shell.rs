use super::capture::{atom, connect, monitors};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, unbounded};
use opencap_core::*;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{Event, xproto::*},
    wrapper::ConnectionExt as _,
};

#[derive(Debug)]
pub enum ShellEvent {
    Show,
    Record,
    Pause,
    OpenFolder,
    Quit,
    Locked,
    Warning(String),
}
pub struct DesktopControls {
    pub events: Receiver<ShellEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl DesktopControls {
    pub fn has_tray(&self) -> bool {
        false
    }
    pub fn set_recording_state(&self, _: bool) {}
}
impl Drop for DesktopControls {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn keycode(c: &x11rb::rust_connection::RustConnection, keysym: u32) -> Result<u8> {
    let setup = c.setup();
    let first = setup.min_keycode;
    let mapping = c
        .get_keyboard_mapping(first, setup.max_keycode - first + 1)?
        .reply()?;
    mapping
        .keysyms
        .chunks(mapping.keysyms_per_keycode as usize)
        .position(|row| row.contains(&keysym))
        .map(|i| first + i as u8)
        .context("键盘布局没有对应的快捷键")
}
fn hotkey(c: &x11rb::rust_connection::RustConnection, text: &str) -> Result<(u8, ModMask)> {
    let mut mask = ModMask::default();
    let mut key = None;
    for part in text.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mask |= ModMask::CONTROL,
            "alt" => mask |= ModMask::M1,
            "shift" => mask |= ModMask::SHIFT,
            "super" | "win" | "meta" => mask |= ModMask::M4,
            _ => {
                let lower = part.to_ascii_lowercase();
                let symbol = if let Some(number) = lower
                    .strip_prefix('f')
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|v| (1..=12).contains(v))
                {
                    0xffbe + number - 1
                } else {
                    anyhow::ensure!(
                        part.len() == 1 && part.is_ascii(),
                        "快捷键支持字母、数字和 F1–F12"
                    );
                    part.to_ascii_lowercase().as_bytes()[0] as u32
                };
                key = Some(keycode(c, symbol)?);
            }
        }
    }
    Ok((key.context("快捷键缺少按键")?, mask))
}
pub fn desktop_controls(record: &str, pause: &str) -> Result<DesktopControls> {
    let (c, screen) = connect()?;
    let root = c.setup().roots[screen].root;
    let keys = [hotkey(&c, record)?, hotkey(&c, pause)?];
    for (code, mask) in keys {
        for lock in [
            ModMask::default(),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ] {
            c.grab_key(
                false,
                root,
                mask | lock,
                code,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()
            .context("全局快捷键被其他应用占用")?;
        }
    }
    c.flush()?;
    let (tx, events) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            match c.poll_for_event() {
                Ok(Some(Event::KeyPress(e))) => {
                    let _ = tx.try_send(if e.detail == keys[0].0 {
                        ShellEvent::Record
                    } else {
                        ShellEvent::Pause
                    });
                }
                Ok(None) => thread::sleep(Duration::from_millis(30)),
                Ok(Some(_)) => (),
                Err(_) => break,
            }
        }
    });
    Ok(DesktopControls {
        events,
        stop,
        worker: Some(worker),
    })
}
pub fn set_app_icon(id: isize, recording: bool) {
    let result = (|| -> Result<()> {
        let (c, _) = connect()?;
        let bytes: &[u8] = if recording {
            include_bytes!("../../../app/resources/opencap-recording.jpg")
        } else {
            include_bytes!("../../../app/resources/opencap-idle.jpg")
        };
        let image =
            image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.to_rgba8();
        let mut data = vec![image.width(), image.height()];
        data.extend(image.pixels().map(|p| {
            u32::from(p[3]) << 24 | u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2])
        }));
        c.change_property32(
            PropMode::REPLACE,
            id as u32,
            atom(&c, "_NET_WM_ICON")?,
            AtomEnum::CARDINAL,
            &data,
        )?
        .check()?;
        c.change_property8(
            PropMode::REPLACE,
            id as u32,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"opencap\0OpenCap\0",
        )?
        .check()?;
        c.flush()?;
        Ok(())
    })();
    let _ = result;
}
pub fn set_capture_exclusion(_: isize, exclude: bool) -> Result<()> {
    anyhow::ensure!(!exclude, "X11 不支持排除本应用；可选择目标窗口录制");
    Ok(())
}
pub fn show_native_window(id: isize, value: bool) {
    if let Ok((c, _)) = connect() {
        if value {
            let _ = c.map_window(id as u32);
            let _ = c.configure_window(
                id as u32,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            );
        } else {
            let _ = c.unmap_window(id as u32);
        }
        let _ = c.flush();
    }
}
pub fn show_controls_without_activation(id: isize, value: bool) {
    show_native_window(id, value);
}
pub fn native_window_visible(id: isize) -> bool {
    connect()
        .ok()
        .and_then(|(c, _)| c.get_window_attributes(id as u32).ok()?.reply().ok())
        .is_some_and(|a| a.map_state == MapState::VIEWABLE)
}
pub fn configure_floating_controls(id: isize, owner: isize) -> Result<()> {
    let (c, _) = connect()?;
    c.change_property32(
        PropMode::REPLACE,
        id as u32,
        atom(&c, "_NET_WM_STATE")?,
        AtomEnum::ATOM,
        &[
            atom(&c, "_NET_WM_STATE_ABOVE")?,
            atom(&c, "_NET_WM_STATE_SKIP_TASKBAR")?,
            atom(&c, "_NET_WM_STATE_SKIP_PAGER")?,
        ],
    )?
    .check()?;
    c.change_property32(
        PropMode::REPLACE,
        id as u32,
        AtomEnum::WM_TRANSIENT_FOR,
        AtomEnum::WINDOW,
        &[owner as u32],
    )?
    .check()?;
    c.flush()?;
    Ok(())
}
pub fn open_path(path: &Path) -> Result<()> {
    anyhow::ensure!(
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()?
            .wait()?
            .success(),
        "无法打开路径，请检查文件关联"
    );
    Ok(())
}
pub fn desktop_available() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
pub struct ProcessMeter {
    previous: HashMap<u32, f64>,
    time: Instant,
}
impl Default for ProcessMeter {
    fn default() -> Self {
        Self {
            previous: HashMap::new(),
            time: Instant::now(),
        }
    }
}
impl ProcessMeter {
    pub fn sample(&mut self) -> (Option<f64>, Option<u64>) {
        let pid = std::process::id();
        let mut pids = vec![pid];
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    pids.extend(
                        children
                            .split_whitespace()
                            .filter_map(|s| s.parse::<u32>().ok()),
                    );
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        let pages = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
        let mut current = HashMap::new();
        let mut delta = 0.;
        let mut memory = 0u64;
        for pid in pids {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(')') else {
                continue;
            };
            let fields: Vec<_> = fields.split_whitespace().collect();
            if fields.len() < 22 {
                continue;
            }
            let cpu = (fields[11].parse::<f64>().unwrap_or(0.)
                + fields[12].parse::<f64>().unwrap_or(0.))
                / ticks;
            delta += (cpu - self.previous.get(&pid).copied().unwrap_or(cpu)).max(0.);
            memory += fields[21].parse::<u64>().unwrap_or(0) * pages;
            current.insert(pid, cpu);
        }
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;
        let percent = delta / self.time.elapsed().as_secs_f64().max(0.001) / cores * 100.;
        self.previous = current;
        self.time = Instant::now();
        (Some(percent), Some(memory))
    }
}

#[derive(Debug)]
pub enum RegionEvent {
    Changed(Rect),
    Start(Rect),
    Closed,
}
pub struct RegionSelection {
    pub events: Receiver<RegionEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for RegionSelection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
pub fn edit_region(monitor: usize, initial: Option<Rect>) -> Result<RegionSelection> {
    let (c, screen) = connect()?;
    let m = *monitors(&c, screen).get(monitor).context("显示器不存在")?;
    let root = m.root;
    let gc = c.generate_id()?;
    c.create_gc(
        gc,
        root,
        &CreateGCAux::new()
            .function(GX::XOR)
            .foreground(0xffffff)
            .line_width(2)
            .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS),
    )?
    .check()?;
    anyhow::ensure!(
        c.grab_pointer(
            false,
            root,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            root,
            x11rb::NONE,
            x11rb::CURRENT_TIME
        )?
        .reply()?
        .status
            == GrabStatus::SUCCESS,
        "鼠标正被其他程序使用"
    );
    if c.grab_keyboard(
        false,
        root,
        x11rb::CURRENT_TIME,
        GrabMode::ASYNC,
        GrabMode::ASYNC,
    )?
    .reply()?
    .status
        != GrabStatus::SUCCESS
    {
        let _ = c.ungrab_pointer(x11rb::CURRENT_TIME);
        c.flush()?;
        anyhow::bail!("键盘正被其他程序使用");
    }
    let enter = keycode(&c, 0xff0d)?;
    let escape = keycode(&c, 0xff1b)?;
    let (tx, events) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || {
        let draw = |r: Rect| {
            let _ = c.poly_rectangle(
                root,
                gc,
                &[Rectangle {
                    x: m.x.saturating_add(r.x as i16),
                    y: m.y.saturating_add(r.y as i16),
                    width: r.width as u16,
                    height: r.height as u16,
                }],
            );
            let _ = c.flush();
        };
        let mut selected = initial;
        if let Some(r) = selected {
            draw(r);
        }
        let mut anchor = None;
        let mut started = false;
        while !worker_stop.load(Ordering::Acquire) {
            match c.poll_for_event() {
                Ok(Some(Event::ButtonPress(e))) if e.detail == 1 => {
                    anchor = Some((e.root_x, e.root_y));
                }
                Ok(Some(Event::MotionNotify(e))) => {
                    if let Some((x, y)) = anchor {
                        if let Some(old) = selected {
                            draw(old);
                        }
                        let clamp_x = |v: i16| {
                            (i32::from(v) - i32::from(m.x)).clamp(0, i32::from(m.width)) as u32
                        };
                        let clamp_y = |v: i16| {
                            (i32::from(v) - i32::from(m.y)).clamp(0, i32::from(m.height)) as u32
                        };
                        let (x1, x2, y1, y2) =
                            (clamp_x(x), clamp_x(e.root_x), clamp_y(y), clamp_y(e.root_y));
                        let r = Rect {
                            x: x1.min(x2),
                            y: y1.min(y2),
                            width: x1.abs_diff(x2),
                            height: y1.abs_diff(y2),
                        };
                        selected = Some(r);
                        draw(r);
                        let _ = tx.try_send(RegionEvent::Changed(r));
                    }
                }
                Ok(Some(Event::ButtonRelease(_))) => anchor = None,
                Ok(Some(Event::KeyPress(e))) if e.detail == enter => {
                    if let Some(r) = selected.filter(|r| r.width >= 16 && r.height >= 16) {
                        let _ = tx.try_send(RegionEvent::Start(r));
                        started = true;
                        break;
                    }
                }
                Ok(Some(Event::KeyPress(e))) if e.detail == escape => break,
                Ok(None) => thread::sleep(Duration::from_millis(15)),
                Ok(Some(_)) => (),
                Err(_) => break,
            }
        }
        if let Some(r) = selected {
            draw(r);
        }
        let _ = c.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = c.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = c.free_gc(gc);
        let _ = c.flush();
        if !started {
            let _ = tx.try_send(RegionEvent::Closed);
        }
    });
    Ok(RegionSelection {
        events,
        stop,
        worker: Some(worker),
    })
}

pub struct CaptureIndicator;
impl CaptureIndicator {
    pub fn new() -> Result<Self> {
        anyhow::bail!("X11 当前不提供排除在录屏之外的边框，请使用录制状态和悬浮控制条")
    }
    pub fn update(&mut self, _: &CaptureTarget, _: bool) -> Result<()> {
        Ok(())
    }
    pub fn hide(&mut self) {}
    pub fn visible(&self) -> bool {
        false
    }
}
pub struct WindowPreview;
impl WindowPreview {
    pub fn new(_: isize, _: isize) -> Result<Self> {
        anyhow::bail!("X11 窗口缩略预览暂不可用，可按标题选择窗口")
    }
    pub fn minimized(&self) -> bool {
        false
    }
    pub fn hide(&self) {}
    pub fn place(&self, _: [i32; 4], _: [i32; 4]) -> Result<()> {
        Ok(())
    }
}

pub struct FixtureCanvas {
    pub width: u32,
    pub height: u32,
    pixels: Mutex<Vec<u8>>,
    font: font_kit::font::Font,
}
impl FixtureCanvas {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        use font_kit::{family_name::FamilyName, properties::Properties, source::SystemSource};
        anyhow::ensure!(
            width > 0 && height > 0 && width <= 8192 && height <= 8192,
            "画布尺寸无效"
        );
        let font = SystemSource::new()
            .select_best_match(
                &[
                    FamilyName::Title("Noto Sans CJK SC".into()),
                    FamilyName::SansSerif,
                ],
                &Properties::default(),
            )?
            .load()?;
        Ok(Self {
            width,
            height,
            pixels: Mutex::new(vec![255; width as usize * height as usize * 4]),
            font,
        })
    }
    pub fn rect(&self, x: i32, y: i32, width: i32, height: i32, color: u32) {
        let mut pixels = self.pixels.lock();
        for y in y.max(0)..(y + height).min(self.height as i32) {
            for x in x.max(0)..(x + width).min(self.width as i32) {
                let offset = (y as usize * self.width as usize + x as usize) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[
                    color as u8,
                    (color >> 8) as u8,
                    (color >> 16) as u8,
                    255,
                ]);
            }
        }
    }
    pub fn text(&self, x: i32, y: i32, size: i32, color: u32, text: &str) {
        use font_kit::{
            canvas::{Canvas, Format, RasterizationOptions},
            hinting::HintingOptions,
        };
        use pathfinder_geometry::{
            transform2d::Transform2F,
            vector::{Vector2F, Vector2I},
        };
        let mut pen = x as f32;
        let size = size.max(1) as f32;
        let mut pixels = self.pixels.lock();
        for ch in text.chars() {
            let Some(glyph) = self.font.glyph_for_char(ch) else {
                continue;
            };
            let Ok(bounds) = self.font.raster_bounds(
                glyph,
                size,
                Transform2F::default(),
                HintingOptions::None,
                RasterizationOptions::GrayscaleAa,
            ) else {
                continue;
            };
            let mut canvas =
                Canvas::new(Vector2I::new(bounds.width(), bounds.height()), Format::A8);
            if self
                .font
                .rasterize_glyph(
                    &mut canvas,
                    glyph,
                    size,
                    Transform2F::from_translation(Vector2F::new(
                        -bounds.origin_x() as f32,
                        -bounds.origin_y() as f32,
                    )),
                    HintingOptions::None,
                    RasterizationOptions::GrayscaleAa,
                )
                .is_ok()
            {
                for row in 0..bounds.height() {
                    for col in 0..bounds.width() {
                        let dx = pen as i32 + bounds.origin_x() + col;
                        let dy = y + size as i32 + bounds.origin_y() + row;
                        if dx < 0 || dy < 0 || dx >= self.width as i32 || dy >= self.height as i32 {
                            continue;
                        }
                        let a =
                            u32::from(canvas.pixels[row as usize * canvas.stride + col as usize]);
                        let offset = (dy as usize * self.width as usize + dx as usize) * 4;
                        for channel in 0..3 {
                            pixels[offset + channel] = ((((color >> (channel * 8)) & 255) * a
                                + u32::from(pixels[offset + channel]) * (255 - a))
                                / 255) as u8;
                        }
                    }
                }
            }
            pen += self
                .font
                .advance(glyph)
                .map(|a| a.x() * size / self.font.metrics().units_per_em as f32)
                .unwrap_or(size);
        }
    }
    pub fn frame(&self, pts_us: i64) -> VideoFrame {
        VideoFrame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            pts_us,
            data: Arc::new(self.pixels.lock().clone()),
        }
    }
}
