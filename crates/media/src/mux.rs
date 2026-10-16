use crate::{
    encoder::{FfmpegVideoEncoder, VIDEO_TIME_BASE, probe_one},
    mixer::AudioMixer,
};
use anyhow::{Context, Result};
use ffmpeg_next as ff;
use opencap_core::*;
use std::path::Path;

pub struct MediaWriter {
    output: ff::format::context::Output,
    pub video: Option<FfmpegVideoEncoder>,
    audio: Option<ff::encoder::audio::Encoder>,
    mixer: AudioMixer,
    video_tb: ff::Rational,
    audio_tb: ff::Rational,
    audio_stream: usize,
    pending: Option<ff::Packet>,
    pub frames: u64,
    pub last_video_us: i64,
    audio_observer: Option<crossbeam_channel::Sender<PcmAudio>>,
}
impl MediaWriter {
    pub fn new(path: &Path, width: u32, height: u32, config: &RecordingConfig) -> Result<Self> {
        if config.audio_only {
            return Self::new_audio(path, config);
        }
        super::initialize()?;
        config.validate()?;
        let mut failures = Vec::new();
        let mut chosen = None;
        let candidates = opencap_policy::prefer_adapter(
            opencap_policy::candidates(config),
            opencap_platform::capture_vendor(&config.target),
        );
        for name in candidates {
            match probe_one(name, width, height, config)
                .and_then(|_| FfmpegVideoEncoder::new(name, width, height, config))
            {
                Ok(encoder) => {
                    chosen = Some(encoder);
                    break;
                }
                Err(e) => failures.push(format!("{name}: {e:#}")),
            }
        }
        let video =
            chosen.with_context(|| format!("No usable encoder: {}", failures.join("; ")))?;
        Self::open(path, config, Some(video))
    }
    pub fn new_audio(path: &Path, config: &RecordingConfig) -> Result<Self> {
        super::initialize()?;
        config.validate()?;
        anyhow::ensure!(
            config.system_audio || config.microphone || config.reserve_audio_track,
            "仅录音需要开启系统声音或麦克风"
        );
        Self::open(path, config, None)
    }
    fn open(
        path: &Path,
        config: &RecordingConfig,
        video: Option<FfmpegVideoEncoder>,
    ) -> Result<Self> {
        let mut output = ff::format::output_as(path, "mp4")?;
        unsafe {
            (*output.as_mut_ptr()).max_interleave_delta = 500_000;
        }
        if let Some(video) = &video {
            let mut stream = output.add_stream(ff::encoder::find(video.inner.id()))?;
            stream.set_parameters(&video.inner);
            stream.set_time_base(VIDEO_TIME_BASE);
            if video.inner.id() == ff::codec::Id::HEVC {
                unsafe {
                    (*stream.parameters().as_mut_ptr()).codec_tag = u32::from_le_bytes(*b"hvc1");
                }
            }
        }
        let audio_stream = usize::from(video.is_some());
        let audio = if config.system_audio || config.microphone || config.reserve_audio_track {
            let codec = ff::encoder::find(ff::codec::Id::AAC).context("AAC encoder missing")?;
            let mut encoder = ff::codec::context::Context::new_with_codec(codec)
                .encoder()
                .audio()?;
            encoder.set_rate(48000);
            encoder.set_channel_layout(if config.audio_channels == 1 {
                ff::ChannelLayout::MONO
            } else {
                ff::ChannelLayout::STEREO
            });
            encoder.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
            encoder.set_bit_rate(config.audio_bitrate_kbps as usize * 1000);
            encoder.set_time_base((1, 48000));
            encoder.set_flags(ff::codec::Flags::GLOBAL_HEADER);
            let encoder = encoder.open_as(codec)?;
            let mut stream = output.add_stream(codec)?;
            stream.set_parameters(&encoder);
            stream.set_time_base((1, 48000));
            Some(encoder)
        } else {
            None
        };
        let mut opts = ff::Dictionary::new();
        opts.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
        opts.set("frag_duration", "1000000");
        opts.set("flush_packets", "1");
        output.write_header_with(opts)?;
        let video_tb = if video.is_some() {
            output.stream(0).unwrap().time_base()
        } else {
            VIDEO_TIME_BASE
        };
        let audio_tb = output
            .stream(audio_stream)
            .map(|s| s.time_base())
            .unwrap_or((1, 48000).into());
        Ok(Self {
            output,
            video,
            audio,
            mixer: AudioMixer::new(),
            video_tb,
            audio_tb,
            audio_stream,
            pending: None,
            frames: 0,
            last_video_us: 0,
            audio_observer: None,
        })
    }
    fn write_video_packet(&mut self, mut packet: ff::Packet) -> Result<()> {
        packet.set_stream(0);
        packet.rescale_ts(VIDEO_TIME_BASE, self.video_tb);
        if let Some(mut previous) = self.pending.take() {
            let duration = packet.pts().unwrap_or(0) - previous.pts().unwrap_or(0);
            anyhow::ensure!(duration > 0, "Non-monotonic video timestamps");
            previous.set_duration(duration);
            previous.write_interleaved(&mut self.output)?;
        }
        self.pending = Some(packet);
        Ok(())
    }
    pub fn video(&mut self, frame: &VideoFrame) -> Result<()> {
        if self.frames > 0 && frame.pts_us <= self.last_video_us {
            return Ok(());
        }
        let encoder = self
            .video
            .as_mut()
            .context("Audio-only output cannot accept video frames")?;
        for p in encoder.encode(frame)? {
            self.write_video_packet(p)?;
        }
        self.frames += 1;
        self.last_video_us = frame.pts_us;
        Ok(())
    }
    pub fn audio(&mut self, chunk: AudioChunk) {
        if self.audio.is_some() {
            self.mixer.push(chunk);
        }
    }
    pub fn observe_audio(&mut self, observer: crossbeam_channel::Sender<PcmAudio>) {
        self.audio_observer = Some(observer);
    }
    pub fn audio_boundary(&self) {
        if let Some(observer) = &self.audio_observer {
            let _ = observer.try_send(PcmAudio {
                pts_us: self.mixer.cursor * 1000 / 48,
                sample_rate: 48000,
                channels: 1,
                samples: Vec::new(),
            });
        }
    }
    pub fn clear_audio_route(&mut self, kind: AudioKind) {
        self.mixer.clear_route(kind);
    }
    pub fn audio_until(&mut self, pts_us: i64, final_chunk: bool) -> Result<()> {
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        let target = pts_us.max(0) * 48 / 1000;
        let frame_size = audio.frame_size() as usize;
        anyhow::ensure!(frame_size > 0, "Invalid AAC frame size");
        while self.mixer.cursor + frame_size as i64 <= target
            || (final_chunk && self.mixer.cursor < target)
        {
            let count = if final_chunk {
                (target - self.mixer.cursor).min(frame_size as i64) as usize
            } else {
                frame_size
            };
            let pts = self.mixer.cursor;
            let samples = self.mixer.take(count);
            if let Some(observer) = &self.audio_observer {
                // Independent bounded ASR queue. Full/closed queues never delay saving.
                let _ = observer.try_send(PcmAudio {
                    pts_us: pts * 1000 / 48,
                    sample_rate: 48000,
                    channels: 1,
                    samples: samples.iter().map(|s| (s[0] + s[1]) * 0.5).collect(),
                });
            }
            let mut frame = ff::frame::Audio::new(audio.format(), count, audio.channel_layout());
            frame.set_rate(48000);
            frame.set_pts(Some(pts));
            for channel in 0..audio.channels() as usize {
                let mono = audio.channels() == 1;
                let plane = frame.plane_mut::<f32>(channel);
                for (i, sample) in samples.iter().enumerate() {
                    plane[i] = if mono {
                        (sample[0] + sample[1]) * 0.5
                    } else {
                        sample[channel]
                    };
                }
            }
            audio.send_frame(&frame)?;
            drain_audio(audio, &mut self.output, self.audio_tb, self.audio_stream)?;
        }
        Ok(())
    }
    pub fn finish(mut self, stop_us: i64) -> Result<()> {
        self.audio_until(stop_us, true)?;
        if let Some(video) = self.video.as_mut() {
            for packet in video.flush()? {
                self.write_video_packet(packet)?;
            }
        }
        if let Some(mut packet) = self.pending.take() {
            let end = ff::Rescale::rescale(&stop_us, (1, 1_000_000), self.video_tb);
            packet.set_duration((end - packet.pts().unwrap_or(0)).max(1));
            packet.write_interleaved(&mut self.output)?;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.send_eof()?;
            drain_audio(audio, &mut self.output, self.audio_tb, self.audio_stream)?;
        }
        self.output.write_trailer()?;
        Ok(())
    }
    pub fn asr_pending_packets(&self) -> Option<usize> {
        self.audio_observer.as_ref().map(|tx| tx.len())
    }
    pub fn audio_cursor_us(&self) -> Option<i64> {
        self.audio.as_ref().map(|_| self.mixer.cursor * 1000 / 48)
    }
    pub fn flush_io(&mut self) -> Result<()> {
        unsafe {
            let io = (*self.output.as_mut_ptr()).pb;
            if !io.is_null() {
                ff::ffi::avio_flush(io);
                anyhow::ensure!(
                    (*io).error >= 0,
                    "Output write failed: {}",
                    ff::Error::from((*io).error)
                );
            }
        }
        Ok(())
    }
}
fn drain_audio(
    audio: &mut ff::encoder::audio::Encoder,
    output: &mut ff::format::context::Output,
    tb: ff::Rational,
    stream_index: usize,
) -> Result<()> {
    loop {
        let mut packet = ff::Packet::empty();
        match audio.receive_packet(&mut packet) {
            Ok(()) => {
                packet.set_stream(stream_index);
                packet.rescale_ts((1, 48000), tb);
                packet.write_interleaved(output)?;
            }
            Err(ff::Error::Eof)
            | Err(ff::Error::Other {
                errno: ff::error::EAGAIN,
            }) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Remux only. The partial file is retained unless a caller explicitly removes it
/// after successful validation; source recordings are never overwritten.
pub fn recover_file(input: &Path, output: &Path) -> Result<()> {
    anyhow::ensure!(!output.exists(), "Recovery output already exists");
    super::initialize()?;
    let mut source = ff::format::input(input)?;
    let mut dest = ff::format::output_as(output, "mp4")?;
    let mut time_bases = Vec::new();
    for stream in source.streams() {
        let mut target = dest.add_stream(ff::encoder::find(stream.parameters().id()))?;
        target.set_parameters(stream.parameters());
        target.set_time_base(stream.time_base());
        time_bases.push(stream.time_base());
    }
    let mut options = ff::Dictionary::new();
    options.set("movflags", "+faststart");
    dest.write_header_with(options)?;
    let mut count = 0;
    for (_, mut packet) in source.packets() {
        let index = packet.stream();
        packet.rescale_ts(time_bases[index], dest.stream(index).unwrap().time_base());
        packet.set_position(-1);
        packet.write_interleaved(&mut dest)?;
        count += 1;
    }
    anyhow::ensure!(count > 0, "No recoverable packets");
    dest.write_trailer()?;
    drop(dest);
    // Verify an actual decoded frame, including recordings with no video stream.
    let mut check = ff::format::input(output)?;
    if check.streams().best(ff::media::Type::Video).is_some() {
        let stream = check
            .streams()
            .best(ff::media::Type::Video)
            .context("Recovered file has no video")?;
        let index = stream.index();
        let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()?;
        let mut decoded = false;
        for (stream, packet) in check.packets() {
            if stream.index() != index {
                continue;
            }
            decoder.send_packet(&packet)?;
            if decoder
                .receive_frame(&mut ff::frame::Video::empty())
                .is_ok()
            {
                decoded = true;
                break;
            }
        }
        if !decoded {
            decoder.send_eof()?;
            decoded = decoder
                .receive_frame(&mut ff::frame::Video::empty())
                .is_ok();
        }
        anyhow::ensure!(decoded, "Recovered video could not be decoded");
    } else {
        let stream = check
            .streams()
            .best(ff::media::Type::Audio)
            .context("Recovered file has no audio or video")?;
        let index = stream.index();
        let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let mut decoded = false;
        for (stream, packet) in check.packets() {
            if stream.index() == index {
                decoder.send_packet(&packet)?;
                if decoder
                    .receive_frame(&mut ff::frame::Audio::empty())
                    .is_ok()
                {
                    decoded = true;
                    break;
                }
            }
        }
        if !decoded {
            decoder.send_eof()?;
            decoded = decoder
                .receive_frame(&mut ff::frame::Audio::empty())
                .is_ok();
        }
        anyhow::ensure!(decoded, "Recovered audio could not be decoded");
    }
    Ok(())
}

pub fn inspect_output(path: &Path) -> Result<serde_json::Value> {
    let context = ff::format::input(path)?;
    let streams:Vec<_>=context.streams().map(|s| {
        let parameters=s.parameters();
        let profile=unsafe {
            let p=ff::ffi::avcodec_profile_name(parameters.id().into(),(*parameters.as_ptr()).profile);
            if p.is_null(){None}else{Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())}
        };
        serde_json::json!({"codec":parameters.id().name(),"profile":profile,"time_base":format!("{}/{}",s.time_base().numerator(),s.time_base().denominator()),"duration_seconds":s.duration() as f64*f64::from(s.time_base()),"average_frame_rate":format!("{}/{}",s.avg_frame_rate().numerator(),s.avg_frame_rate().denominator()),"frames":s.frames()})
    }).collect();
    Ok(serde_json::json!({"duration_seconds":context.duration() as f64/1e6,"streams":streams}))
}

/// Copy the mixed AAC track without a second encode or another audio capture session.
/// Write to a temporary file first; never overwrite an existing recording.
pub fn export_audio(input: &Path, output: &Path) -> Result<()> {
    super::initialize()?;
    anyhow::ensure!(!output.exists(), "Audio output already exists");
    let mut source = ff::format::input(input)?;
    let stream = source
        .streams()
        .best(ff::media::Type::Audio)
        .context("Recording has no audio track")?;
    let index = stream.index();
    let source_tb = stream.time_base();
    let temporary = output.with_extension("partial.m4a");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut dest = ff::format::output_as(&temporary, "mp4")?;
    let mut target = dest.add_stream(ff::encoder::find(stream.parameters().id()))?;
    target.set_parameters(stream.parameters());
    target.set_time_base(source_tb);
    let mut options = ff::Dictionary::new();
    options.set("movflags", "+faststart");
    dest.write_header_with(options)?;
    let dest_tb = dest.stream(0).unwrap().time_base();
    let mut packets = 0;
    for (stream, mut packet) in source.packets() {
        if stream.index() != index {
            continue;
        }
        packet.rescale_ts(source_tb, dest_tb);
        packet.set_stream(0);
        packet.set_position(-1);
        packet.write_interleaved(&mut dest)?;
        packets += 1;
    }
    anyhow::ensure!(packets > 0, "No audio packets");
    dest.write_trailer()?;
    drop(dest);
    // Hard-link publishes atomically and refuses to replace any existing output.
    std::fs::hard_link(&temporary, output)?;
    std::fs::remove_file(temporary)?;
    Ok(())
}
