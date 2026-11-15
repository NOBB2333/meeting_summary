use crate::{
    SpeakerAnalysisVersion, SpeakerTurn, Transcript, audio::decode_file_with_expected_stream,
};
use anyhow::{Context, Result};
use sherpa_onnx::{OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
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
    transcript: Transcript,
    root: &Path,
    output: &Path,
    num_speakers: u32,
    threshold: f32,
    threads: usize,
    cancelled: &AtomicBool,
    status: impl FnMut(String),
) -> Result<Transcript> {
    distinguish_speakers_inner(
        transcript,
        root,
        output,
        num_speakers,
        threshold,
        threads,
        cancelled,
        None,
        status,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn distinguish_speakers_guarded(
    transcript: Transcript,
    root: &Path,
    output: &Path,
    num_speakers: u32,
    threshold: f32,
    threads: usize,
    cancelled: &AtomicBool,
    expected_sha256: &str,
    status: impl FnMut(String),
) -> Result<Transcript> {
    distinguish_speakers_inner(
        transcript,
        root,
        output,
        num_speakers,
        threshold,
        threads,
        cancelled,
        Some(expected_sha256),
        status,
    )
}

#[allow(clippy::too_many_arguments)]
fn distinguish_speakers_inner(
    mut transcript: Transcript,
    root: &Path,
    output: &Path,
    num_speakers: u32,
    threshold: f32,
    threads: usize,
    cancelled: &AtomicBool,
    expected_sha256: Option<&str>,
    mut status: impl FnMut(String),
) -> Result<Transcript> {
    let _meeting_activity = opencap_core::managed_meeting_activity(output)?;
    let began = Instant::now();
    anyhow::ensure!(transcript.is_complete(), "请先完整完成转录");
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
    let run = transcript.run.as_ref();
    let recorded_fingerprint = run
        .and_then(|run| run.extra.get("input_media_fingerprint"))
        .cloned()
        .map(serde_json::from_value::<opencap_core::MediaFingerprint>)
        .transpose()
        .context("识别运行的媒体指纹无效")?;
    let stream_value = run
        .and_then(|run| run.extra.get("input_audio"))
        .and_then(|audio| audio.get("audio_stream_index"));
    let expected_stream = stream_value
        .map(|value| value.as_u64().context("识别运行的音轨编号无效"))
        .transpose()?;
    let historical_verified = run
        .and_then(|run| run.extra.get("input_integrity"))
        .and_then(|value| value.as_str())
        == Some("hash_matched_before_and_after");
    status("正在核对说话人分析输入媒体…".into());
    let before = opencap_core::fingerprint_media(source, Some(cancelled))?;
    if let Some(expected) = &recorded_fingerprint {
        anyhow::ensure!(
            &before == expected,
            "当前媒体与识别运行指纹不一致，未开始说话人分析"
        );
    }
    let model = crate::speaker_models::prepare_speakers_cancellable(root, cancelled, &mut status)?;
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消");
    let mut config = OfflineSpeakerDiarizationConfig::default();
    config.segmentation.pyannote.model =
        Some(model.join("model.int8.onnx").to_string_lossy().into());
    config.embedding.model = Some(
        model
            .join(crate::speaker_models::EMBEDDING)
            .to_string_lossy()
            .into(),
    );
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
    let mut gap_count = 0u64;
    let mut gap_samples = 0u64;
    let mut gaps = Vec::<(u64, u64)>::new();
    let decoded = decode_file_with_expected_stream(source, expected_stream, |packet, _| {
        anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消");
        anyhow::ensure!(
            packet.sample_rate == 16000 && packet.channels == 1,
            "说话人分析音频格式不匹配"
        );
        let timestamp = u128::try_from(packet.pts_us).context("音频时间戳为负数")?;
        // The decoder emits microseconds rounded down from a 16 kHz sample
        // position. Round back to the nearest sample instead of flooring twice.
        let offset = usize::try_from((timestamp * 16000 + 500_000) / 1_000_000)
            .context("媒体时间戳超出平台范围")?;
        anyhow::ensure!(
            offset >= samples.len(),
            "音频时间轴发生回退或重叠，无法可靠关联说话人；已有稿件保留"
        );
        let end = offset
            .checked_add(packet.samples.len())
            .context("媒体时长溢出")?;
        anyhow::ensure!(
            end <= 16000 * 7200,
            "当前说话人分析支持最长 2 小时，请分段导入"
        );
        if offset > samples.len() {
            gap_count = gap_count.saturating_add(1);
            gap_samples = gap_samples.saturating_add((offset - samples.len()) as u64);
            if gaps.len() < 1024 {
                gaps.push((samples.len() as u64, offset as u64));
            }
        }
        if samples.len() < end {
            samples.resize(end, 0.0);
        }
        samples[offset..end].copy_from_slice(&packet.samples);
        Ok(())
    })?;
    status("正在复核说话人分析输入媒体…".into());
    anyhow::ensure!(
        opencap_core::fingerprint_media(source, Some(cancelled))? == before,
        "媒体在说话人分析读取期间发生变化，未应用结果"
    );
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
    let mut new_turns: Vec<SpeakerTurn> = result
        .sort_by_start_time()
        .into_iter()
        .filter(|s| s.start.is_finite() && s.end.is_finite() && s.end > s.start && s.speaker >= 0)
        .map(|s| {
            let next = labels.len() as u32 + 1;
            let speaker = *labels.entry(s.speaker).or_insert(next);
            SpeakerTurn {
                extra: Default::default(),
                start_ms: (s.start.max(0.) * 1000.) as u64,
                end_ms: (s.end * 1000.) as u64,
                speaker,
            }
        })
        .collect();
    anyhow::ensure!(
        !new_turns.is_empty(),
        "未检测到可区分的说话人；请检查音轨和声音大小"
    );
    drop(result);
    drop(diarizer);
    let prior_versions = transcript.speaker_history.len();
    let prior_speakers: BTreeSet<u32> = transcript
        .speaker_turns
        .iter()
        .map(|turn| turn.speaker)
        .collect();
    preserve_and_map_speakers(&mut transcript, &mut new_turns);
    let refined_speakers: BTreeSet<u32> = new_turns.iter().map(|turn| turn.speaker).collect();
    let reused_speakers = prior_speakers.intersection(&refined_speakers).count();
    transcript.speaker_turns = new_turns;
    let unmatched_names = (transcript.speaker_history.len() > prior_versions)
        .then(|| transcript.speaker_history.last())
        .flatten()
        .map_or(0, |previous| {
            previous
                .names
                .len()
                .saturating_sub(transcript.speaker_names.len())
        });
    let preserved_names = transcript.speaker_names.len();
    status("提取独立发言声纹并匹配本机已保存姓名…".into());
    transcript.speaker_memory_status = match crate::speaker_memory::extract_and_match(
        &mut transcript,
        &samples,
        root,
        threads,
        cancelled,
    ) {
        Ok(()) => format!(
            "保留 {preserved_names} 个旧姓名；本次声纹自动匹配 {} 个姓名，未匹配者保留本场编号",
            transcript
                .speaker_names
                .len()
                .saturating_sub(preserved_names)
        ),
        Err(e) => format!("分析已保留，声纹记忆暂不可用：{e:#}"),
    };
    if !prior_speakers.is_empty() {
        transcript.speaker_memory_status.push_str(&format!(
            "；与上一版对照：{} 组变为 {} 组，按时间重叠复用 {reused_speakers} 个编号；编号复用不代表身份已确认",
            prior_speakers.len(), refined_speakers.len()
        ));
    }
    if unmatched_names > 0 {
        transcript.speaker_memory_status.push_str(&format!(
            "；{unmatched_names} 个旧姓名未可靠映射，已保留在历史版本"
        ));
    }
    if recorded_fingerprint.is_none() || expected_stream.is_none() || !historical_verified {
        transcript
            .speaker_memory_status
            .push_str("；历史识别媒体或音轨未完整核实，说话人归属需人工核对");
    }
    if gap_count > 0 {
        transcript.speaker_memory_status.push_str(&format!(
            "；输入有 {gap_count} 处时间缺口（约 {:.2} 秒），缺口填充静音，附近说话人归属需人工核对",
            gap_samples as f64 / 16000.0
        ));
    }
    transcript.extra.insert(
        "speaker_analysis_source_v1".into(),
        serde_json::json!({
            "media_fingerprint": before,
            "audio_stream_index": decoded.audio_stream_index,
            "recorded_fingerprint_matched": recorded_fingerprint.as_ref().map(|_| true),
            "recorded_audio_stream_matched": expected_stream.map(|_| true),
            "recognition_input_integrity_confirmed": historical_verified,
            "previous_speaker_count": (!prior_speakers.is_empty()).then_some(prior_speakers.len()),
            "refined_speaker_count": refined_speakers.len(),
            "reused_previous_speaker_ids": reused_speakers,
            "gap_count": gap_count,
            "gap_samples": gap_samples,
            "gap_sample_ranges": gaps,
            "gap_ranges_truncated": gap_count > 1024,
            "scope": "file hash before and after decoding; not a continuously locked source"
        }),
    );
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消；已有结果保留");
    transcript.speaker_analysis_seconds = Some(began.elapsed().as_secs_f64());
    if let Some(expected) = expected_sha256 {
        if let Err(error) = transcript.sync_outputs_guarded(output, expected) {
            transcript.primary_save_pending = true;
            transcript
                .output_warnings
                .push(format!("逐字稿并发核对失败：{error:#}"));
        }
    } else {
        transcript.sync_outputs(output);
    }
    Ok(transcript)
}

/// Preserve every previous version and reuse an old ID only when temporal
/// evidence is unambiguous and one-to-one. Ambiguous names stay in history.
fn preserve_and_map_speakers(transcript: &mut Transcript, new_turns: &mut [SpeakerTurn]) {
    let old_turns = std::mem::take(&mut transcript.speaker_turns);
    let old_names = std::mem::take(&mut transcript.speaker_names);
    let old_prints = std::mem::take(&mut transcript.speaker_voiceprints);
    if old_turns.is_empty() && old_names.is_empty() && old_prints.is_empty() {
        return;
    }
    transcript.speaker_history.push(SpeakerAnalysisVersion {
        version_id: opencap_core::new_stable_id(),
        created_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        turns: old_turns.clone(),
        names: old_names.clone(),
        voiceprints: old_prints.clone(),
        status: transcript.speaker_memory_status.clone(),
        extra: Default::default(),
    });
    // Duplicate or overlapping turns for the same speaker must not count the
    // same evidence twice. Merge each speaker's intervals before scoring;
    // overlaps between different speakers remain separate ambiguous evidence.
    fn merged_turns(turns: &[SpeakerTurn]) -> Vec<SpeakerTurn> {
        let mut grouped = BTreeMap::<u32, Vec<(u64, u64)>>::new();
        for turn in turns.iter().filter(|turn| turn.end_ms > turn.start_ms) {
            grouped
                .entry(turn.speaker)
                .or_default()
                .push((turn.start_ms, turn.end_ms));
        }
        let mut merged = Vec::new();
        for (speaker, mut ranges) in grouped {
            ranges.sort_unstable();
            let mut current: Option<(u64, u64)> = None;
            for (start, end) in ranges {
                match current {
                    Some((first, last)) if start <= last => current = Some((first, last.max(end))),
                    Some((first, last)) => {
                        merged.push(SpeakerTurn {
                            extra: Default::default(),
                            start_ms: first,
                            end_ms: last,
                            speaker,
                        });
                        current = Some((start, end));
                    }
                    None => current = Some((start, end)),
                }
            }
            if let Some((start_ms, end_ms)) = current {
                merged.push(SpeakerTurn {
                    extra: Default::default(),
                    start_ms,
                    end_ms,
                    speaker,
                });
            }
        }
        merged.sort_by_key(|turn| turn.start_ms);
        merged
    }
    let ordered_old = merged_turns(&old_turns);
    let ordered_new = merged_turns(new_turns);
    let mut overlaps: BTreeMap<u32, BTreeMap<u32, u64>> = BTreeMap::new();
    let mut durations: BTreeMap<u32, u64> = BTreeMap::new();
    let mut old_durations = BTreeMap::<u32, u64>::new();
    for turn in &ordered_old {
        let total = old_durations.entry(turn.speaker).or_default();
        *total = total.saturating_add(turn.end_ms - turn.start_ms);
    }
    let mut active = Vec::<&SpeakerTurn>::new();
    let mut cursor = 0usize;
    for turn in &ordered_new {
        let duration = durations.entry(turn.speaker).or_default();
        *duration = duration.saturating_add(turn.end_ms.saturating_sub(turn.start_ms));
        active.retain(|old| old.end_ms > turn.start_ms);
        while cursor < ordered_old.len() && ordered_old[cursor].start_ms < turn.end_ms {
            active.push(&ordered_old[cursor]);
            cursor += 1;
        }
        for old in &active {
            let overlap = turn
                .end_ms
                .min(old.end_ms)
                .saturating_sub(turn.start_ms.max(old.start_ms));
            if overlap > 0 {
                let total = overlaps
                    .entry(turn.speaker)
                    .or_default()
                    .entry(old.speaker)
                    .or_default();
                *total = total.saturating_add(overlap);
            }
        }
    }
    let mut proposed = BTreeMap::new();
    for (new_id, scores) in overlaps {
        let mut scores: Vec<_> = scores.into_iter().collect();
        scores.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));
        let Some(&(old_id, best)) = scores.first() else {
            continue;
        };
        let total = durations.get(&new_id).copied().unwrap_or(0);
        let old_total = old_durations.get(&old_id).copied().unwrap_or(0);
        let second = scores.get(1).map_or(0, |(_, value)| *value);
        if total >= 1_500
            && best.saturating_mul(4) >= total.saturating_mul(3)
            // A short excerpt should not inherit the identity of a much
            // longer old speaker merely because it falls inside that turn.
            && old_total > 0 && best.saturating_mul(2) >= old_total
            && best.saturating_sub(second).saturating_mul(5) >= total
        {
            proposed.insert(new_id, old_id);
        }
    }
    let mut mapping = BTreeMap::new();
    for (&new_id, &old_id) in &proposed {
        if proposed
            .values()
            .filter(|candidate| **candidate == old_id)
            .count()
            == 1
        {
            mapping.insert(new_id, old_id);
            if let Some(name) = old_names.get(&old_id) {
                transcript.speaker_names.insert(old_id, name.clone());
            }
            if let Some(print) = old_prints.get(&old_id) {
                transcript.speaker_voiceprints.insert(old_id, print.clone());
            }
        }
    }
    let mut used_ids: BTreeSet<u32> = old_turns
        .iter()
        .map(|t| t.speaker)
        .chain(old_names.keys().copied())
        .chain(old_prints.keys().copied())
        .collect();
    let mut next = 1u32;
    for turn in new_turns {
        let mapped = mapping.entry(turn.speaker).or_insert_with(|| {
            while used_ids.contains(&next) {
                next = next.checked_add(1).expect("speaker ID space exhausted");
            }
            let id = next;
            used_ids.insert(id);
            id
        });
        turn.speaker = *mapped;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn turn_change_does_not_attribute_whole_utterance_to_one_person() {
        let t = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 1,
                },
                SpeakerTurn {
                    extra: Default::default(),
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: 2,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.speaker_label(0, 800), "说话人 1");
        assert!(t.speaker_label(500, 1500).contains("多人"));
        assert_eq!(t.speaker_label(2200, 3000), "说话人未确定");
    }
}
