use anyhow::Result;
use opencap_core::*;
#[cfg(windows)]
use std::sync::Arc;
use std::time::Instant;
#[cfg(windows)]
use windows::{
    Win32::{Foundation::*, Graphics::Gdi::*},
    core::PCWSTR,
};

/// Original procedural content. No Office files, fonts or third-party videos are bundled.
#[cfg(windows)]
struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}
#[cfg(windows)]
impl Canvas {
    fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            anyhow::ensure!(!dc.is_invalid(), "CreateCompatibleDC failed");
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(dc, HGDIOBJ(bitmap.0));
            Ok(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }
    fn rect(&self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        unsafe {
            let brush = CreateSolidBrush(COLORREF(color));
            FillRect(
                self.dc,
                &RECT {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                },
                brush,
            );
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
    }
    fn text(&self, x: i32, y: i32, size: i32, color: u32, text: &str) {
        unsafe {
            let face: Vec<u16> = "Microsoft YaHei UI".encode_utf16().chain(Some(0)).collect();
            let font = CreateFontW(
                -size,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                PCWSTR(face.as_ptr()),
            );
            let old = SelectObject(self.dc, HGDIOBJ(font.0));
            SetTextColor(self.dc, COLORREF(color));
            SetBkMode(self.dc, TRANSPARENT);
            let text: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(self.dc, x, y, &text);
            SelectObject(self.dc, old);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }
    fn frame(&self, pts_us: i64) -> VideoFrame {
        unsafe {
            let _ = GdiFlush();
            VideoFrame {
                width: self.width,
                height: self.height,
                stride: self.width as usize * 4,
                pts_us,
                data: Arc::new(
                    std::slice::from_raw_parts(
                        self.bits,
                        self.width as usize * self.height as usize * 4,
                    )
                    .to_vec(),
                ),
            }
        }
    }
}
#[cfg(windows)]
impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}
#[cfg(target_os = "macos")]
use opencap_platform::FixtureCanvas as Canvas;
pub fn run(
    mut config: RecordingConfig,
    scene: &str,
    seconds: u64,
    width: u32,
    height: u32,
    fast: bool,
) -> Result<()> {
    use opencap_media::MediaWriter;
    config.system_audio = true;
    config.microphone = false; // Include a known audio marker track in size/timing comparisons.
    std::fs::create_dir_all(&config.output_dir)?;
    let suffix = if config.adaptive { "adaptive" } else { "fixed" };
    let name = config
        .encoder
        .clone()
        .unwrap_or_else(|| format!("{:?}", config.codec));
    let output = config
        .output_dir
        .join(format!("{scene}-{name}-{suffix}-{seconds}s.mp4"));
    let partial = output.with_extension("partial.mp4");
    anyhow::ensure!(
        !output.exists() && !partial.exists(),
        "Benchmark output exists; choose another directory"
    );
    let canvas = Canvas::new(width, height)?;
    let mut writer = MediaWriter::new(&partial, width, height, &config)?;
    let encoder = writer.video.name.clone();
    let options = writer.video.options.clone();
    let details = writer.video.details();
    let start = Instant::now();
    let mut meter = opencap_platform::ProcessMeter::default();
    let mut peak_cpu: f64 = 0.0;
    let mut peak_memory = 0;
    let mut policy = opencap_policy::FramePolicy::default();
    let mut cached: Option<(u64, VideoFrame)> = None;
    for index in 0..seconds * 30 {
        if !fast {
            let due = std::time::Duration::from_micros(index * 1_000_000 / 30);
            if let Some(delay) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(delay);
            }
        }
        let pts_us = (index * 1_000_000 / 30) as i64;
        let t = index as f64 / 30.0;
        let content_key = match scene {
            "ppt" => index / 900,
            "document" => index / 300,
            "ide" => index / 90,
            _ => index,
        };
        let key = content_key * 2 + u64::from(index % 30 < 3);
        let frame = if let Some((previous, frame)) = &cached
            && *previous == key
        {
            let mut frame = frame.clone();
            frame.pts_us = pts_us;
            frame
        } else {
            draw(&canvas, scene, t);
            let frame = canvas.frame(pts_us);
            cached = Some((key, frame.clone()));
            frame
        };
        if policy.accept(&frame, &config) {
            writer.video(&frame)?;
        }
        // 80 ms tone at each whole second, useful for A/V checks. The rest is silence.
        let samples = (0..1600)
            .flat_map(|i| {
                let sample_index = index * 1600 + i;
                let phase = sample_index % 48000;
                let v = if phase < 3840 {
                    (std::f64::consts::TAU * 1000.0 * sample_index as f64 / 48000.0).sin() as f32
                        * 0.1
                } else {
                    0.0
                };
                [v, v]
            })
            .collect();
        writer.audio(AudioChunk {
            kind: AudioKind::System,
            pts_us,
            samples,
        });
        writer.audio_until(pts_us + 33_333, false)?;
        if index % 300 == 0 {
            let (cpu, memory) = meter.sample();
            peak_cpu = peak_cpu.max(cpu.unwrap_or(0.0));
            peak_memory = peak_memory.max(memory.unwrap_or(0));
            eprintln!("{scene}: {:.0}/{seconds}s", t);
        }
    }
    let frames = writer.frames;
    if !fast
        && let Some(delay) = std::time::Duration::from_secs(seconds).checked_sub(start.elapsed())
    {
        std::thread::sleep(delay);
    }
    writer.finish(seconds as i64 * 1_000_000)?;
    opencap_media::recover_file(&partial, &output)?;
    std::fs::remove_file(partial)?;
    let bytes = std::fs::metadata(&output)?.len();
    let report = serde_json::json!({"fixture_version":2,"scene":scene,"synthetic":true,"fast":fast,"duration_seconds":seconds,"width":width,"height":height,"bytes":bytes,"mb_per_hour_extrapolated":bytes as f64/1e6*3600.0/seconds as f64,"encoded_frames":frames,"actual_fps":frames as f64/seconds as f64,"elapsed_seconds":start.elapsed().as_secs_f64(),"encoder":encoder,"encoder_options":options,"encoder_details":details,"output_media":opencap_media::inspect_output(&output)?,"config":config,"cpu_peak_percent":peak_cpu,"peak_memory_bytes":peak_memory,"gpu_percent":null,"text_readability":"manual review required; no OCR used","quality_target_passed":null});
    std::fs::write(
        output.with_extension("benchmark.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
fn draw(c: &Canvas, scene: &str, t: f64) {
    c.rect(
        0,
        0,
        c.width as i32,
        c.height as i32,
        if scene == "ide" { 0x24201c } else { 0xf8f7f5 },
    );
    c.rect(0, 0, c.width as i32, 64, 0x40362e);
    c.text(
        32,
        18,
        24,
        0xffffff,
        &format!("OpenCap · {scene} · original benchmark fixture"),
    );
    match scene {
        "ppt" => {
            c.text(
                120,
                180,
                48,
                0x302820,
                &format!("项目实施方案 · 第 {} 页", t as u64 / 30 + 1),
            );
            for (i, line) in [
                "目标：录制桌面内容并保持文字清晰",
                "采集 → 编码 → 封装 → 本地保存",
                "静态桌面、滚动、音频同步",
                "Open source · Local first · No watermark",
            ]
            .iter()
            .enumerate()
            {
                c.text(130, 320 + i as i32 * 85, 30, 0x403830, line);
            }
        }
        "document" => {
            c.rect(240, 100, 1080, 900, 0xffffff);
            for row in 0..25 {
                c.text(280,140+row*31,18,0x302820,&format!("{:02}  会议记录 / 工作事项  The quick brown fox jumps over the lazy dog.  0123456789",row+1));
            }
            let cell = t as i32 / 10 % 8;
            c.rect(1420, 150 + cell * 60, 280, 52, 0xd7f0dc);
            for row in 0..10 {
                c.text(
                    1420,
                    150 + row * 60,
                    20,
                    0x403020,
                    &format!("Q{}   128.50   完成", row + 1),
                );
            }
        }
        "ide" => {
            for row in 0..28 {
                c.text(30, 105 + row * 29, 16, 0x94877b, &format!("{:03}", row + 1));
                c.text(
                    100,
                    105 + row * 29,
                    16,
                    0xdcd4bd,
                    match row % 5 {
                        0 => "pub fn record_frame(frame: &VideoFrame) -> Result<()> {",
                        1 => "    let timestamp = clock.now_us(); // 统一时间戳",
                        2 => "    encoder.send_frame(frame)?;",
                        3 => "    muxer.write_packets(timestamp)?;",
                        _ => "}",
                    },
                );
            }
            c.text(
                120,
                970,
                18,
                0x96dcc2,
                &format!(
                    "// edit {}: text remains readable at native resolution",
                    t as u64 / 3
                ),
            );
        }
        "scroll" => {
            let offset = ((t % 12.0) * 150.0) as i32;
            for row in 0..50 {
                let y = 100 + row * 55 - offset;
                if y > 70 && y < c.height as i32 {
                    c.text(
                        100,
                        y,
                        22,
                        0x302820,
                        &format!(
                            "文章 {:02} · 浏览器滚动测试 · Readable text and smooth scrolling",
                            row + 1
                        ),
                    );
                }
            }
        }
        _ => {
            for i in 0..45 {
                let x = ((t * 160.0 + i as f64 * 97.0).sin() * 0.45 + 0.5) * c.width as f64;
                let y = ((t * 1.7 + i as f64).cos() * 0.4 + 0.55) * c.height as f64;
                c.rect(x as i32, y as i32, 90, 70, ((i * 31337) as u32) & 0xffffff);
            }
            c.text(
                60,
                100,
                28,
                0x202020,
                "Synthetic motion stress test — not a real video reference",
            );
        }
    }
    let sample_y = c.height as i32 - 120;
    c.rect(
        0,
        sample_y - 12,
        c.width as i32,
        132,
        if scene == "ide" { 0x24201c } else { 0xf8f7f5 },
    );
    for (i, size) in [12, 14, 16].into_iter().enumerate() {
        c.text(
            100,
            sample_y + i as i32 * 30,
            size,
            if scene == "ide" { 0xffffff } else { 0x101010 },
            "文字清晰度样本 AaBb 0123456789 Il1 O0 {}[] 中文：录屏测试",
        );
    }
    // A visual marker aligns with the audio tone every second.
    if t.fract() < 0.08 {
        c.rect(c.width as i32 - 60, 80, 30, 30, 0x2020ef);
    }
}
