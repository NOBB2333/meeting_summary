use anyhow::{Context, Result};
use ffmpeg_next as ff;
use opencap_core::PcmAudio;
use std::path::Path;

pub struct Normalizer {
    resampler: Option<ff::software::resampling::Context>,
    rate: u32,
}
impl Normalizer {
    pub fn new() -> Self {
        Self {
            resampler: None,
            rate: 0,
        }
    }
    pub fn convert(&mut self, chunk: &PcmAudio) -> Result<Vec<f32>> {
        anyhow::ensure!(chunk.channels > 0 && chunk.sample_rate > 0, "无效 PCM 格式");
        let mono: Vec<f32> = chunk
            .samples
            .chunks_exact(chunk.channels as usize)
            .map(|frame| {
                frame
                    .iter()
                    .map(|v| if v.is_finite() { *v } else { 0.0 })
                    .sum::<f32>()
                    / chunk.channels as f32
            })
            .collect();
        if chunk.sample_rate == 16000 {
            return Ok(mono);
        }
        if self.rate != chunk.sample_rate {
            self.rate = chunk.sample_rate;
            self.resampler = Some(ff::software::resampling::Context::get(
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                ff::ChannelLayout::MONO,
                chunk.sample_rate,
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                ff::ChannelLayout::MONO,
                16000,
            )?);
        }
        let mut input = ff::frame::Audio::new(
            ff::format::Sample::F32(ff::format::sample::Type::Packed),
            mono.len(),
            ff::ChannelLayout::MONO,
        );
        input.set_rate(chunk.sample_rate);
        input.plane_mut::<f32>(0)[..mono.len()].copy_from_slice(&mono);
        let mut out = ff::frame::Audio::empty();
        self.resampler.as_mut().unwrap().run(&input, &mut out)?;
        Ok(out.plane::<f32>(0)[..out.samples()].to_vec())
    }
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        loop {
            let mut out = ff::frame::Audio::new(
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                4096,
                ff::ChannelLayout::MONO,
            );
            resampler.flush(&mut out)?;
            if out.samples() == 0 {
                break;
            }
            result.extend_from_slice(&out.plane::<f32>(0)[..out.samples()]);
        }
        Ok(result)
    }
}

/// Decode incrementally; callback errors (including cancellation) stop reading.
/// libswresample retains filter history and emits its tail before EOF.
pub fn decode_file(
    path: &Path,
    mut accept: impl FnMut(PcmAudio, Option<f64>) -> Result<()>,
) -> Result<()> {
    ff::init()?;
    let mut input = ff::format::input(path).context("无法读取音频或视频文件")?;
    let stream = input
        .streams()
        .best(ff::media::Type::Audio)
        .context("文件中没有音轨")?;
    let index = stream.index();
    let tb = stream.time_base();
    let mut decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
    let duration = (input.duration() > 0).then_some(input.duration() as f64 / 1e6);
    let origin = unsafe { (*input.as_ptr()).start_time };
    let origin = if origin == ff::ffi::AV_NOPTS_VALUE {
        0
    } else {
        origin
    };
    let layout = if decoder.channel_layout().is_empty() {
        ff::ChannelLayout::default(decoder.channels() as i32)
    } else {
        decoder.channel_layout()
    };
    let mut resampler = ff::software::resampling::Context::get(
        decoder.format(),
        layout,
        decoder.rate(),
        ff::format::Sample::F32(ff::format::sample::Type::Packed),
        ff::ChannelLayout::MONO,
        16000,
    )?;
    let mut cursor = 0i64;
    let mut first = true;
    {
        let mut drain = |decoder: &mut ff::decoder::Audio| -> Result<()> {
            loop {
                let mut decoded = ff::frame::Audio::empty();
                match decoder.receive_frame(&mut decoded) {
                    Ok(()) => (),
                    Err(ff::Error::Eof)
                    | Err(ff::Error::Other {
                        errno: ff::error::EAGAIN,
                    }) => break,
                    Err(e) => return Err(e.into()),
                }
                if decoded.channel_layout().is_empty() {
                    decoded.set_channel_layout(layout);
                }
                if let Some(pts) = decoded.timestamp() {
                    let pts = ff::Rescale::rescale(&pts, tb, (1, 16000)) - origin * 16 / 1000;
                    if first || (pts - cursor).abs() > 1600 {
                        cursor = pts.max(0);
                    }
                }
                first = false;
                let capacity =
                    (decoded.samples() as u64 * 16000 / decoded.rate() as u64 + 128) as usize;
                let mut output = ff::frame::Audio::new(
                    ff::format::Sample::F32(ff::format::sample::Type::Packed),
                    capacity,
                    ff::ChannelLayout::MONO,
                );
                resampler.run(&decoded, &mut output)?;
                if output.samples() > 0 {
                    let samples = output.plane::<f32>(0)[..output.samples()].to_vec();
                    let pts_us = cursor * 1000 / 16;
                    cursor += samples.len() as i64;
                    accept(
                        PcmAudio {
                            pts_us,
                            sample_rate: 16000,
                            channels: 1,
                            samples,
                        },
                        duration,
                    )?;
                }
            }
            Ok(())
        };
        for (stream, packet) in input.packets() {
            if stream.index() == index {
                decoder.send_packet(&packet)?;
                drain(&mut decoder)?;
            }
        }
        decoder.send_eof()?;
        drain(&mut decoder)?;
    }
    loop {
        let mut out = ff::frame::Audio::new(
            ff::format::Sample::F32(ff::format::sample::Type::Packed),
            4096,
            ff::ChannelLayout::MONO,
        );
        resampler.flush(&mut out)?;
        if out.samples() == 0 {
            break;
        }
        let samples = out.plane::<f32>(0)[..out.samples()].to_vec();
        accept(
            PcmAudio {
                pts_us: cursor * 1000 / 16,
                sample_rate: 16000,
                channels: 1,
                samples: samples.clone(),
            },
            duration,
        )?;
        cursor += samples.len() as i64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_resampler_keeps_tail_across_arbitrary_packets() {
        ff::init().unwrap();
        for packet in [123, 441, 1024] {
            let mut normalizer = Normalizer::new();
            let mut output = Vec::new();
            for start in (0..48000).step_by(packet) {
                let count = packet.min(48000 - start);
                let samples: Vec<_> = (start..start + count)
                    .flat_map(|i| {
                        let x = (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.25;
                        [x, x]
                    })
                    .collect();
                output.extend(
                    normalizer
                        .convert(&PcmAudio {
                            pts_us: start as i64 * 1000 / 48,
                            sample_rate: 48000,
                            channels: 2,
                            samples,
                        })
                        .unwrap(),
                );
            }
            output.extend(normalizer.flush().unwrap());
            assert_eq!(output.len(), 16000, "packet {packet} lost its tail");
            let rms = (output.iter().map(|x| x * x).sum::<f32>() / output.len() as f32).sqrt();
            assert!((rms - 0.1768).abs() < 0.002);
        }
    }
}
