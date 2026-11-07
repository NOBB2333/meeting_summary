use anyhow::{Context, Result};
use ffmpeg_next as ff;
use opencap_core::*;
use std::{collections::BTreeMap, sync::Arc};

pub const VIDEO_TIME_BASE: ff::Rational = ff::Rational(1, 90_000);
pub struct FfmpegVideoEncoder {
    pub inner: ff::encoder::video::Encoder,
    pub name: String,
    pub options: BTreeMap<String, String>,
    scaler: Option<ff::software::scaling::Context>,
    source_size: (u32, u32),
    scaled_frame: Option<ff::frame::Video>,
    canvas_frame: Option<ff::frame::Video>,
    canvas_layout: Option<(u32, u32)>,
    next_keyframe: i64,
    gop_us: i64,
}
impl FfmpegVideoEncoder {
    pub fn new(name: &str, width: u32, height: u32, config: &RecordingConfig) -> Result<Self> {
        let codec = ff::encoder::find_by_name(name)
            .with_context(|| format!("Encoder {name} is not in this FFmpeg build"))?;
        let mut context = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        context.set_width(width & !1);
        context.set_height(height & !1);
        let pixel = if name.ends_with("_qsv") || name.ends_with("_amf") {
            ff::format::Pixel::NV12
        } else {
            ff::format::Pixel::YUV420P
        };
        context.set_format(pixel);
        context.set_time_base(VIDEO_TIME_BASE);
        context.set_frame_rate(Some((config.max_fps as i32, 1)));
        context.set_max_b_frames(0);
        context.set_gop(config.gop_seconds * config.max_fps);
        context.set_flags(ff::codec::Flags::GLOBAL_HEADER);
        let available_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        // Use fewer encoder workers in fast mode to reduce competition with
        // desktop and audio work; this does not reserve a processor.
        let encoder_threads = if config.fast_software_encoding && name.starts_with("libx26") {
            available_threads.saturating_sub(1).clamp(1, 4)
        } else {
            available_threads.min(8)
        };
        context.set_threading(ff::codec::threading::Config {
            count: encoder_threads,
            ..Default::default()
        });
        let mut options = BTreeMap::new();
        if name.starts_with("libx26") {
            options.insert(
                "preset".into(),
                if config.fast_software_encoding {
                    "ultrafast"
                } else {
                    "veryfast"
                }
                .into(),
            );
            options.insert("crf".into(), config.quality.to_string());
            if name == "libx264" {
                options.insert("tune".into(), "stillimage,zerolatency".into());
            }
            if name == "libx265" {
                options.insert(
                    "x265-params".into(),
                    if config.fast_software_encoding {
                        "log-level=error:pools=1:frame-threads=1:rc-lookahead=0:bframes=0"
                    } else {
                        "log-level=error:pools=4:frame-threads=2:rc-lookahead=0:bframes=0"
                    }
                    .into(),
                );
            }
        } else if name == "libsvtav1" {
            options.insert("preset".into(), "8".into());
            options.insert("crf".into(), (config.quality + 5).to_string());
            options.insert("svtav1-params".into(), "lp=4:pred-struct=1".into());
        } else if name.ends_with("_videotoolbox") {
            options.insert("realtime".into(), "1".into());
            options.insert("allow_sw".into(), "0".into());
            context.set_bit_rate(config.max_bitrate_kbps as usize * 750);
        } else if name.ends_with("_qsv") {
            options.insert("preset".into(), "medium".into());
            options.insert("async_depth".into(), "1".into());
            // ICQ exposes quality without imposing a constant bitrate on static desktops.
            context.set_quality(config.quality as usize);
        } else if name.ends_with("_nvenc") {
            options.insert("preset".into(), "p4".into());
            options.insert("rc".into(), "vbr".into());
            options.insert("cq".into(), config.quality.to_string());
        } else if name.ends_with("_amf") {
            options.insert("quality".into(), "balanced".into());
            options.insert("rc".into(), "vbr_peak".into());
            context.set_bit_rate(config.max_bitrate_kbps as usize * 500);
        }
        // QSV ICQ and low-delay SVT CQP do not promise a VBV cap.
        if !name.ends_with("_qsv") && name != "libsvtav1" {
            context.set_max_bit_rate(config.max_bitrate_kbps as usize * 1000);
            unsafe {
                (*context.as_mut_ptr()).rc_buffer_size = (config.max_bitrate_kbps * 2000) as i32;
            }
        }
        unsafe {
            (*context.as_mut_ptr()).color_primaries = ff::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*context.as_mut_ptr()).color_trc =
                ff::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            (*context.as_mut_ptr()).colorspace = ff::ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*context.as_mut_ptr()).color_range = ff::ffi::AVColorRange::AVCOL_RANGE_MPEG;
        }
        options.extend(config.encoder_options.clone());
        // FFmpeg's open_with discards unknown dictionary entries; reject misspellings explicitly.
        for key in config.encoder_options.keys() {
            let key = std::ffi::CString::new(key.as_str())?;
            let found = unsafe {
                ff::ffi::av_opt_find(
                    context.as_mut_ptr().cast(),
                    key.as_ptr(),
                    std::ptr::null(),
                    0,
                    ff::ffi::AV_OPT_SEARCH_CHILDREN,
                )
            };
            anyhow::ensure!(
                !found.is_null(),
                "Unknown encoder option: {}",
                key.to_string_lossy()
            );
        }
        let mut dictionary = ff::Dictionary::new();
        for (k, v) in &options {
            dictionary.set(k, v);
        }
        let inner = context
            .open_with(dictionary)
            .with_context(|| format!("Opening encoder {name}"))?;
        Ok(Self {
            inner,
            name: name.into(),
            options,
            scaler: None,
            source_size: (0, 0),
            scaled_frame: None,
            canvas_frame: None,
            canvas_layout: None,
            next_keyframe: 0,
            gop_us: config.gop_seconds as i64 * 1_000_000,
        })
    }
    pub fn parameters(&self) -> ff::codec::Parameters {
        ff::codec::Parameters::from(&self.inner)
    }
    pub fn details(&self) -> serde_json::Value {
        let profile = unsafe {
            let name = ff::ffi::avcodec_profile_name(
                self.inner.id().into(),
                (*self.inner.as_ptr()).profile,
            );
            if name.is_null() {
                None
            } else {
                Some(
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned(),
                )
            }
        };
        serde_json::json!({"name":self.name,"codec":self.inner.id().name(),"profile":profile,"options":self.options,
            "pixel_format":format!("{:?}",self.inner.format()),"width":self.inner.width(),"height":self.inner.height(),
            "rate_control":if self.name.ends_with("_qsv"){"ICQ; no hard bitrate cap"}else if self.name=="libsvtav1"{"low-delay CQP; no hard bitrate cap"}else{"encoder-specific quality with VBV"},
            "gop_seconds":self.gop_us as f64/1e6,"time_base":"1/90000","ffmpeg_version":unsafe{std::ffi::CStr::from_ptr(ff::ffi::av_version_info()).to_string_lossy()}})
    }
    fn drain(&mut self) -> Result<Vec<ff::Packet>> {
        let mut packets = Vec::new();
        loop {
            let mut packet = ff::Packet::empty();
            match self.inner.receive_packet(&mut packet) {
                Ok(()) => packets.push(packet),
                Err(ff::Error::Eof)
                | Err(ff::Error::Other {
                    errno: ff::error::EAGAIN,
                }) => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(packets)
    }
}
impl VideoEncoder for FfmpegVideoEncoder {
    type Packet = ff::Packet;
    fn encode(&mut self, video: &VideoFrame) -> Result<Vec<ff::Packet>> {
        video.validate()?;
        let (width, height) = (self.inner.width(), self.inner.height());
        let scale = (width as f64 / video.width as f64).min(height as f64 / video.height as f64);
        let sw = ((video.width as f64 * scale) as u32 & !1).max(2);
        let sh = ((video.height as f64 * scale) as u32 & !1).max(2);
        if self.source_size != (video.width, video.height) {
            let mut scaler = ff::software::scaling::Context::get(
                ff::format::Pixel::BGRA,
                video.width,
                video.height,
                self.inner.format(),
                sw,
                sh,
                ff::software::scaling::Flags::BICUBIC,
            )?;
            unsafe {
                let coefficients = ff::ffi::sws_getCoefficients(ff::ffi::SWS_CS_ITU709);
                ff::ffi::sws_setColorspaceDetails(
                    scaler.as_mut_ptr(),
                    coefficients,
                    1,
                    coefficients,
                    0,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }
            self.scaler = Some(scaler);
            self.source_size = (video.width, video.height);
        }
        let (scaled, _) = writable_video_frame(&mut self.scaled_frame, self.inner.format(), sw, sh);
        scale_bgra(self.scaler.as_mut().unwrap(), video, scaled)?;
        let output = if sw == width && sh == height {
            scaled
        } else {
            let (canvas, allocated) =
                writable_video_frame(&mut self.canvas_frame, self.inner.format(), width, height);
            if allocated || self.canvas_layout != Some((sw, sh)) {
                for plane in 0..canvas.planes() {
                    canvas
                        .data_mut(plane)
                        .fill(if plane == 0 { 16 } else { 128 });
                }
                self.canvas_layout = Some((sw, sh));
            }
            let x = ((width - sw) / 2) as usize & !1;
            let y = ((height - sh) / 2) as usize & !1;
            for plane in 0..scaled.planes() {
                let (px, py, row_bytes, rows) = if plane == 0 {
                    (x, y, sw as usize, sh as usize)
                } else if self.inner.format() == ff::format::Pixel::NV12 {
                    (x, y / 2, sw as usize, sh as usize / 2)
                } else {
                    (x / 2, y / 2, sw as usize / 2, sh as usize / 2)
                };
                let (ds, ss) = (canvas.stride(plane), scaled.stride(plane));
                for r in 0..rows {
                    canvas.data_mut(plane)[(py + r) * ds + px..(py + r) * ds + px + row_bytes]
                        .copy_from_slice(&scaled.data(plane)[r * ss..r * ss + row_bytes]);
                }
            }
            canvas
        };
        output.set_pts(Some(video.pts_us * 90 / 1000));
        // A reused frame must not inherit the preceding forced keyframe.
        output.set_kind(ff::picture::Type::None);
        if video.pts_us >= self.next_keyframe {
            output.set_kind(ff::picture::Type::I);
            self.next_keyframe = video.pts_us + self.gop_us;
        }
        self.inner.send_frame(output)?;
        self.drain()
    }
    fn flush(&mut self) -> Result<Vec<ff::Packet>> {
        self.inner.send_eof()?;
        self.drain()
    }
}

/// Reuse only buffers no longer held by an asynchronous encoder. Allocating a
/// fresh frame is cheaper than making a shared frame writable by copying its
/// previous pixels, which will be overwritten anyway.
fn writable_video_frame(
    slot: &mut Option<ff::frame::Video>,
    format: ff::format::Pixel,
    width: u32,
    height: u32,
) -> (&mut ff::frame::Video, bool) {
    let reusable = slot.as_mut().is_some_and(|frame| {
        frame.format() == format && frame.width() == width && frame.height() == height
            // SAFETY: the owned frame is alive for the duration of this query.
            && unsafe { ff::ffi::av_frame_is_writable(frame.as_mut_ptr()) != 0 }
    });
    if !reusable {
        *slot = Some(ff::frame::Video::new(format, width, height));
    }
    (slot.as_mut().expect("frame allocated"), !reusable)
}

/// Convert owned capture pixels directly when they include libswscale's
/// required overread padding. External/test frames without it use a safe copy.
fn scale_bgra(
    scaler: &mut ff::software::scaling::Context,
    video: &VideoFrame,
    output: &mut ff::frame::Video,
) -> Result<()> {
    let image_bytes = video.stride.checked_mul(video.height as usize);
    let padded = image_bytes
        .and_then(|size| size.checked_add(VideoFrame::PADDING))
        .is_some_and(|size| video.data.len() >= size);
    if padded {
        let stride = i32::try_from(video.stride).context("BGRA stride is too large")?;
        let height = i32::try_from(video.height).context("BGRA height is too large")?;
        let source = [
            video.data.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        ];
        let strides = [stride, 0, 0, 0];
        // SAFETY: validate() checked the frame geometry; the complete BGRA
        // plane and its initialized 16-byte padding remain alive throughout
        // this synchronous call. output owns its allocated FFmpeg planes.
        let rows = unsafe {
            ff::ffi::sws_scale(
                scaler.as_mut_ptr(),
                source.as_ptr(),
                strides.as_ptr(),
                0,
                height,
                (*output.as_mut_ptr()).data.as_ptr(),
                (*output.as_mut_ptr()).linesize.as_ptr(),
            )
        };
        anyhow::ensure!(
            rows == output.height() as i32,
            "BGRA conversion failed: {rows}"
        );
    } else {
        let mut input = ff::frame::Video::new(ff::format::Pixel::BGRA, video.width, video.height);
        let stride = input.stride(0);
        let row_bytes = video.width as usize * 4;
        for row in 0..video.height as usize {
            input.data_mut(0)[row * stride..row * stride + row_bytes]
                .copy_from_slice(&video.data[row * video.stride..row * video.stride + row_bytes]);
        }
        scaler.run(&input, output)?;
    }
    Ok(())
}

/// Exercise the actual device, encoder and decoder; enumeration alone is insufficient.
pub fn probe_one(name: &str, width: u32, height: u32, config: &RecordingConfig) -> Result<()> {
    let mut enc = FfmpegVideoEncoder::new(name, width, height, config)?;
    let mut decoder = ff::codec::context::Context::from_parameters(enc.parameters())?
        .decoder()
        .video()?;
    let pixels = Arc::new(vec![90; width as usize * height as usize * 4]);
    let mut packets = Vec::new();
    for i in 0..3 {
        packets.extend(enc.encode(&VideoFrame {
            width,
            height,
            stride: width as usize * 4,
            pts_us: i * 66_667,
            data: pixels.clone(),
        })?);
    }
    packets.extend(enc.flush()?);
    let mut decoded = 0;
    for packet in packets {
        decoder.send_packet(&packet)?;
        while decoder
            .receive_frame(&mut ff::frame::Video::empty())
            .is_ok()
        {
            decoded += 1;
        }
    }
    decoder.send_eof()?;
    while decoder
        .receive_frame(&mut ff::frame::Video::empty())
        .is_ok()
    {
        decoded += 1;
    }
    anyhow::ensure!(decoded > 0, "Encoder produced no decodable frames");
    Ok(())
}
pub fn probe_encoders() -> Vec<EncoderCapabilities> {
    let _ = super::initialize();
    [
        "h264_videotoolbox",
        "hevc_videotoolbox",
        "h264_qsv",
        "hevc_qsv",
        "av1_qsv",
        "h264_nvenc",
        "hevc_nvenc",
        "av1_nvenc",
        "h264_amf",
        "hevc_amf",
        "av1_amf",
        "libx264",
        "libx265",
        "libsvtav1",
    ]
    .into_iter()
    .map(|name| {
        let result = probe_one(name, 640, 360, &RecordingConfig::default());
        EncoderCapabilities {
            name: name.into(),
            codec: if name.contains("264") {
                "h264"
            } else if name.contains("265") || name.contains("hevc") {
                "hevc"
            } else {
                "av1"
            }
            .into(),
            available: result.is_ok(),
            hardware: !name.starts_with("lib"),
            reason: result.err().map(|e| format!("{e:#}")),
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_frames_never_overwrite_a_buffer_still_held_by_encoder() {
        super::super::initialize().unwrap();
        let mut slot = None;
        let (frame, allocated) =
            writable_video_frame(&mut slot, ff::format::Pixel::YUV420P, 64, 36);
        assert!(allocated);
        frame.data_mut(0).fill(42);
        let mut held = ff::frame::Video::empty();
        // Model the reference an asynchronous encoder retains after send_frame.
        assert_eq!(
            unsafe { ff::ffi::av_frame_ref(held.as_mut_ptr(), frame.as_ptr()) },
            0
        );
        let (frame, allocated) =
            writable_video_frame(&mut slot, ff::format::Pixel::YUV420P, 64, 36);
        assert!(allocated);
        frame.data_mut(0).fill(99);
        assert_eq!(held.data(0)[0], 42);
        let pointer = frame.data(0).as_ptr();
        let (frame, allocated) =
            writable_video_frame(&mut slot, ff::format::Pixel::YUV420P, 64, 36);
        assert!(!allocated);
        assert_eq!(frame.data(0).as_ptr(), pointer);
    }

    #[test]
    fn fast_software_mode_changes_the_x264_preset() {
        super::super::initialize().unwrap();
        let standard = RecordingConfig::default();
        let fast = RecordingConfig {
            fast_software_encoding: true,
            ..standard.clone()
        };
        let standard_encoder = FfmpegVideoEncoder::new("libx264", 320, 180, &standard).unwrap();
        let fast_encoder = FfmpegVideoEncoder::new("libx264", 320, 180, &fast).unwrap();
        assert_eq!(standard_encoder.options["preset"], "veryfast");
        assert_eq!(fast_encoder.options["preset"], "ultrafast");
        drop((standard_encoder, fast_encoder));
        probe_one("libx264", 320, 180, &fast).unwrap();
        if ff::encoder::find_by_name("libx265").is_some() {
            probe_one("libx265", 320, 180, &fast).unwrap();
        }
    }

    #[test]
    fn padded_capture_matches_copied_conversion_for_strides_and_formats() {
        super::super::initialize().unwrap();
        for (width, height, extra_stride, out_width, out_height, format) in [
            (64, 36, 0, 64, 36, ff::format::Pixel::YUV420P),
            (66, 38, 12, 34, 20, ff::format::Pixel::YUV420P),
            (66, 38, 12, 34, 20, ff::format::Pixel::NV12),
        ] {
            let stride = width as usize * 4 + extra_stride;
            let mut pixels = vec![0; stride * height as usize];
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let i = y * stride + x * 4;
                    pixels[i..i + 4].copy_from_slice(&[
                        (x * 11 + y * 3) as u8,
                        (x * 7 + y * 13) as u8,
                        (x * 5 + y * 17) as u8,
                        255,
                    ]);
                }
            }
            let copied = VideoFrame {
                width,
                height,
                stride,
                pts_us: 0,
                data: Arc::new(pixels.clone()),
            };
            let direct = VideoFrame {
                data: VideoFrame::copy_pixels(&pixels),
                ..copied.clone()
            };
            let make_scaler = || {
                ff::software::scaling::Context::get(
                    ff::format::Pixel::BGRA,
                    width,
                    height,
                    format,
                    out_width,
                    out_height,
                    ff::software::scaling::Flags::BICUBIC,
                )
                .unwrap()
            };
            let mut expected = ff::frame::Video::new(format, out_width, out_height);
            let mut actual = ff::frame::Video::new(format, out_width, out_height);
            copied.validate().unwrap();
            direct.validate().unwrap();
            scale_bgra(&mut make_scaler(), &copied, &mut expected).unwrap();
            scale_bgra(&mut make_scaler(), &direct, &mut actual).unwrap();
            for plane in 0..actual.planes() {
                let (row_bytes, rows) = if plane == 0 {
                    (out_width as usize, out_height as usize)
                } else if format == ff::format::Pixel::NV12 {
                    (out_width as usize, out_height as usize / 2)
                } else {
                    (out_width as usize / 2, out_height as usize / 2)
                };
                for row in 0..rows {
                    let a = row * actual.stride(plane);
                    let b = row * expected.stride(plane);
                    assert_eq!(
                        &actual.data(plane)[a..a + row_bytes],
                        &expected.data(plane)[b..b + row_bytes],
                        "format {format:?}, plane {plane}, row {row}"
                    );
                }
            }
        }
    }
}
