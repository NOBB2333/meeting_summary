//! Experimental WGC readback. The WGC event only wakes the owner thread;
//! the D3D immediate context and staging textures never cross thread boundaries.
use super::capture::NativeItem;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select};
use opencap_core::{CaptureBackend, CaptureMetrics, RecordingClock, Rect, VideoFrame};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem},
        DirectX::DirectXPixelFormat,
    },
    Win32::{
        Graphics::{
            Direct3D11::{
                D3D11_BOX, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::DXGI_ERROR_WAS_STILL_DRAWING,
        },
        System::WinRT::{
            Direct3D11::IDirect3DDxgiInterfaceAccess, RO_INIT_MULTITHREADED, RoInitialize,
            RoUninitialize,
        },
    },
    core::{IInspectable, Interface},
};
use windows_capture::{
    d3d11::{StagingTexture, create_d3d_device, create_direct3d_device},
    graphics_capture_api::GraphicsCaptureApi,
    settings::{DrawBorderSettings, GraphicsCaptureItemType},
};

const MAX_STAGING_BYTES: usize = 128 * 1024 * 1024;
const MAX_SLOTS: usize = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) struct AsyncArgs {
    pub item: NativeItem,
    pub crop: Option<Rect>,
    pub cursor: bool,
    pub border: DrawBorderSettings,
    pub interval: Duration,
    pub clock: RecordingClock,
    pub tx: Sender<VideoFrame>,
    pub evict: Receiver<VideoFrame>,
    pub errors: Sender<String>,
    pub dropped: Arc<AtomicU64>,
    pub metrics: Arc<CaptureMetrics>,
}

pub(super) struct AsyncCapture {
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl AsyncCapture {
    pub fn start(args: AsyncArgs) -> Result<Self> {
        let (stop, stopped) = bounded(1);
        let (ready, initialized) = bounded(1);
        let errors = args.errors.clone();
        let worker = thread::Builder::new()
            .name("opencap-wgc-readback".into())
            .spawn(move || {
                if let Err(error) = run(args, stopped, &ready) {
                    let message = format!("{error:#}");
                    if ready.try_send(Err(message.clone())).is_err() {
                        let _ = errors.try_send(format!("异步画面采集结束：{message}"));
                    }
                }
            })?;
        match initialized.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                anyhow::bail!("{error}");
            }
            Err(_) => {
                let _ = worker.join();
                anyhow::bail!("异步采集线程未完成初始化");
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl CaptureBackend for AsyncCapture {
    fn stop(&mut self) -> Result<()> {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("异步画面采集线程异常退出"))?;
        }
        Ok(())
    }
}
impl Drop for AsyncCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

struct Pending {
    // A WGC surface must remain checked out until the GPU copy has finished.
    _frame: Direct3D11CaptureFrame,
    width: u32,
    height: u32,
    pts_us: i64,
    submitted: Instant,
    flushed: bool,
}
struct Slot {
    texture: StagingTexture,
    pending: Option<Pending>,
}

struct Readback {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    slots: Vec<Slot>,
    order: crate::readback_queue::ReadbackQueue,
}

impl Readback {
    fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Self {
        Self {
            device,
            context,
            slots: Vec::new(),
            order: Default::default(),
        }
    }

    fn submit(
        &mut self,
        frame: Direct3D11CaptureFrame,
        texture: &ID3D11Texture2D,
        desc: D3D11_TEXTURE2D_DESC,
        crop: Option<Rect>,
        pts_us: i64,
        metrics: &CaptureMetrics,
    ) -> Result<()> {
        let (width, height) = crop.map_or((desc.Width, desc.Height), |r| (r.width, r.height));
        if let Some(r) = crop {
            anyhow::ensure!(
                r.x.checked_add(r.width).is_some_and(|x| x <= desc.Width)
                    && r.y.checked_add(r.height).is_some_and(|y| y <= desc.Height),
                "录制区域已超出显示器范围"
            );
        }
        // Reserve for both slots, including conservative per-row alignment.
        let row = (width as usize)
            .checked_mul(4)
            .and_then(|n| n.checked_add(255))
            .map(|n| n & !255)
            .context("画面尺寸过大")?;
        let bytes = row
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(MAX_SLOTS))
            .context("画面尺寸过大")?;
        anyhow::ensure!(bytes <= MAX_STAGING_BYTES, "画面过大，异步采集超过内存上限");
        let Some(index) = self.order.next_free() else {
            metrics.staging_full.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        if index == self.slots.len() {
            self.slots.push(Slot {
                texture: StagingTexture::new(&self.device, width, height, desc.Format)?,
                pending: None,
            });
        }
        let slot = &mut self.slots[index];
        let current = slot.texture.desc();
        if (current.Width, current.Height, current.Format) != (width, height, desc.Format) {
            slot.texture = StagingTexture::new(&self.device, width, height, desc.Format)?;
        }
        let submitted = Instant::now();
        unsafe {
            if let Some(r) = crop {
                let region = D3D11_BOX {
                    left: r.x,
                    top: r.y,
                    front: 0,
                    right: r.x + r.width,
                    bottom: r.y + r.height,
                    back: 1,
                };
                self.context.CopySubresourceRegion(
                    slot.texture.texture(),
                    0,
                    0,
                    0,
                    0,
                    texture,
                    0,
                    Some(&region),
                );
            } else {
                self.context.CopyResource(slot.texture.texture(), texture);
            }
        }
        metrics
            .copy_submit_max_us
            .fetch_max(submitted.elapsed().as_micros() as u64, Ordering::Relaxed);
        slot.pending = Some(Pending {
            _frame: frame,
            width,
            height,
            pts_us,
            submitted,
            flushed: false,
        });
        self.order.submitted(index);
        Ok(())
    }

    fn poll(&mut self, args: &AsyncArgs) -> Result<()> {
        while let Some(index) = self.order.front() {
            let slot = &mut self.slots[index];
            let pending = slot.pending.as_mut().expect("pending slot");
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let began = Instant::now();
            let result = unsafe {
                self.context.Map(
                    slot.texture.texture(),
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            };
            args.metrics
                .map_call_max_us
                .fetch_max(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            if let Err(error) = result {
                if error.code() == DXGI_ERROR_WAS_STILL_DRAWING {
                    args.metrics.map_not_ready.fetch_add(1, Ordering::Relaxed);
                    if !pending.flushed && pending.submitted.elapsed() >= Duration::from_millis(100)
                    {
                        // Usually Map submits queued GPU work. If a driver has
                        // not done so after 100 ms, submit once without waiting.
                        unsafe { self.context.Flush() };
                        pending.flushed = true;
                    }
                    anyhow::ensure!(
                        pending.submitted.elapsed() < Duration::from_secs(2),
                        "显卡读取画面超过 2 秒，请使用兼容模式"
                    );
                    break;
                }
                return Err(error.into());
            }
            // Every successful map is unmapped, including invalid geometry errors.
            struct Mapping<'a>(&'a ID3D11DeviceContext, &'a ID3D11Texture2D);
            impl Drop for Mapping<'_> {
                fn drop(&mut self) {
                    unsafe { self.0.Unmap(self.1, 0) };
                }
            }
            let mapping = Mapping(&self.context, slot.texture.texture());
            let stride = mapped.RowPitch as usize;
            let len = stride
                .checked_mul(pending.height as usize)
                .context("采集图像过大")?;
            anyhow::ensure!(
                !mapped.pData.is_null()
                    && stride >= pending.width as usize * 4
                    && len <= isize::MAX as usize,
                "采集图像映射无效"
            );
            let pixels = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            let frame = VideoFrame {
                width: pending.width,
                height: pending.height,
                stride,
                pts_us: pending.pts_us,
                data: VideoFrame::copy_pixels(pixels),
            };
            args.metrics
                .record_readback(len, pending.submitted.elapsed());
            drop(mapping);
            slot.pending = None;
            self.order.completed();
            if let Err(TrySendError::Full(frame)) = args.tx.try_send(frame) {
                if args.evict.try_recv().is_ok() {
                    args.dropped.fetch_add(1, Ordering::Relaxed);
                }
                let _ = args.tx.try_send(frame);
            }
        }
        Ok(())
    }
}

fn run(
    args: AsyncArgs,
    stopped: Receiver<()>,
    ready: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let _apartment = Apartment::new()?;
    anyhow::ensure!(
        GraphicsCaptureApi::is_supported()?,
        "Windows 图形采集不可用"
    );
    let item: GraphicsCaptureItemType = args.item.try_into().map_err(anyhow::Error::msg)?;
    let item = match item {
        GraphicsCaptureItemType::Monitor((item, _))
        | GraphicsCaptureItemType::Window((item, _)) => item,
        GraphicsCaptureItemType::Unknown((item, _)) => item,
    };
    let (device, context) = create_d3d_device()?;
    super::capture::record_adapter(&args.metrics, &device);
    let direct3d = create_direct3d_device(&device)?;
    let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
    let mut size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&direct3d, format, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    anyhow::ensure!(
        GraphicsCaptureApi::is_cursor_settings_supported()?,
        "当前系统不支持鼠标捕获设置"
    );
    session.SetIsCursorCaptureEnabled(args.cursor)?;
    match args.border {
        DrawBorderSettings::Default => {}
        DrawBorderSettings::WithBorder => session.SetIsBorderRequired(true)?,
        DrawBorderSettings::WithoutBorder => session.SetIsBorderRequired(false)?,
    }
    if GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
        session.SetMinUpdateInterval(args.interval.into())?;
    }
    let (wakeup_tx, wakeup) = bounded::<()>(1);
    let callback_metrics = args.metrics.clone();
    let frame_token = pool.FrameArrived(&TypedEventHandler::<
        Direct3D11CaptureFramePool,
        IInspectable,
    >::new(move |_, _| {
        callback_metrics.callbacks.fetch_add(1, Ordering::Relaxed);
        let _ = wakeup_tx.try_send(());
        Ok(())
    }))?;
    let (closed_tx, closed) = bounded::<()>(1);
    let closed_token = item.Closed(
        &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            let _ = closed_tx.try_send(());
            Ok(())
        }),
    )?;
    session.StartCapture()?;
    let _ = ready.try_send(Ok(()));
    let mut readback = Readback::new(device, context);
    let mut last = None::<Instant>;
    let result: Result<()> = (|| {
        loop {
            let timeout = if readback.order.is_empty() {
                Duration::from_secs(1)
            } else {
                POLL_INTERVAL
            };
            let mut frame_ready = false;
            select! {
                recv(stopped) -> _ => break,
                recv(closed) -> _ => {
                    let _ = args.errors.try_send("录制目标已关闭或设备断开".into());
                    break;
                },
                recv(wakeup) -> _ => frame_ready = true,
                default(timeout) => {},
            }
            readback.poll(&args)?;
            if !frame_ready {
                continue;
            }
            let Some(frame) = newest_frame(&pool)? else {
                continue;
            };
            let content_size = frame.ContentSize()?;
            if content_size.Width != size.Width || content_size.Height != size.Height {
                drop(frame);
                if content_size.Width >= 2 && content_size.Height >= 2 {
                    // Recreate invalidates pool surfaces. Release all checked-out
                    // frames first; never reuse a surface still being copied.
                    let deadline = Instant::now() + DRAIN_TIMEOUT;
                    while !readback.order.is_empty() && Instant::now() < deadline {
                        readback.poll(&args)?;
                        if !readback.order.is_empty() {
                            thread::sleep(POLL_INTERVAL);
                        }
                    }
                    anyhow::ensure!(
                        readback.order.is_empty(),
                        "画面尺寸变化时显卡仍在忙，请使用兼容模式"
                    );
                    size = content_size;
                    pool.Recreate(&direct3d, format, 2, size)?;
                }
                continue;
            }
            if args.clock.is_paused() {
                continue;
            }
            if last.is_some_and(|t| t.elapsed() < args.interval) {
                args.metrics.rate_skips.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            last = Some(Instant::now());
            let latency = super::qpc_us()
                .saturating_sub(frame.SystemRelativeTime()?.Duration / 10)
                .max(0);
            let pts_us = (args.clock.now_us() - latency).max(0);
            let access = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            if desc.Width >= 2 && desc.Height >= 2 {
                readback.submit(frame, &texture, desc, args.crop, pts_us, &args.metrics)?;
            }
        }
        Ok(())
    })();
    // Drain already submitted frames without unbounded waits, including a
    // final frame when the desktop stopped generating updates.
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while !readback.order.is_empty() && Instant::now() < deadline {
        if readback.poll(&args).is_err() {
            break;
        }
        if !readback.order.is_empty() {
            thread::sleep(POLL_INTERVAL);
        }
    }
    let _ = session.Close();
    let _ = pool.RemoveFrameArrived(frame_token);
    let _ = item.RemoveClosed(closed_token);
    let _ = pool.Close();
    result
}

fn newest_frame(pool: &Direct3D11CaptureFramePool) -> Result<Option<Direct3D11CaptureFrame>> {
    // The event channel is deliberately one-slot. Drain the two-frame WGC
    // pool so a coalesced final notification cannot strand the newest image.
    let mut latest = None;
    for _ in 0..2 {
        match pool.TryGetNextFrame() {
            Ok(frame) => latest = Some(frame),
            // WinRT returns a successful null when its pool is empty; the Rust
            // projection represents this as Error::empty (HRESULT 0).
            Err(error) if error.code().0 == 0 => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(latest)
}
