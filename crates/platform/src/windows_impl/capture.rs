use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender};
use opencap_core::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::Graphics::{
        Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6},
        Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
    },
    core::Interface,
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::*,
    window::Window,
};

type CaptureError = Box<dyn std::error::Error + Send + Sync>;
enum NativeItem {
    Display(Monitor),
    Window(Window),
}
impl TryFrom<NativeItem> for GraphicsCaptureItemType {
    type Error = String;
    fn try_from(item: NativeItem) -> std::result::Result<Self, Self::Error> {
        match item {
            NativeItem::Display(m) => m
                .try_into()
                .map_err(|e: windows::core::Error| e.to_string()),
            NativeItem::Window(w) => w
                .try_into()
                .map_err(|e: windows::core::Error| e.to_string()),
        }
    }
}
pub fn targets() -> Result<Vec<TargetInfo>> {
    let mut targets = Vec::new();
    for (index, m) in Monitor::enumerate()?.iter().enumerate() {
        targets.push(TargetInfo {
            target: CaptureTarget::Monitor(index),
            name: format!("显示器 {} · {}", index + 1, m.name().unwrap_or_default()),
            width: m.width()?,
            height: m.height()?,
        });
    }
    for w in Window::enumerate()? {
        if w.process_id().ok() == Some(std::process::id()) {
            continue;
        }
        let title = w.title().unwrap_or_default();
        let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
        if title.is_empty() || width < 2 || height < 2 {
            continue;
        }
        targets.push(TargetInfo {
            target: CaptureTarget::Window(w.as_raw_hwnd() as isize),
            name: title,
            width: width as u32,
            height: height as u32,
        });
    }
    Ok(targets)
}
pub fn monitor_bounds(index: usize) -> Result<(i32, i32, u32, u32)> {
    let monitors = Monitor::enumerate()?;
    let monitor = monitors.get(index).context("Display no longer exists")?;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut info).ok()?;
    }
    let r = info.rcMonitor;
    Ok((
        r.left,
        r.top,
        (r.right - r.left) as u32,
        (r.bottom - r.top) as u32,
    ))
}
/// Physical screen region for a visible window, including tool windows that WGC
/// cannot open as a standalone item. The rectangle is clipped to its monitor.
pub fn window_region(hwnd: isize) -> Result<CaptureTarget> {
    let window = Window::from_raw_hwnd(hwnd as _);
    let monitor = window.monitor().context("Window has no monitor")?;
    let index = Monitor::enumerate()?
        .iter()
        .position(|m| m.as_raw_hmonitor() == monitor.as_raw_hmonitor())
        .context("Monitor disappeared")?;
    let (x, y, w, h) = monitor_bounds(index)?;
    let r = window.rect()?;
    let left = r.left.max(x);
    let top = r.top.max(y);
    let right = r.right.min(x + w as i32);
    let bottom = r.bottom.min(y + h as i32);
    anyhow::ensure!(right > left && bottom > top, "Window is outside monitor");
    Ok(CaptureTarget::Region {
        monitor: index,
        rect: Rect {
            x: (left - x) as u32,
            y: (top - y) as u32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        },
    })
}
pub fn capture_vendor(target: &CaptureTarget) -> Option<u32> {
    let monitor = match *target {
        CaptureTarget::Monitor(i) | CaptureTarget::Region { monitor: i, .. } => {
            *Monitor::enumerate().ok()?.get(i)?
        }
        CaptureTarget::Window(hwnd) => Window::from_raw_hwnd(hwnd as _).monitor()?,
    };
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                if output.GetDesc().ok()?.Monitor.0 == monitor.as_raw_hmonitor() {
                    return Some(adapter.GetDesc1().ok()?.VendorId);
                }
                j += 1;
            }
            i += 1;
        }
    }
    None
}
fn ensure_sdr(monitor: Monitor) -> Result<()> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                if output.GetDesc()?.Monitor.0 == monitor.as_raw_hmonitor() {
                    let output: IDXGIOutput6 = output
                        .cast()
                        .context("Cannot determine display HDR state")?;
                    anyhow::ensure!(
                        output.GetDesc1()?.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                        "当前显示器已启用 HDR，请切换为 SDR 后录制。"
                    );
                    return Ok(());
                }
                j += 1;
            }
            i += 1;
        }
    }
    anyhow::bail!("Cannot determine capture display color space")
}
struct Flags {
    clock: RecordingClock,
    tx: Sender<VideoFrame>,
    evict: Receiver<VideoFrame>,
    errors: Sender<String>,
    dropped: Arc<AtomicU64>,
    crop: Option<Rect>,
    interval: Duration,
}
struct Handler {
    flags: Flags,
    last: Option<Instant>,
}
impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = CaptureError;
    fn new(ctx: Context<Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            last: None,
        })
    }
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        if self.flags.clock.is_paused()
            || self.last.is_some_and(|t| t.elapsed() < self.flags.interval)
        {
            return Ok(());
        }
        self.last = Some(Instant::now());
        let latency = super::qpc_us()
            .saturating_sub(frame.timestamp()?.Duration / 10)
            .max(0);
        let pts_us = (self.flags.clock.now_us() - latency).max(0);
        let (width, height) = (frame.width(), frame.height());
        if width < 2 || height < 2 {
            return Ok(());
        }
        let (width, height, mut buffer) = if let Some(r) = self.flags.crop {
            if r.x.checked_add(r.width).is_none_or(|x| x > width)
                || r.y.checked_add(r.height).is_none_or(|y| y > height)
            {
                let _ = self
                    .flags
                    .errors
                    .try_send("录制区域已超出显示器范围，录制已停止".into());
                return Ok(());
            }
            (
                r.width,
                r.height,
                frame.buffer_crop(r.x, r.y, r.x + r.width, r.y + r.height)?,
            )
        } else {
            (width, height, frame.buffer()?)
        };
        // Preserve the native row stride. De-padding uses the dependency's global
        // Rayon pool and then requires another full-frame copy for ownership.
        let stride = buffer.row_pitch() as usize;
        let data = Arc::new(buffer.as_raw_buffer().to_vec());
        let video = VideoFrame {
            width,
            height,
            stride,
            pts_us,
            data,
        };
        if let Err(crossbeam_channel::TrySendError::Full(video)) = self.flags.tx.try_send(video) {
            if self.flags.evict.try_recv().is_ok() {
                self.flags.dropped.fetch_add(1, Ordering::Relaxed);
            }
            let _ = self.flags.tx.try_send(video);
        }
        Ok(())
    }
    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        let _ = self
            .flags
            .errors
            .try_send("录制目标已关闭或设备断开".into());
        Ok(())
    }
}
pub struct WindowsCapture {
    control: Option<CaptureControl<Handler, CaptureError>>,
    warnings: Vec<String>,
}
impl WindowsCapture {
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn is_finished(&self) -> bool {
        self.control.as_ref().is_none_or(|c| c.is_finished())
    }
}
impl CaptureBackend for WindowsCapture {
    fn stop(&mut self) -> Result<()> {
        if let Some(c) = self.control.take() {
            c.stop().map_err(|e| anyhow::anyhow!(e.to_string()))?;
        }
        Ok(())
    }
}
impl Drop for WindowsCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn start_capture(
    config: &RecordingConfig,
    clock: RecordingClock,
    tx: Sender<VideoFrame>,
    evict: Receiver<VideoFrame>,
    errors: Sender<String>,
    dropped: Arc<AtomicU64>,
) -> Result<WindowsCapture> {
    // Windows owns its yellow capture border. Ask the documented borderless API,
    // then use our platform-neutral border policy for the application's overlay.
    let mut warnings = Vec::new();
    let border = borderless_settings(&mut warnings);
    let (item, crop) = match config.target {
        CaptureTarget::Monitor(i) | CaptureTarget::Region { monitor: i, .. } => {
            let monitors = Monitor::enumerate()?;
            let monitor = *monitors.get(i).context("Display no longer exists")?;
            ensure_sdr(monitor)?;
            let crop = if let CaptureTarget::Region { rect, .. } = config.target {
                anyhow::ensure!(
                    rect.x
                        .checked_add(rect.width)
                        .is_some_and(|x| x <= monitor.width().unwrap_or(0))
                        && rect
                            .y
                            .checked_add(rect.height)
                            .is_some_and(|y| y <= monitor.height().unwrap_or(0)),
                    "区域超出显示器范围"
                );
                Some(rect)
            } else {
                None
            };
            (NativeItem::Display(monitor), crop)
        }
        CaptureTarget::Window(hwnd) => {
            let w = Window::from_raw_hwnd(hwnd as _);
            anyhow::ensure!(
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(
                        windows::Win32::Foundation::HWND(hwnd as _),
                    ))
                    .as_bool()
                },
                "Window no longer exists"
            );
            ensure_sdr(w.monitor().context("Window has no display")?)?;
            (NativeItem::Window(w), None)
        }
    };
    let flags = Flags {
        clock,
        tx,
        evict,
        errors,
        dropped,
        crop,
        interval: Duration::from_micros(1_000_000 / config.max_fps as u64),
    };
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;
    let update_interval =
        if GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
            MinimumUpdateIntervalSettings::Custom(flags.interval)
        } else {
            MinimumUpdateIntervalSettings::Default
        };
    let settings = Settings::new(
        item,
        if config.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        },
        border,
        SecondaryWindowSettings::Default,
        update_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    let control =
        Handler::start_free_threaded(settings).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(WindowsCapture {
        control: Some(control),
        warnings,
    })
}

fn borderless_settings(warnings: &mut Vec<String>) -> DrawBorderSettings {
    use windows::{
        Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind},
        Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    };
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;
    // This runs on the recording worker before WGC creates its own WinRT thread.
    struct Apartment(bool);
    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    windows::Win32::System::WinRT::RoUninitialize();
                }
            }
        }
    }
    let _apartment = Apartment(unsafe {
        windows::Win32::System::WinRT::RoInitialize(
            windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
        )
        .is_ok()
    });
    if !GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
        warnings.push("此 Windows 版本不支持隐藏系统捕获边框，黄色系统边框可能保留。".into());
        return DrawBorderSettings::Default;
    }
    // Cache per process so new recordings do not repeatedly prompt the user.
    static ACCESS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let result = ACCESS.get_or_init(|| {
        GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|request| request.join())
            .map_err(|e| e.to_string())
            .and_then(|status| {
                if status == AppCapabilityAccessStatus::Allowed {
                    Ok(())
                } else {
                    Err(format!("权限状态 {status:?}"))
                }
            })
    });
    if let Err(reason) = result {
        warnings.push(format!(
            "Windows 未授予隐藏系统捕获边框的权限，黄色边框可能保留：{reason}"
        ));
    }
    DrawBorderSettings::WithoutBorder
}
