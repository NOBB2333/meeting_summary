//! Optional model-reported word times for a saved file transcription.
//!
//! This is a second recognition pass, not a forced aligner. A segment is
//! eligible only when its exact displayed text is fully covered by the
//! backend's actual word results within that segment's existing time range.
use crate::{
    AlignedWord, NemoStreamingAsr, StreamingAsrBackend, StreamingTextEvent, Transcript,
    WordAlignment,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn check_cancel(cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Relaxed), "词时间任务已取消");
    Ok(())
}

fn separator(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_ascii_punctuation()
        || matches!(
            ch,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '〈'
                | '〉'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
                | '·'
        )
}

fn meaningful(text: &str) -> String {
    text.chars().filter(|ch| !separator(*ch)).collect()
}

fn collect_final_words(
    events: Vec<StreamingTextEvent>,
    output: &mut Vec<crate::StreamingWordTiming>,
) -> Result<()> {
    for event in events {
        if !event.finalized || event.kind != "snapshot" {
            continue;
        }
        ensure!(event.timing_error.is_none(), "模型返回的词时间无效");
        let word_text = event
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<String>();
        ensure!(
            meaningful(&event.text) == meaningful(&word_text),
            "模型词列表没有完整覆盖最终识别文字"
        );
        ensure!(
            output.len().saturating_add(event.words.len()) <= 200_000,
            "模型词数超过限制"
        );
        for word in event.words {
            ensure!(
                word.method == "nemo_decoder_emission_frames_not_forced_alignment"
                    && !word.text.trim().is_empty()
                    && word.end_ms >= word.start_ms
                    && output
                        .last()
                        .is_none_or(|previous| previous.start_ms <= word.start_ms),
                "模型返回的词或时间顺序无效"
            );
            output.push(word);
        }
    }
    Ok(())
}

/// Generate model-reported word times without changing the transcript.
///
/// # Safety
/// `library` must be a trusted native library implementing the pinned NeMo C ABI.
pub unsafe fn generate_nemo_word_timings(
    base: &Path,
    transcript: &Transcript,
    library: &Path,
    model: &Path,
    cancelled: &AtomicBool,
) -> Result<Vec<WordAlignment>> {
    ensure!(
        transcript.is_complete(),
        "只有已完成的文件逐字稿可生成词时间"
    );
    let run = transcript.run.as_ref().context("逐字稿缺少识别运行记录")?;
    ensure!(
        !run.run_id.trim().is_empty()
            && run
                .extra
                .get("input_integrity")
                .and_then(|value| value.as_str())
                == Some("hash_matched_before_and_after"),
        "原识别没有完整的媒体校验记录"
    );
    let meeting_id = run.meeting_id.as_deref().context("识别运行缺少会议身份")?;
    let asset_id = run
        .input_asset_id
        .as_deref()
        .context("识别运行缺少媒体资产身份")?;
    let expected_fingerprint: opencap_core::MediaFingerprint = serde_json::from_value(
        run.extra
            .get("input_media_fingerprint")
            .cloned()
            .context("识别运行缺少媒体指纹")?,
    )?;
    let expected_audio = run
        .extra
        .get("input_audio")
        .cloned()
        .context("识别运行缺少音轨信息")?;
    let stream = expected_audio
        .get("audio_stream_index")
        .and_then(|value| value.as_u64())
        .context("识别运行缺少音轨编号")?;
    let media = transcript.source.as_deref().context("原媒体不可用")?;
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let verify_asset = || -> Result<()> {
        let manifest = opencap_core::meeting_manifest(base)?.context("会议清单不可用")?;
        ensure!(manifest.meeting_id == meeting_id, "会议身份已变化");
        let mut assets = manifest
            .assets
            .iter()
            .filter(|asset| asset.asset_id == asset_id);
        ensure!(
            assets
                .next()
                .is_some_and(opencap_core::is_input_media_asset)
                && assets.next().is_none(),
            "原媒体资产不存在或不唯一"
        );
        ensure!(
            opencap_core::resolve_asset_in_manifest(base, &manifest, asset_id)?.as_deref()
                == Some(media),
            "原媒体资产路径已变化"
        );
        Ok(())
    };
    check_cancel(cancelled)?;
    verify_asset()?;
    ensure!(
        opencap_core::fingerprint_media(media, Some(cancelled))? == expected_fingerprint,
        "原媒体与识别运行指纹不一致"
    );
    let model_fingerprint = opencap_core::fingerprint_media(model, Some(cancelled))?;
    ensure!(
        model_fingerprint.size_bytes == crate::NEMOTRON_ASR_BYTES
            && model_fingerprint.sha256 == crate::NEMOTRON_ASR_SHA256,
        "词时间模型不匹配固定版本"
    );
    let library_fingerprint = opencap_core::fingerprint_media(library, Some(cancelled))?;
    check_cancel(cancelled)?;
    let mut backend = unsafe { NemoStreamingAsr::open(library, model)? };
    ensure!(
        backend.library_sha256 == library_fingerprint.sha256,
        "加载前原生库已变化"
    );
    check_cancel(cancelled)?;
    let mut words = Vec::new();
    let decoded =
        crate::audio::decode_file_with_expected_stream(media, Some(stream), |audio, _| {
            check_cancel(cancelled)?;
            collect_final_words(backend.push(&audio)?, &mut words)
        })?;
    check_cancel(cancelled)?;
    collect_final_words(backend.finish()?, &mut words)?;
    ensure!(
        serde_json::to_value(decoded)? == expected_audio,
        "原媒体音轨或时间映射与识别时不同"
    );
    ensure!(!words.is_empty(), "模型没有返回可用词时间");
    check_cancel(cancelled)?;
    ensure!(
        opencap_core::fingerprint_media(media, Some(cancelled))? == expected_fingerprint,
        "词时间生成期间原媒体发生变化"
    );
    ensure!(
        opencap_core::fingerprint_media(model, Some(cancelled))? == model_fingerprint,
        "词时间生成期间模型发生变化"
    );
    ensure!(
        opencap_core::fingerprint_media(library, Some(cancelled))? == library_fingerprint,
        "词时间生成期间原生库发生变化"
    );
    verify_asset()?;

    map_word_timings(
        transcript,
        &words,
        &expected_fingerprint,
        stream,
        asset_id,
        &model_fingerprint.sha256,
        &library_fingerprint.sha256,
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn map_word_timings(
    transcript: &Transcript,
    words: &[crate::StreamingWordTiming],
    fingerprint: &opencap_core::MediaFingerprint,
    stream: u64,
    asset_id: &str,
    model_sha256: &str,
    runtime_sha256: &str,
    cancelled: &AtomicBool,
) -> Result<Vec<WordAlignment>> {
    let run = transcript.run.as_ref().context("逐字稿缺少识别运行记录")?;
    let mut ids = HashSet::new();
    ensure!(
        transcript
            .segments
            .iter()
            .all(|segment| ids.insert(segment.id)),
        "逐字稿存在重复句段编号"
    );
    let mut segments: Vec<_> = transcript
        .segments
        .iter()
        .filter(|segment| segment.is_confirmed() && segment.end_ms > segment.start_ms)
        .collect();
    segments.sort_by_key(|segment| (segment.start_ms, segment.end_ms, segment.id));
    let mut max_prior_end = 0u64;
    let alignment_run_id = opencap_core::new_stable_id();
    let mut result = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        check_cancel(cancelled)?;
        let overlaps_prior = index > 0 && segment.start_ms < max_prior_end;
        max_prior_end = max_prior_end.max(segment.end_ms);
        let overlaps_next = segments
            .get(index + 1)
            .is_some_and(|next| next.start_ms < segment.end_ms);
        if overlaps_prior
            || overlaps_next
            || transcript
                .gaps
                .iter()
                .any(|(start, end)| *start < segment.end_ms && *end > segment.start_ms)
        {
            continue;
        }
        let first = words.partition_point(|word| word.start_ms < segment.start_ms);
        if first > 0 && words[first - 1].end_ms > segment.start_ms {
            continue;
        }
        let end = words.partition_point(|word| word.start_ms < segment.end_ms);
        if first == end || end - first > 10_000 {
            continue;
        }
        let mut cursor = 0usize;
        let mut mapped = Vec::new();
        for word in &words[first..end] {
            if word.end_ms <= word.start_ms || word.end_ms > segment.end_ms {
                mapped.clear();
                break;
            }
            let remaining = &segment.text[cursor..];
            cursor += remaining
                .char_indices()
                .find(|(_, ch)| !separator(*ch))
                .map(|(offset, _)| offset)
                .unwrap_or(remaining.len());
            let text = word.text.trim();
            if !segment.text[cursor..].starts_with(text) {
                mapped.clear();
                break;
            }
            let start_byte = cursor;
            cursor += text.len();
            mapped.push(AlignedWord {
                start_byte,
                end_byte: cursor,
                start_ms: word.start_ms,
                end_ms: word.end_ms,
            });
        }
        if mapped.is_empty() || !segment.text[cursor..].chars().all(separator) {
            continue;
        }
        let alignment = WordAlignment {
            source_run_id: run.run_id.clone(),
            source_media_fingerprint: Some(fingerprint.clone()),
            source_audio_stream_index: Some(stream),
            source_asset_id: Some(asset_id.to_owned()),
            segment_id: segment.id,
            segment_start_ms: segment.start_ms,
            segment_end_ms: segment.end_ms,
            text_revision: segment.revision,
            text_sha256: format!("{:x}", Sha256::digest(segment.text.as_bytes())),
            alignment_run_id: alignment_run_id.clone(),
            model_id: "Nemotron 3.5 ASR Streaming 0.6B".into(),
            method: "asr_word_timing".into(),
            timing_detail: Some("decoder_emission_frame_80ms_not_forced_alignment".into()),
            model_sha256: Some(model_sha256.to_owned()),
            runtime_sha256: Some(runtime_sha256.to_owned()),
            words: mapped,
        };
        let mut candidate = (**segment).clone();
        candidate.extra.insert(
            "word_alignment_v1".into(),
            serde_json::to_value(&alignment)?,
        );
        if crate::validated_word_alignment(&candidate, Some(run)).is_some() {
            result.push(alignment);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Transcript, opencap_core::MediaFingerprint) {
        let fingerprint = opencap_core::MediaFingerprint {
            sha256: "a".repeat(64),
            size_bytes: 123,
        };
        let run: crate::TranscriptionRun = serde_json::from_value(serde_json::json!({
            "run_id":"run", "started_unix_ms":1, "application_version":"dev",
            "backend":"local", "runtime_version":"1", "model_id":"original",
            "provider":"cpu", "threads":1, "requested_language":"zh", "requested_hotwords":"",
            "punctuation_requested":true, "punctuation_effective":"model", "revision_scheduled":false,
            "input_kind":"file", "state":"finished", "meeting_id":"meeting", "input_asset_id":"asset",
            "input_integrity":"hash_matched_before_and_after", "input_audio":{"audio_stream_index":3},
            "input_media_fingerprint":fingerprint
        })).unwrap();
        (
            Transcript {
                complete: true,
                run: Some(run),
                segments: vec![crate::Segment {
                    id: 1,
                    revision: 4,
                    start_ms: 0,
                    end_ms: 2_000,
                    text: "预算，确认。".into(),
                    finalized: true,
                    correcting: false,
                    review: None,
                    extra: Default::default(),
                }],
                ..Default::default()
            },
            fingerprint,
        )
    }

    fn word(text: &str, start_ms: u64, end_ms: u64) -> crate::StreamingWordTiming {
        crate::StreamingWordTiming {
            text: text.into(),
            start_ms,
            end_ms,
            method: "nemo_decoder_emission_frames_not_forced_alignment".into(),
        }
    }

    #[test]
    fn model_words_preserve_reported_times_and_stop_applying_after_text_edits() {
        let (mut transcript, fingerprint) = sample();
        let words = [word("预算", 100, 500), word("确认", 700, 1_100)];
        let result = map_word_timings(
            &transcript,
            &words,
            &fingerprint,
            3,
            "asset",
            "model",
            "runtime",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        let alignment = &result[0];
        assert_eq!(
            (alignment.words[0].start_ms, alignment.words[0].end_ms),
            (100, 500)
        );
        assert_eq!(
            (alignment.words[1].start_ms, alignment.words[1].end_ms),
            (700, 1_100)
        );
        assert_eq!(
            &transcript.segments[0].text
                [alignment.words[0].start_byte..alignment.words[0].end_byte],
            "预算"
        );
        assert_eq!(
            &transcript.segments[0].text
                [alignment.words[1].start_byte..alignment.words[1].end_byte],
            "确认"
        );
        transcript.segments[0].extra.insert(
            "word_alignment_v1".into(),
            serde_json::to_value(alignment).unwrap(),
        );
        assert!(
            crate::validated_word_alignment(&transcript.segments[0], transcript.run.as_ref())
                .is_some()
        );
        transcript.segments[0].text = "预算，未确认。".into();
        assert!(
            crate::validated_word_alignment(&transcript.segments[0], transcript.run.as_ref())
                .is_none()
        );
    }

    #[test]
    fn incomplete_zero_length_and_overlapping_words_do_not_gain_timing() {
        let (mut transcript, fingerprint) = sample();
        let cancel = AtomicBool::new(false);
        let map = |transcript: &Transcript, words: &[crate::StreamingWordTiming]| {
            map_word_timings(
                transcript,
                words,
                &fingerprint,
                3,
                "asset",
                "model",
                "runtime",
                &cancel,
            )
            .unwrap()
        };
        assert!(map(&transcript, &[word("预算", 100, 500)]).is_empty());
        assert!(
            map(
                &transcript,
                &[word("预算", 100, 500), word("确认", 700, 700)]
            )
            .is_empty()
        );
        let mut overlapping = transcript.segments[0].clone();
        overlapping.id = 2;
        overlapping.start_ms = 900;
        overlapping.end_ms = 3_000;
        transcript.segments.push(overlapping);
        assert!(
            map(
                &transcript,
                &[word("预算", 100, 500), word("确认", 700, 1_100)]
            )
            .is_empty()
        );
    }
}
