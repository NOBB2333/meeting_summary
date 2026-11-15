use crate::{
    encoder::{FfmpegVideoEncoder, VIDEO_TIME_BASE, probe_one},
    mixer::AudioMixer,
};
use anyhow::{Context, Result};
use ffmpeg_next as ff;
use opencap_core::*;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::OnceLock,
};

/// Cache only successful software probes. A hardware encoder's actual device
/// and driver identity are not yet known here, so caching it would be unsafe.
#[derive(Clone, PartialEq, Eq)]
struct SoftwareProbeKey {
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
    max_bitrate_kbps: u32,
    gop_seconds: u32,
    fast: bool,
    available_threads: usize,
    options: BTreeMap<String, String>,
}
impl SoftwareProbeKey {
    fn new(name: &str, width: u32, height: u32, config: &RecordingConfig) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            fps: config.max_fps,
            quality: config.quality,
            max_bitrate_kbps: config.max_bitrate_kbps,
            gop_seconds: config.gop_seconds,
            fast: config.fast_software_encoding,
            available_threads: std::thread::available_parallelism().map_or(2, usize::from),
            options: config.encoder_options.clone(),
        }
    }
}
#[derive(Default)]
struct SoftwareProbeCache(VecDeque<SoftwareProbeKey>);
impl SoftwareProbeCache {
    fn contains(&self, key: &SoftwareProbeKey) -> bool {
        self.0.contains(key)
    }
    fn insert(&mut self, key: SoftwareProbeKey) {
        self.remove(&key);
        if self.0.len() == 32 {
            self.0.pop_front();
        }
        self.0.push_back(key);
    }
    fn remove(&mut self, key: &SoftwareProbeKey) {
        self.0.retain(|entry| entry != key);
    }
}
fn software_probe_cache() -> &'static parking_lot::Mutex<SoftwareProbeCache> {
    static CACHE: OnceLock<parking_lot::Mutex<SoftwareProbeCache>> = OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(SoftwareProbeCache::default()))
}

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
    audio_observers: Vec<AudioObserver>,
    audio_generation: u64,
    video_mux_time: crate::timing::TimingHistogram,
}

struct AudioObserver {
    sender: crossbeam_channel::Sender<PcmAudio>,
    source: AudioSource,
    dropped_packets: u64,
    consumer: &'static str,
    disconnected: bool,
    full_packets: u64,
    disconnected_packets: u64,
    dropped_audio_frames: u64,
    dropped_boundaries: u64,
}
impl AudioObserver {
    fn send(&mut self, audio: PcmAudio) {
        let error = match self.sender.try_send(audio) {
            Ok(()) => return,
            Err(error) => error,
        };
        let packet = match error {
            crossbeam_channel::TrySendError::Full(packet) => {
                self.full_packets = self.full_packets.saturating_add(1);
                packet
            }
            crossbeam_channel::TrySendError::Disconnected(packet) => {
                self.disconnected = true;
                self.disconnected_packets = self.disconnected_packets.saturating_add(1);
                packet
            }
        };
        self.dropped_packets = self.dropped_packets.saturating_add(1);
        if packet.samples.is_empty() {
            self.dropped_boundaries = self.dropped_boundaries.saturating_add(1);
        } else {
            self.dropped_audio_frames = self.dropped_audio_frames.saturating_add(
                (packet.samples.len() / usize::from(packet.channels.max(1))) as u64,
            );
        }
    }
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
            let key = name
                .starts_with("lib")
                .then(|| SoftwareProbeKey::new(name, width, height, config));
            let cached = key
                .as_ref()
                .is_some_and(|key| software_probe_cache().lock().contains(key));
            let result = if cached {
                Ok(())
            } else {
                probe_one(name, width, height, config)
            }
            .and_then(|_| FfmpegVideoEncoder::new(name, width, height, config));
            match result {
                Ok(encoder) => {
                    if let Some(key) = key {
                        software_probe_cache().lock().insert(key);
                    }
                    chosen = Some(encoder);
                    break;
                }
                Err(e) => {
                    if let Some(key) = key {
                        software_probe_cache().lock().remove(&key);
                    }
                    failures.push(format!("{name}: {e:#}"));
                }
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
            audio_observers: Vec::new(),
            audio_generation: 0,
            video_mux_time: Default::default(),
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
        let packets = encoder.encode(frame)?;
        let mux_started = std::time::Instant::now();
        for p in packets {
            self.write_video_packet(p)?;
        }
        self.video_mux_time.record(mux_started.elapsed());
        self.frames += 1;
        self.last_video_us = frame.pts_us;
        Ok(())
    }
    pub(crate) fn take_video_stage_timings(&mut self) -> serde_json::Value {
        serde_json::json!({"encoder":self.video.as_mut().map(FfmpegVideoEncoder::take_stage_timings), "mux":self.video_mux_time.take()})
    }
    pub fn audio(&mut self, chunk: AudioChunk) {
        if self.audio.is_some() {
            self.mixer.push(chunk);
        }
    }
    pub fn observe_audio(&mut self, observer: crossbeam_channel::Sender<PcmAudio>) {
        self.audio_observers.push(AudioObserver {
            sender: observer,
            source: AudioSource::MixedRecording,
            dropped_packets: 0,
            consumer: "other",
            disconnected: false,
            full_packets: 0,
            disconnected_packets: 0,
            dropped_audio_frames: 0,
            dropped_boundaries: 0,
        });
    }
    pub fn observe_audio_source(
        &mut self,
        observer: crossbeam_channel::Sender<PcmAudio>,
        source: AudioSource,
    ) -> Result<()> {
        self.observe_audio_source_for(observer, source, "other")
    }
    pub(crate) fn observe_audio_source_for(
        &mut self,
        observer: crossbeam_channel::Sender<PcmAudio>,
        source: AudioSource,
        consumer: &'static str,
    ) -> Result<()> {
        anyhow::ensure!(
            source != AudioSource::ImportedMedia,
            "录制不能输出导入媒体来源"
        );
        self.audio_observers.push(AudioObserver {
            sender: observer,
            source,
            dropped_packets: 0,
            consumer,
            disconnected: false,
            full_packets: 0,
            disconnected_packets: 0,
            dropped_audio_frames: 0,
            dropped_boundaries: 0,
        });
        Ok(())
    }
    pub fn audio_boundary(&mut self) {
        self.audio_generation = self.audio_generation.saturating_add(1);
        for observer in &mut self.audio_observers {
            let audio = PcmAudio {
                pts_us: self.mixer.cursor * 1000 / 48,
                sample_rate: 48000,
                channels: 1,
                source: observer.source,
                generation: self.audio_generation,
                samples: Vec::new().into(),
            };
            observer.send(audio);
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
            let want_separate = self
                .audio_observers
                .iter()
                .any(|observer| observer.source != AudioSource::MixedRecording);
            let (samples, system, microphone) = if want_separate {
                let (mixed, system, microphone) = self.mixer.take_separated(count);
                (mixed, Some(system), Some(microphone))
            } else {
                (self.mixer.take(count), None, None)
            };
            if !self.audio_observers.is_empty() {
                // Allocate at most once per requested source; independent bounded
                // queues share the same Arc and never delay the recorder.
                let mono = |values: &[[f32; 2]]| -> std::sync::Arc<[f32]> {
                    values
                        .iter()
                        .map(|sample| (sample[0] + sample[1]) * 0.5)
                        .collect::<Vec<_>>()
                        .into()
                };
                let mixed = self
                    .audio_observers
                    .iter()
                    .any(|observer| observer.source == AudioSource::MixedRecording)
                    .then(|| mono(&samples));
                let system = system
                    .as_ref()
                    .filter(|_| {
                        self.audio_observers
                            .iter()
                            .any(|observer| observer.source == AudioSource::System)
                    })
                    .map(|values| mono(values));
                let microphone = microphone
                    .as_ref()
                    .filter(|_| {
                        self.audio_observers
                            .iter()
                            .any(|observer| observer.source == AudioSource::Microphone)
                    })
                    .map(|values| mono(values));
                for observer in &mut self.audio_observers {
                    let shared = match observer.source {
                        AudioSource::MixedRecording => mixed.as_ref(),
                        AudioSource::System => system.as_ref(),
                        AudioSource::Microphone => microphone.as_ref(),
                        AudioSource::ImportedMedia => None,
                    };
                    let Some(shared) = shared else {
                        continue;
                    };
                    let audio = PcmAudio {
                        pts_us: pts * 1000 / 48,
                        sample_rate: 48000,
                        channels: 1,
                        source: observer.source,
                        generation: self.audio_generation,
                        samples: shared.clone(),
                    };
                    observer.send(audio);
                }
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
    pub fn finish(self, stop_us: i64) -> Result<()> {
        self.finish_with_observer_status(stop_us).map(|_| ())
    }
    pub(crate) fn finish_with_observer_status(
        mut self,
        stop_us: i64,
    ) -> Result<Vec<serde_json::Value>> {
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
        Ok(self.audio_observer_status())
    }
    pub fn asr_pending_packets(&self) -> Option<usize> {
        self.audio_observers
            .iter()
            .find(|observer| observer.consumer == "asr")
            .map(|observer| observer.sender.len())
    }
    pub fn audio_observer_drops(&self) -> Vec<u64> {
        self.audio_observers
            .iter()
            .map(|observer| observer.dropped_packets)
            .collect()
    }
    pub fn audio_observer_status(&self) -> Vec<serde_json::Value> {
        self.audio_observers
            .iter()
            .map(|observer| {
                serde_json::json!({
                    "source": observer.source,
                    "consumer": observer.consumer,
                    "queued_packets": observer.sender.len(),
                    "capacity_packets": observer.sender.capacity(),
                    "dropped_packets": observer.dropped_packets,
                    "queue_full_packets": observer.full_packets,
                    "receiver_disconnected_packets": observer.disconnected_packets,
                    "dropped_audio_frames": observer.dropped_audio_frames,
                    "dropped_boundary_events": observer.dropped_boundaries,
                    "audio_sample_rate_hz": 48000,
                    "receiver_disconnected": observer.disconnected,
                })
            })
            .collect()
    }
    pub fn audio_cursor_us(&self) -> Option<i64> {
        self.audio.as_ref().map(|_| self.mixer.cursor * 1000 / 48)
    }
    /// Media time at which the next complete AAC packet can be written.
    /// Keep the same 150 ms capture/mixing allowance as the recording loop.
    pub fn next_audio_due_us(&self) -> Option<i64> {
        self.audio.as_ref().map(|audio| {
            ((self.mixer.cursor + i64::from(audio.frame_size())) * 1_000 + 47) / 48 + 150_000
        })
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
    let filename = output
        .file_name()
        .context("Recovery output has no filename")?
        .to_string_lossy();
    let temporary = output.with_file_name(format!(
        ".{filename}.recovery-{}.partial",
        opencap_core::new_stable_id()
    ));
    let temporary_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    recover_into_temporary(input, output, &temporary, &temporary_file).with_context(|| {
        format!(
            "恢复未完全结束；原文件保留，暂存文件位置：{}；目标若已发布请先检查：{}",
            temporary.display(),
            output.display()
        )
    })
}

fn recover_into_temporary(
    input: &Path,
    output: &Path,
    temporary: &Path,
    temporary_file: &std::fs::File,
) -> Result<()> {
    let temporary_identity = same_file::Handle::from_file(temporary_file.try_clone()?)?;
    let source_file = opencap_core::open_regular_file_for_read(input)?;
    let source_identity = same_file::Handle::from_file(source_file.try_clone()?)?;
    let source_before = source_file.metadata()?;
    anyhow::ensure!(
        source_identity
            == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(input)?)?,
        "恢复前源文件已被替换"
    );
    let mut source = ff::format::input(input)?;
    verify_temporary_path(temporary, &temporary_identity)?;
    let mut dest = ff::format::output_as(&temporary, "mp4")?;
    let mut time_bases = Vec::new();
    let mut video_timings = Vec::new();
    for stream in source.streams() {
        let mut target = dest.add_stream(ff::encoder::find(stream.parameters().id()))?;
        target.set_parameters(stream.parameters());
        target.set_time_base(stream.time_base());
        time_bases.push(stream.time_base());
        video_timings.push(if stream.parameters().medium() == ff::media::Type::Video {
            Some((stream.start_time(), stream.duration()))
        } else {
            None
        });
    }
    let mut options = ff::Dictionary::new();
    options.set("movflags", "+faststart");
    dest.write_header_with(options)?;
    let mut count = 0;
    let mut last_video: Vec<Option<ff::Packet>> = (0..time_bases.len()).map(|_| None).collect();
    let mut first_video_pts = vec![None; time_bases.len()];
    let mut reordered_video = vec![false; time_bases.len()];
    for (_, mut packet) in source.packets() {
        let index = packet.stream();
        packet.rescale_ts(time_bases[index], dest.stream(index).unwrap().time_base());
        packet.set_position(-1);
        if video_timings[index].is_some() {
            if let (Some(pts), Some(dts)) = (packet.pts(), packet.dts())
                && pts != dts
            {
                reordered_video[index] = true;
            }
            if first_video_pts[index].is_none() {
                first_video_pts[index] = packet.pts();
            }
            if let Some(previous) = last_video[index].replace(packet) {
                previous.write_interleaved(&mut dest)?;
            }
        } else {
            packet.write_interleaved(&mut dest)?;
        }
        count += 1;
    }
    anyhow::ensure!(count > 0, "No recoverable packets");
    for (index, packet) in last_video.into_iter().enumerate() {
        if let Some(mut packet) = packet {
            let (start, duration) = video_timings[index].expect("video timing");
            let output_tb = dest.stream(index).unwrap().time_base();
            let start = if start != ff::ffi::AV_NOPTS_VALUE {
                Some(ff::Rescale::rescale(&start, time_bases[index], output_tb))
            } else {
                first_video_pts[index]
            };
            if duration > 0
                && !reordered_video[index]
                && let (Some(start), Some(pts)) = (start, packet.pts())
            {
                let duration = ff::Rescale::rescale(&duration, time_bases[index], output_tb);
                let tail = start.saturating_add(duration).saturating_sub(pts);
                // Older FFmpeg can demux the last packet with the nominal frame
                // duration even when the stream declares a longer static tail.
                if tail > packet.duration() {
                    packet.set_duration(tail);
                }
            }
            packet.write_interleaved(&mut dest)?;
        }
    }
    dest.write_trailer()?;
    drop(dest);
    // Verify an actual decoded frame, including recordings with no video stream.
    verify_temporary_path(temporary, &temporary_identity)?;
    let mut check = ff::format::input(&temporary)?;
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
    drop(check);
    drop(source);
    let source_after = source_file.metadata()?;
    anyhow::ensure!(
        source_before.len() == source_after.len()
            && source_before.modified()? == source_after.modified()?
            && source_identity
                == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(input)?)?,
        "恢复期间源文件已变化；未发布恢复结果"
    );
    verify_temporary_path(temporary, &temporary_identity)?;
    temporary_file.sync_all()?;
    std::fs::hard_link(temporary, output)
        .context("Recovery output could not be published without overwriting an existing file")?;
    verify_temporary_path(output, &temporary_identity)
        .context("恢复目标已发布，但无法确认其文件身份")?;
    opencap_core::sync_parent_directory(output)?;
    std::fs::remove_file(temporary).with_context(|| {
        format!(
            "Recovered output published at {}, but temporary file could not be removed: {}",
            output.display(),
            temporary.display()
        )
    })?;
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
    export_audio_cancellable(input, output, None)
}

pub fn export_audio_cancellable(
    input: &Path,
    output: &Path,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
        "音频导出已取消"
    );
    super::initialize()?;
    anyhow::ensure!(!output.exists(), "Audio output already exists");
    let filename = output
        .file_name()
        .context("Audio output has no filename")?
        .to_string_lossy();
    let temporary = output.with_file_name(format!(
        ".{filename}.export-{}.partial",
        opencap_core::new_stable_id()
    ));
    let temporary_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    export_audio_into_temporary(input, output, &temporary, &temporary_file, cancelled).with_context(
        || {
            format!(
                "音频导出未完全结束；暂存文件位置：{}；目标若已发布请先检查：{}",
                temporary.display(),
                output.display()
            )
        },
    )
}

fn export_audio_into_temporary(
    input: &Path,
    output: &Path,
    temporary: &Path,
    temporary_file: &std::fs::File,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let temporary_identity = same_file::Handle::from_file(temporary_file.try_clone()?)?;
    let source_file = opencap_core::open_regular_file_for_read(input)?;
    let source_identity = same_file::Handle::from_file(source_file.try_clone()?)?;
    let source_before = source_file.metadata()?;
    anyhow::ensure!(
        source_identity
            == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(input)?)?,
        "导出前源媒体已被替换"
    );
    let mut source = ff::format::input(input)?;
    let stream = source
        .streams()
        .best(ff::media::Type::Audio)
        .context("Recording has no audio track")?;
    let index = stream.index();
    let source_tb = stream.time_base();
    verify_temporary_path(temporary, &temporary_identity)?;
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
        anyhow::ensure!(
            !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
            "音频导出已取消"
        );
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
    drop(source);
    verify_temporary_path(temporary, &temporary_identity)?;
    let mut check = ff::format::input(temporary)?;
    let stream = check
        .streams()
        .best(ff::media::Type::Audio)
        .context("导出的文件没有音轨")?;
    let index = stream.index();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    let mut decoded = false;
    for (stream, packet) in check.packets() {
        anyhow::ensure!(
            !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
            "音频导出已取消"
        );
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
    anyhow::ensure!(decoded, "导出的音频无法解码");
    drop(check);
    let source_after = source_file.metadata()?;
    anyhow::ensure!(
        source_before.len() == source_after.len()
            && source_before.modified()? == source_after.modified()?
            && source_identity
                == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(input)?)?,
        "导出期间源媒体已变化；未发布音频文件"
    );
    verify_temporary_path(temporary, &temporary_identity)?;
    temporary_file.sync_all()?;
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
        "音频导出已取消；暂存文件保留"
    );
    // Hard-link publishes atomically and refuses to replace any existing output.
    std::fs::hard_link(temporary, output)?;
    verify_temporary_path(output, &temporary_identity)
        .context("音频目标已发布，但无法确认其文件身份")?;
    opencap_core::sync_parent_directory(output)?;
    std::fs::remove_file(temporary)?;
    Ok(())
}

fn verify_temporary_path(path: &Path, identity: &same_file::Handle) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "暂存或发布路径不是普通文件：{}",
        path.display()
    );
    anyhow::ensure!(
        *identity == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(path)?)?,
        "暂存或发布文件身份已变化：{}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod probe_cache_tests {
    use super::*;

    #[test]
    fn cached_probe_requires_matching_encoder_geometry_and_options_and_is_bounded() {
        let config = RecordingConfig::default();
        let original = SoftwareProbeKey::new("libx264", 1280, 720, &config);
        let mut cache = SoftwareProbeCache::default();
        cache.insert(original.clone());
        assert!(cache.contains(&original));
        assert!(!cache.contains(&SoftwareProbeKey::new("libx264", 1920, 1080, &config)));
        let modified = RecordingConfig {
            fast_software_encoding: true,
            ..config.clone()
        };
        assert!(!cache.contains(&SoftwareProbeKey::new("libx264", 1280, 720, &modified)));
        for width in 1..=32 {
            cache.insert(SoftwareProbeKey::new("libx264", width, 720, &config));
        }
        assert_eq!(cache.0.len(), 32);
        assert!(!cache.contains(&original));
    }
}
