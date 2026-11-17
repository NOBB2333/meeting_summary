//! Offline replay diagnostics. Faults here are deliberately simulated and
//! never claim to measure ASR, a physical device, or a remote AI service.
use crate::{OnlineSpeakerInterval, OnlineSpeakerRegistry, OnlineSpeakerSnapshot};
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use serde::Serialize;
use std::{
    io::{Read, Write},
    path::Path,
};

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticFault {
    None,
    Silence,
    DropPacket,
    SpeakerTimeout,
    MinutesTimeout,
    LateSpeakerResult,
    ExportFailure,
}

fn read_stable_diagnostic_file(
    path: &Path,
    limit: u64,
) -> std::result::Result<Vec<u8>, &'static str> {
    match std::fs::metadata(path) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("not_recorded"),
        Err(_) => return Err("unreadable"),
    }
    let mut file = opencap_core::open_regular_file_for_read(path).map_err(|_| "unreadable")?;
    let before = file.metadata().map_err(|_| "unreadable")?;
    if before.len() > limit {
        return Err("too_large_to_inspect");
    }
    let identity = file
        .try_clone()
        .and_then(same_file::Handle::from_file)
        .map_err(|_| "unreadable")?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unreadable")?;
    if bytes.len() as u64 > limit {
        return Err("too_large_to_inspect");
    }
    let stable = file
        .metadata()
        .ok()
        .zip(opencap_core::open_regular_file_for_read(path).ok())
        .is_some_and(|(after, current)| {
            before.len() == bytes.len() as u64
                && before.len() == after.len()
                && before
                    .modified()
                    .ok()
                    .zip(after.modified().ok())
                    .is_some_and(|(a, b)| a == b)
                && same_file::Handle::from_file(current)
                    .ok()
                    .is_some_and(|current| current == identity)
        });
    if !stable {
        return Err("changed_during_read");
    }
    Ok(bytes)
}

fn saved_recording_status(base: &Path) -> serde_json::Value {
    let path = base.with_extension("recording.json");
    let bytes = match read_stable_diagnostic_file(&path, 64 * 1024 * 1024) {
        Ok(bytes) => bytes,
        Err(state) => return serde_json::json!({"state":state}),
    };
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return serde_json::json!({"state":"unreadable"});
    };
    if !report.is_object() {
        return serde_json::json!({"state":"unreadable"});
    }
    let stats = report.get("stats");
    let capture = report.get("capture_metrics");
    let observers = report
        .get("audio_observers_final")
        .and_then(|value| value.as_array());
    let sanitized = observers.map(|items| items.iter().take(32).map(|item| {
        let consumer = match item.get("consumer").and_then(|value| value.as_str()) {
            Some("asr") => "asr", Some("online_speaker") => "online_speaker", _ => "other_or_unknown",
        };
        let source = match item.get("source").and_then(|value| value.as_str()) {
            Some("mixed_recording") => "mixed_recording", Some("system") => "system",
            Some("microphone") => "microphone", _ => "unknown",
        };
        serde_json::json!({"consumer":consumer, "source":source,
            "queued_packets":item.get("queued_packets").and_then(|value| value.as_u64()),
            "capacity_packets":item.get("capacity_packets").and_then(|value| value.as_u64()),
            "dropped_packets":item.get("dropped_packets").and_then(|value| value.as_u64()),
            "queue_full_packets":item.get("queue_full_packets").and_then(|value| value.as_u64()),
            "receiver_disconnected_packets":item.get("receiver_disconnected_packets").and_then(|value| value.as_u64()),
            "dropped_audio_frames":item.get("dropped_audio_frames").and_then(|value| value.as_u64()),
            "dropped_boundary_events":item.get("dropped_boundary_events").and_then(|value| value.as_u64()),
            "receiver_disconnected":item.get("receiver_disconnected").and_then(|value| value.as_bool())})
    }).collect::<Vec<_>>());
    let fps_valid = report
        .get("frame_rate_valid")
        .and_then(|value| value.as_bool());
    let bitrate_valid = report
        .get("bitrate_valid")
        .and_then(|value| value.as_bool());
    serde_json::json!({"state":"available",
        "duration_seconds":stats.and_then(|value| value.get("duration_seconds")).and_then(|value| value.as_f64()),
        "encoded_frames":stats.and_then(|value| value.get("encoded_frames")).and_then(|value| value.as_u64()),
        "frame_rate_valid":fps_valid,
        "frames_per_second":fps_valid.filter(|valid| *valid).and_then(|_| stats.and_then(|value| value.get("fps"))
            .and_then(|value| value.as_f64()).filter(|value| value.is_finite() && *value >= 0.0)),
        "bitrate_valid":bitrate_valid,
        "bitrate_kbps":bitrate_valid.filter(|valid| *valid).and_then(|_| stats.and_then(|value| value.get("bitrate_kbps"))
            .and_then(|value| value.as_f64()).filter(|value| value.is_finite() && *value >= 0.0)),
        "capture_queue_dropped_frames":stats.and_then(|value| value.get("dropped_frames")).and_then(|value| value.as_u64()),
        "boundary_discarded_audio_chunks":report.get("boundary_discarded_audio_chunks").and_then(|value| value.as_u64()),
        "boundary_discarded_audio_frames":report.get("boundary_discarded_audio_frames").and_then(|value| value.as_u64()),
        "boundary_discard_scope":"recorder queue only; 48 kHz stereo sample frames; excludes capture and route-change drops",
        "warning_count":stats.and_then(|value| value.get("warnings")).and_then(|value| value.as_array()).map(|items| items.len()),
        "readbacks":capture.and_then(|value| value.get("readbacks")).and_then(|value| value.as_u64()),
        "readback_p95_upper_us":capture.and_then(|value| value.get("readback_p95_upper_us")).and_then(|value| value.as_u64()),
        "performance_samples_retained":report.get("performance_samples").and_then(|value| value.as_array()).map(|items| items.len()),
        "performance_samples_discarded":report.get("performance_samples_discarded").and_then(|value| value.as_u64()),
        "final_audio_observers":sanitized, "audio_observers_truncated":observers.is_some_and(|items| items.len() > 32),
        "observer_scope":"final sender snapshot; not consumer completion or recording media loss"})
}

fn saved_run_status(run: &crate::TranscriptionRun) -> serde_json::Value {
    let punctuation = match run.punctuation_effective.as_str() {
        "model_native" => "model_native",
        "ct_transformer" => "ct_transformer",
        "ct_transformer_partial" => "ct_transformer_partial",
        "sentence_end_fallback" => "sentence_end_fallback",
        "disabled" => "disabled",
        _ => "other_or_unknown",
    };
    let revision = run
        .extra
        .get("revision_effective")
        .and_then(|value| value.as_str())
        .map(|value| match value {
            "ready" => "ready",
            "loading" => "loading",
            "failed" => "failed",
            "not_scheduled" => "not_scheduled",
            "disabled" => "disabled",
            _ => "other_or_unknown",
        });
    let timing = run.extra.get("block_processing_timing");
    let stage = |key: &str| {
        timing.and_then(|value| value.get(key)).and_then(|value| {
        Some(serde_json::json!({"count":value.get("count")?.as_u64()?,
            "mean_ms":value.get("mean_ms")?.as_f64()?, "p95_upper_ms":value.get("p95_upper_ms")?.as_f64()?,
            "max_ms":value.get("max_ms")?.as_f64()?}))
    })
    };
    serde_json::json!({"punctuation_requested":run.punctuation_requested, "punctuation_effective":punctuation,
        "language_mode_sent":run.extra.get("language_mode_sent").and_then(|value| value.as_str()).map(|value| match value {
            "zh" => "zh", "en" => "en", "auto" => "auto", _ => "other_or_unknown" }),
        "recognized_language":"not_independently_verified",
        "container_origin_known":run.extra.get("input_audio").and_then(|value| value.get("container_origin_known")).and_then(|value| value.as_bool()),
        "itn_requested":run.extra.get("itn_requested").and_then(|value| value.as_bool()),
        "itn_effectiveness":"not_independently_verified",
        "revision_language_mode_configured":run.extra.get("revision_language_mode_configured").and_then(|value| value.as_str()).map(|value| match value {
            "zh" => "zh", "en" => "en", "auto" => "auto", _ => "other_or_unknown" }),
        "revision_itn_configured":run.extra.get("revision_itn_configured").and_then(|value| value.as_bool()),
        "revision_requested":run.extra.get("revision_requested").and_then(|value| value.as_bool()),
        "revision_scheduled":run.revision_scheduled, "revision_effective":revision,
        "normalization_timing":stage("normalization"), "recognizer_feed_timing":stage("recognizer_feed"),
        "feed_wall_time_over_audio":timing.and_then(|value| value.get("feed_wall_time_over_audio")).and_then(|value| value.as_f64()),
        "timing_scope":"synchronous block wall time; excludes decoding, asynchronous review and GPU completion"})
}

/// Inspect only persisted state. This command has no IPC connection to a live
/// app process and never emits transcript/minutes text, credentials, or paths.
pub fn diagnostic_status(
    config: Option<&Path>,
    meeting: Option<&Path>,
) -> Result<serde_json::Value> {
    let config_path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(opencap_core::config_path);
    let config = match read_stable_diagnostic_file(&config_path, 4 * 1024 * 1024) {
        Ok(bytes) => match serde_json::from_slice::<opencap_core::RecordingConfig>(&bytes) {
            Ok(value) if value.validate().is_ok() => {
                serde_json::json!({"state":"loaded", "realtime_requested":value.realtime_transcription,
                "revision_requested":value.transcription_context_revision, "punctuation_requested":value.transcription_punctuation,
                "online_speakers_requested":value.online_speakers, "system_audio_requested":value.system_audio,
                "microphone_requested":value.microphone, "system_follows_default":value.system_device.is_none(),
                "microphone_follows_default":value.microphone_device.is_none(), "preview_requested":value.show_recording_preview})
            }
            _ => serde_json::json!({"state":"unreadable_or_invalid"}),
        },
        Err("not_recorded") => serde_json::json!({"state":"not_saved"}),
        Err("changed_during_read") => serde_json::json!({"state":"changed_during_read"}),
        Err("too_large_to_inspect") => serde_json::json!({"state":"too_large_to_inspect"}),
        Err(_) => serde_json::json!({"state":"unreadable_or_invalid"}),
    };
    let root = crate::model_root();
    let models = crate::ModelKind::ALL.into_iter().map(|kind| serde_json::json!({
        "id":kind.directory(), "present":kind.ready(&root), "verification":"presence_and_expected_size_only"
    })).collect::<Vec<_>>();
    let meeting = meeting.map(|path| {
        let base = opencap_core::meeting_base(path);
        let manifest = match opencap_core::meeting_manifest(&base) {
            Ok(Some(value)) => serde_json::json!({"state":"available", "assets":value.assets.len(), "runs":value.transcription_runs.len()}),
            Ok(None) => serde_json::json!({"state":"not_recorded"}),
            Err(_) => serde_json::json!({"state":"unreadable"}),
        };
        let transcript = match read_stable_diagnostic_file(&base.with_extension("transcript.json"), 64_000_000) {
            Ok(bytes) => match serde_json::from_slice::<crate::Transcript>(&bytes).ok() {
                Some(value) => {
                    let mut ids = std::collections::HashSet::new();
                    let mut duplicate_ids = 0usize;
                    let mut confirmed = 0usize;
                    let mut empty_text = 0usize;
                    let mut invalid_time = 0usize;
                    let mut token_evidence = 0usize;
                    let mut usable_token_starts = 0usize;
                    let mut alignment_evidence = 0usize;
                    let mut usable_word_alignments = 0usize;
                    for segment in &value.segments {
                        if !ids.insert(segment.id) { duplicate_ids += 1; }
                        if segment.is_confirmed() { confirmed += 1; }
                        if segment.text.trim().is_empty() { empty_text += 1; }
                        if segment.end_ms <= segment.start_ms { invalid_time += 1; }
                        if segment.extra.contains_key("raw_token_timing") {
                            token_evidence += 1;
                            if crate::exact_token_starts(segment, value.run.as_ref()).is_some() {
                                usable_token_starts += 1;
                            }
                        }
                        if segment.extra.contains_key("word_alignment_v1") {
                            alignment_evidence += 1;
                            if crate::validated_word_alignment(segment, value.run.as_ref()).is_some() {
                                usable_word_alignments += 1;
                            }
                        }
                    }
                    let run_state = value.run.as_ref().map(|run| match run.state.as_str() {
                        "running" => "running", "completed" => "completed", "cancelled" => "cancelled", "partial" => "partial", _ => "other"
                    });
                    let input_integrity = value.run.as_ref().and_then(|run| run.extra.get("input_integrity")).and_then(|value| value.as_str()).map(|state| match state {
                        "hash_matched_before_and_after" => "hash_matched_before_and_after",
                        "changed_during_transcription" => "changed_during_transcription",
                        "verification_unavailable_after_transcription" => "verification_unavailable_after_transcription",
                        "not_checked_after_partial_input" => "not_checked_after_partial_input",
                        "fingerprint_unavailable" => "fingerprint_unavailable", _ => "other"
                    });
                    serde_json::json!({"state":"available", "complete":value.complete,
                        "effective_complete":value.is_complete(), "cancelled":value.cancelled,
                        "segments":value.segments.len(), "speaker_turns":value.speaker_turns.len(), "gaps":value.gaps.len(),
                        "confirmed_segments":confirmed, "unconfirmed_segments":value.segments.len().saturating_sub(confirmed),
                        "empty_text_segments":empty_text, "segments_without_playable_time":invalid_time,
                        "duplicate_segment_ids":duplicate_ids,
                        "raw_token_evidence_segments":token_evidence, "mapped_token_start_segments":usable_token_starts,
                        "word_alignment_evidence_segments":alignment_evidence, "validated_word_alignment_segments":usable_word_alignments,
                        "timing_scope":"text and saved source evidence only; current media bytes not checked",
                        "output_warnings":value.output_warnings.len(), "run_state":run_state, "input_integrity":input_integrity,
                        "run_diagnostics":value.run.as_ref().map(saved_run_status)})
                },
                None => serde_json::json!({"state":"unreadable"}),
            },
            Err(state) => serde_json::json!({"state":state}),
        };
        let notes = match crate::LiveNotes::load(&base) {
            Ok(Some(value)) => serde_json::json!({"state":"available", "revision":value.revision,
                "items":value.items.len(), "locked":value.items.iter().filter(|item| item.user_locked).count(),
                "needs_review":value.items.iter().filter(|item| item.needs_review).count(), "covered_segments":value.coverage.len()}),
            Ok(None) => serde_json::json!({"state":"not_recorded"}),
            Err(_) => serde_json::json!({"state":"unreadable"}),
        };
        let minutes = match crate::MinutesHistory::load(&base) {
            Ok(value) => serde_json::json!({"state":"available", "versions":value.versions.len(), "exports_pending":value.exports_pending()}),
            Err(_) => serde_json::json!({"state":"unreadable"}),
        };
        let speakers = match OnlineSpeakerRegistry::load(&base) {
            Ok(Some(value)) => serde_json::json!({
                "state":"available", "finished":value.finished, "backend_final":value.backend_final,
                "has_incomplete_reason":value.incomplete_reason.is_some(),
                "run_bound":value.run_id.is_some(), "input_end_ms":value.input_end_ms,
                "identities":value.identities.len(), "intervals":value.interval_count(),
                "revision":value.revision
            }),
            Ok(None) => serde_json::json!({"state":"not_recorded"}),
            Err(_) => serde_json::json!({"state":"unreadable"}),
        };
        serde_json::json!({"manifest":manifest, "recording":saved_recording_status(&base), "transcript":transcript, "online_speakers":speakers, "live_notes":notes, "minutes":minutes})
    });
    Ok(
        serde_json::json!({"schema_version":1,"scope":"saved files only; not a live process, hardware, or model-quality check",
        "profile":if opencap_core::is_dev_profile() {"development"} else {"production"},
        "live_process":"not_connected", "config":config, "models":models,
        "punctuation_present":crate::punctuation_ready(&root), "meeting":meeting}),
    )
}

/// Decode a chosen file without writing to it. A report path must not already
/// exist. No speech text or audio samples are included in the report.
pub fn diagnostic_replay(input: &Path, report: &Path, fault: DiagnosticFault) -> Result<()> {
    ensure!(input.is_file(), "输入媒体不可访问");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .context("无法新建诊断报告；不会覆盖已有文件")?;
    let result = (|| -> Result<()> {
        let mut packets = 0u64;
        let mut samples = 0u64;
        let mut squared = 0f64;
        let mut peak = 0f32;
        let mut previous_end = None::<i64>;
        let mut previous_start = None::<i64>;
        let mut out_of_order_packets = 0u64;
        let mut gaps = Vec::<(u64, u64)>::new();
        let mut gap_count = 0u64;
        let mut injected = false;
        let mut end_ms = 0u64;
        crate::audio::decode_file(input, |packet, _| {
            ensure!(
                packet.sample_rate > 0 && packet.channels > 0 && packet.pts_us >= 0,
                "音频块格式无效"
            );
            let frames = packet.samples.len() / usize::from(packet.channels);
            let end = packet.pts_us.saturating_add(
                (frames as i64).saturating_mul(1_000_000) / i64::from(packet.sample_rate),
            );
            if previous_start.is_some_and(|start| packet.pts_us < start) {
                out_of_order_packets = out_of_order_packets.saturating_add(1);
            }
            previous_start = Some(packet.pts_us);
            if let Some(previous) = previous_end
                && packet.pts_us > previous.saturating_add(1_000)
            {
                gap_count = gap_count.saturating_add(1);
                if gaps.len() < 4096 {
                    gaps.push((previous.max(0) as u64 / 1000, packet.pts_us as u64 / 1000));
                }
            }
            previous_end = Some(previous_end.map_or(end, |previous| previous.max(end)));
            end_ms = end_ms.max(end.max(0) as u64 / 1000);
            packets = packets.saturating_add(1);
            if matches!(fault, DiagnosticFault::DropPacket)
                && !injected
                && (packet.pts_us >= 500_000 || packets == 3)
            {
                gap_count = gap_count.saturating_add(1);
                if gaps.len() < 4096 {
                    gaps.push((packet.pts_us as u64 / 1000, end.max(0) as u64 / 1000));
                }
                injected = true;
                return Ok(());
            }
            for sample in packet.samples.iter().copied() {
                let sample = if matches!(fault, DiagnosticFault::Silence) || !sample.is_finite() {
                    0.
                } else {
                    sample.clamp(-1., 1.)
                };
                squared += f64::from(sample).powi(2);
                peak = peak.max(sample.abs());
                samples = samples.saturating_add(1);
            }
            Ok(())
        })?;
        if matches!(fault, DiagnosticFault::Silence) {
            injected = packets > 0;
        }
        let mut simulation = serde_json::Value::Null;
        if matches!(
            fault,
            DiagnosticFault::LateSpeakerResult | DiagnosticFault::SpeakerTimeout
        ) {
            let mut registry = OnlineSpeakerRegistry::new(2, None)?;
            let snapshot = |revision, finished| OnlineSpeakerSnapshot {
                revision,
                input_end_ms: 1000,
                finished,
                intervals: vec![OnlineSpeakerInterval {
                    start_ms: 0,
                    end_ms: 1000,
                    slot: 1,
                    extra: Default::default(),
                }],
            };
            registry.apply_snapshot(snapshot(2, false))?;
            registry.rename(1, "用户指定")?;
            if matches!(fault, DiagnosticFault::LateSpeakerResult) {
                let stale_accepted = registry.apply_snapshot(snapshot(1, false))?;
                registry.apply_snapshot(snapshot(3, true))?;
                simulation = serde_json::json!({"scope":"synthetic_speaker_state", "stale_accepted":stale_accepted,
                    "manual_name_preserved":registry.identities.get(&1).and_then(|value| value.name.as_deref()) == Some("用户指定")});
            } else {
                registry.incomplete_reason = Some("模拟超时".into());
                simulation = serde_json::json!({"scope":"synthetic_speaker_state", "finished":registry.finished,
                    "prior_intervals_retained":!registry.turns().is_empty(), "reason":registry.incomplete_reason});
            }
            injected = true;
        }
        if matches!(fault, DiagnosticFault::MinutesTimeout) {
            let meeting_id = opencap_core::new_stable_id();
            let run_id = opencap_core::new_stable_id();
            let mut notes = crate::LiveNotes::new(meeting_id.clone(), run_id.clone())?;
            notes.items.push(crate::LiveNoteItem {
                item_id: opencap_core::new_stable_id(),
                kind: crate::LiveNoteKind::Point,
                text: "模拟的既有正文".into(),
                status: "保留".into(),
                owner: None,
                due: None,
                evidence: Vec::new(),
                user_locked: true,
                needs_review: false,
                extra: Default::default(),
            });
            let transcript = crate::Transcript {
                run: Some(crate::TranscriptionRun {
                    run_id,
                    started_unix_ms: 0,
                    finished_unix_ms: None,
                    application_version: "diagnostic".into(),
                    backend: "synthetic".into(),
                    runtime_version: "none".into(),
                    model_id: "none".into(),
                    model_sha256: None,
                    provider: "none".into(),
                    threads: 0,
                    requested_language: "unknown".into(),
                    effective_language: None,
                    requested_hotwords: String::new(),
                    accepted_hotwords: None,
                    punctuation_requested: false,
                    punctuation_effective: "none".into(),
                    revision_scheduled: false,
                    revision_model: None,
                    input_kind: "synthetic".into(),
                    state: "running".into(),
                    meeting_id: Some(meeting_id),
                    input_asset_id: None,
                    extra: Default::default(),
                }),
                segments: vec![crate::Segment {
                    id: 1,
                    revision: 1,
                    start_ms: 0,
                    end_ms: 1000,
                    text: "模拟来源".into(),
                    finalized: true,
                    correcting: false,
                    review: None,
                    extra: Default::default(),
                }],
                ..Default::default()
            };
            let before = serde_json::to_vec(&notes.items)?;
            let request = notes
                .begin(&transcript, false)?
                .context("无法创建模拟纪要请求")?;
            notes.abandon(&request.request_id);
            let preserved = before == serde_json::to_vec(&notes.items)?;
            let retry_possible = notes.begin(&transcript, false)?.is_some();
            simulation = serde_json::json!({"scope":"synthetic_minutes_state", "prior_body_preserved":preserved,
                "retry_possible":retry_possible, "remote_service_called":false});
            injected = true;
        }
        if matches!(fault, DiagnosticFault::ExportFailure) {
            let parent = report
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let work = parent.join(format!(
                ".opencap-diagnostic-{}",
                opencap_core::new_stable_id()
            ));
            std::fs::create_dir(&work)?;
            let check = (|| -> Result<serde_json::Value> {
                let base = work.join("sample");
                // A directory at the intended Word path forces a real export
                // error without modifying or locking any user document.
                std::fs::create_dir(base.with_extension("summary.docx"))?;
                let history = crate::MinutesHistory::default().save_revision(
                    &base,
                    "# 模拟正文\n",
                    "诊断",
                )?;
                let saved = crate::MinutesHistory::load(&base)?;
                Ok(
                    serde_json::json!({"scope":"isolated_export_failure", "body_preserved":saved.latest() == "# 模拟正文\n",
                    "retry_pending":history.exports_pending(), "remote_service_called":false}),
                )
            })();
            let cleanup = std::fs::remove_dir_all(&work);
            simulation = check?;
            cleanup.context("无法清理本次诊断的隔离目录")?;
            injected = true;
        }
        let gaps_truncated = gap_count > gaps.len() as u64;
        let report = serde_json::json!({"schema_version":1,"scope":"offline decode and explicitly simulated faults; not model, playback or hardware acceptance",
            "fault":fault,"injection_applied":injected,"packets":packets,"sample_count":samples,"media_end_ms":end_ms,
            "rms":if samples == 0 {0.} else {(squared / samples as f64).sqrt()},"peak":peak,"gaps_ms":gaps,
            "gap_count":gap_count,"gaps_truncated":gaps_truncated,
            "gap_scope":"observed packet-order discontinuities and simulated drops; later packets can overlap or fill them",
            "out_of_order_packets":out_of_order_packets,"simulation":simulation});
        file.write_all(&serde_json::to_vec_pretty(&report)?)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(report);
    }
    result
}
