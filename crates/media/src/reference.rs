use anyhow::{Context, Result};
use ffmpeg_next as ff;
use opencap_core::*;
use std::{path::Path, sync::Arc, time::Instant};

/// Codec comparison with a supplied reference. Audio is intentionally excluded from
/// this video-only test and this distinction is recorded in the report.
pub fn encode_reference(input: &Path, output: &Path, config: &RecordingConfig) -> Result<()> {
    crate::initialize()?;
    anyhow::ensure!(!output.exists(), "Output already exists");
    let mut source = ff::format::input(input)?;
    let stream = source
        .streams()
        .best(ff::media::Type::Video)
        .context("Reference has no video stream")?;
    let index = stream.index();
    let tb = stream.time_base();
    let nominal = stream.avg_frame_rate();
    let frame_duration = if nominal.numerator() > 0 && nominal.denominator() > 0 {
        1_000_000i64 * nominal.denominator() as i64 / nominal.numerator() as i64
    } else {
        33_333
    };
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let mut cfg = config.clone();
    cfg.system_audio = false;
    cfg.microphone = false;
    cfg.reserve_audio_track = false;
    let partial = output.with_extension("partial.mp4");
    anyhow::ensure!(!partial.exists(), "Partial output exists");
    let mut writer = crate::MediaWriter::new(&partial, decoder.width(), decoder.height(), &cfg)?;
    let name = writer.video.name.clone();
    let options = writer.video.options.clone();
    let mut scaler = ff::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ff::format::Pixel::BGRA,
        decoder.width(),
        decoder.height(),
        ff::software::scaling::Flags::BICUBIC,
    )?;
    let mut policy = opencap_policy::FramePolicy::default();
    let mut first_pts = None;
    let mut end = 0;
    let start = Instant::now();
    let mut consume = |decoder: &mut ff::decoder::Video| -> Result<()> {
        loop {
            let mut decoded = ff::frame::Video::empty();
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {}
                Err(ff::Error::Eof)
                | Err(ff::Error::Other {
                    errno: ff::error::EAGAIN,
                }) => break,
                Err(e) => return Err(e.into()),
            }
            let timestamp = decoded
                .timestamp()
                .map(|p| ff::Rescale::rescale(&p, tb, (1, 1_000_000)))
                .unwrap_or(end);
            let origin = *first_pts.get_or_insert(timestamp);
            let pts = (timestamp - origin).max(0);
            let duration = unsafe { (*decoded.as_ptr()).duration };
            end = pts
                + if duration > 0 {
                    ff::Rescale::rescale(&duration, tb, (1, 1_000_000))
                } else {
                    frame_duration
                };
            let mut bgra = ff::frame::Video::empty();
            scaler.run(&decoded, &mut bgra)?;
            let video = VideoFrame {
                width: bgra.width(),
                height: bgra.height(),
                stride: bgra.stride(0),
                pts_us: pts,
                data: Arc::new(bgra.data(0).to_vec()),
            };
            if policy.accept(&video, &cfg) {
                writer.video(&video)?;
            }
        }
        Ok(())
    };
    for (stream, packet) in source.packets() {
        if stream.index() == index {
            decoder.send_packet(&packet)?;
            consume(&mut decoder)?;
        }
    }
    decoder.send_eof()?;
    consume(&mut decoder)?;
    let frames = writer.frames;
    anyhow::ensure!(frames > 0, "No decoded reference frames");
    writer.finish(end)?;
    crate::recover_file(&partial, output)?;
    std::fs::remove_file(partial)?;
    let bytes = std::fs::metadata(output)?.len();
    let report = serde_json::json!({"source":input,"audio_included":false,"config":cfg,"encoder":name,"encoder_options":options,"output_media":crate::inspect_output(output)?,"duration_seconds":end as f64/1e6,"encoded_frames":frames,"bytes":bytes,"elapsed_seconds":start.elapsed().as_secs_f64(),"quality_target_passed":null,"text_readability":"manual review required"});
    std::fs::write(
        output.with_extension("benchmark.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
