use crate::{SpeakerTurn, Transcript, audio::decode_file, models::download};
use anyhow::{Context, Result};
use sherpa_onnx::{OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const SEGMENTATION: &str = "sherpa-onnx-pyannote-segmentation-3-0";
pub(crate) const EMBEDDING: &str = "3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx";
pub fn speakers_ready(root: &Path) -> bool {
    [
        root.join("diarization/model.int8.onnx"),
        root.join("diarization").join(EMBEDDING),
    ]
    .iter()
    .all(|p| p.metadata().is_ok_and(|m| m.len() > 0))
}
pub fn prepare_speakers(root: &Path, mut status: impl FnMut(String)) -> Result<PathBuf> {
    if speakers_ready(root) {
        return Ok(root.join("diarization"));
    }
    std::fs::create_dir_all(root)?;
    let staging = root.join(format!(
        ".speakers-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir(&staging)?;
    let result = (|| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()?;
        status("下载说话人分段模型…".into());
        let response = download(
            &client,
            &format!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/{SEGMENTATION}.tar.bz2"
            ),
        )?;
        let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(response));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path()?.into_owned();
            if path.parent() != Some(Path::new(SEGMENTATION)) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if ["model.int8.onnx", "LICENSE", "README.md"].contains(&name) {
                entry.unpack(staging.join(name))?;
            }
        }
        status("下载中文说话人声纹模型…".into());
        let mut response = download(
            &client,
            &format!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/{EMBEDDING}"
            ),
        )?;
        let mut file = std::fs::File::create(staging.join(EMBEDDING))?;
        std::io::copy(&mut response, &mut file)?;
        file.sync_all()?;
        anyhow::ensure!(
            staging.join("model.int8.onnx").metadata()?.len() > 0
                && staging.join(EMBEDDING).metadata()?.len() > 0,
            "说话人模型不完整"
        );
        let dest = root.join("diarization");
        if speakers_ready(root) {
            return Ok(dest);
        }
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::rename(&staging, &dest)?;
        Ok(dest)
    })();
    let _ = std::fs::remove_dir_all(staging);
    result
}
/// Optional, post-record CPU pass. Speaker labels are local to one meeting.
pub fn distinguish_speakers(
    transcript: Transcript,
    root: &Path,
    output: &Path,
    num_speakers: u32,
    threshold: f32,
    cancelled: &AtomicBool,
    status: impl FnMut(String),
) -> Result<Transcript> {
    distinguish_speakers_with_threads(
        transcript,
        root,
        output,
        num_speakers,
        threshold,
        2,
        cancelled,
        status,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn distinguish_speakers_with_threads(
    mut transcript: Transcript,
    root: &Path,
    output: &Path,
    num_speakers: u32,
    threshold: f32,
    threads: usize,
    cancelled: &AtomicBool,
    mut status: impl FnMut(String),
) -> Result<Transcript> {
    let began = Instant::now();
    anyhow::ensure!(transcript.complete, "请先完整完成转录");
    anyhow::ensure!(num_speakers <= 20, "人数应为 0（自动）或 1–20");
    anyhow::ensure!(
        threshold.is_finite() && (0.1..=1.0).contains(&threshold),
        "聚类阈值应为 0.1–1.0"
    );
    let source = transcript
        .source
        .as_ref()
        .context("转录稿没有原媒体路径，请重新导入原录音")?;
    anyhow::ensure!(source.is_file(), "原录音不在原路径，请重新导入音视频");
    let model = prepare_speakers(root, &mut status)?;
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消");
    let mut config = OfflineSpeakerDiarizationConfig::default();
    config.segmentation.pyannote.model =
        Some(model.join("model.int8.onnx").to_string_lossy().into());
    config.embedding.model = Some(model.join(EMBEDDING).to_string_lossy().into());
    let threads = threads
        .clamp(1, 4)
        .min(std::thread::available_parallelism().map_or(1, usize::from));
    config.segmentation.num_threads = threads as i32;
    config.embedding.num_threads = threads as i32;
    config.clustering.num_clusters = if num_speakers == 0 {
        -1
    } else {
        num_speakers as i32
    };
    config.clustering.threshold = threshold;
    let diarizer = OfflineSpeakerDiarization::create(&config).context("说话人模型加载失败")?;
    anyhow::ensure!(diarizer.sample_rate() == 16000, "说话人模型采样率不匹配");
    status("读取原音频；最长支持 2 小时，长会议会增加内存使用…".into());
    let mut samples = Vec::<f32>::new();
    decode_file(source, |packet, _| {
        anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消");
        let offset = packet.pts_us.max(0) as usize * 16000 / 1_000_000;
        let end = offset
            .checked_add(packet.samples.len())
            .context("媒体时长溢出")?;
        anyhow::ensure!(
            end <= 16000 * 7200,
            "当前说话人分析支持最长 2 小时，请分段导入"
        );
        if samples.len() < end {
            samples.resize(end, 0.0);
        }
        samples[offset..end].copy_from_slice(&packet.samples);
        Ok(())
    })?;
    anyhow::ensure!(!samples.is_empty(), "音频没有采样");
    status(format!(
        "本机 CPU 正在区分说话人 · {threads} 线程 · 音频 {:.1} 分钟；当前模型不提供总进度",
        samples.len() as f64 / 16000. / 60.
    ));
    let result = diarizer.process(&samples).context("说话人分析失败")?;
    anyhow::ensure!(
        !cancelled.load(Ordering::Relaxed),
        "已取消；原转录与纪要保留"
    );
    let mut labels = HashMap::new();
    transcript.speaker_turns = result
        .sort_by_start_time()
        .into_iter()
        .filter(|s| s.start.is_finite() && s.end.is_finite() && s.end > s.start && s.speaker >= 0)
        .map(|s| {
            let next = labels.len() as u32 + 1;
            let speaker = *labels.entry(s.speaker).or_insert(next);
            SpeakerTurn {
                start_ms: (s.start.max(0.) * 1000.) as u64,
                end_ms: (s.end * 1000.) as u64,
                speaker,
            }
        })
        .collect();
    anyhow::ensure!(
        !transcript.speaker_turns.is_empty(),
        "未检测到可区分的说话人；请检查音轨和声音大小"
    );
    drop(result);
    drop(diarizer);
    transcript.speaker_names.clear();
    transcript.speaker_voiceprints.clear();
    status("提取独立发言声纹并匹配本机已保存姓名…".into());
    transcript.speaker_memory_status = match crate::speaker_memory::extract_and_match(
        &mut transcript,
        &samples,
        root,
        threads,
        cancelled,
    ) {
        Ok(()) => format!(
            "已匹配 {} 个姓名；未匹配者保留本场编号",
            transcript.speaker_names.len()
        ),
        Err(e) => format!("分析已保留，声纹记忆暂不可用：{e:#}"),
    };
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消；已有结果保留");
    transcript.speaker_analysis_seconds = Some(began.elapsed().as_secs_f64());
    transcript.sync_outputs(output);
    Ok(transcript)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn turn_change_does_not_attribute_whole_utterance_to_one_person() {
        let t = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 1,
                },
                SpeakerTurn {
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: 2,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.speaker_label(0, 800), "说话人 1");
        assert!(t.speaker_label(500, 1500).contains("交替或重叠"));
        assert_eq!(t.speaker_label(2200, 3000), "说话人未确定");
    }
}
