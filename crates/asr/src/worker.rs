use crate::{
    ModelKind, ReviewState, Segment, SentenceReview, Transcript, TranscriptionRun,
    audio::{Normalizer, SpeechHistory, decode_file_with_expected_stream},
    prepare_cancellable,
    punctuation::{PartialPunctuation, Punctuator},
    revision::{MAX_REVISION_SAMPLES, Reviser},
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::{PcmAudio, TranscriptionLanguage};
use sherpa_onnx::*;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Fixed-memory wall-time observations for the synchronous per-block path.
/// Bucket limits and maximum are retained so that p95 is an upper bound.
#[derive(Default)]
struct BlockTiming {
    buckets: [u64; 15],
    count: u64,
    total_us: u128,
    max_us: u64,
}

/// Media-time estimate for a contiguous normalized stream. Resampler output
/// belongs to the stream, not necessarily to the input packet that caused it
/// to be emitted. Reset only after the old resampler tail has been drained.
#[derive(Default)]
struct NormalizedClock {
    origin_us: Option<i64>,
    emitted_samples: u64,
}
impl NormalizedClock {
    fn begin(&mut self, input_pts_us: i64) {
        self.origin_us.get_or_insert(input_pts_us.max(0));
    }
    fn next_ms(&self, fallback_us: i64) -> i64 {
        let offset =
            (u128::from(self.emitted_samples) * 1_000_000 / 16_000).min(i64::MAX as u128) as i64;
        self.origin_us
            .unwrap_or(fallback_us.max(0))
            .saturating_add(offset)
            / 1000
    }
    fn advance(&mut self, samples: usize) {
        self.emitted_samples = self.emitted_samples.saturating_add(samples as u64);
    }
}
impl BlockTiming {
    fn record(&mut self, elapsed: Duration) {
        const LIMITS: [u64; 15] = [
            100,
            250,
            500,
            1_000,
            2_000,
            4_000,
            8_000,
            16_000,
            33_000,
            66_000,
            125_000,
            250_000,
            500_000,
            1_000_000,
            u64::MAX,
        ];
        let us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        let index = LIMITS
            .partition_point(|limit| *limit < us)
            .min(LIMITS.len() - 1);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.total_us = self.total_us.saturating_add(u128::from(us));
        self.max_us = self.max_us.max(us);
    }
    fn report(&self) -> serde_json::Value {
        const LIMITS: [u64; 15] = [
            100,
            250,
            500,
            1_000,
            2_000,
            4_000,
            8_000,
            16_000,
            33_000,
            66_000,
            125_000,
            250_000,
            500_000,
            1_000_000,
            u64::MAX,
        ];
        if self.count == 0 {
            return serde_json::Value::Null;
        }
        let target = self.count.saturating_mul(95).saturating_add(99) / 100;
        let mut accumulated = 0u64;
        let mut p95 = self.max_us;
        for (limit, count) in LIMITS.iter().zip(self.buckets) {
            accumulated = accumulated.saturating_add(count);
            if accumulated >= target {
                p95 = (*limit).min(self.max_us);
                break;
            }
        }
        serde_json::json!({"count":self.count, "mean_ms":self.total_us as f64 / self.count as f64 / 1000.,
            "p95_upper_ms":p95 as f64 / 1000., "max_ms":self.max_us as f64 / 1000.})
    }
}

/// Preserve actual backend token starts as raw evidence only. Punctuation and
/// later edits can change the display text; this is never a word alignment.
fn raw_token_evidence(
    tokens: &[String],
    timestamps: Option<&[f32]>,
    durations: Option<&[f32]>,
    origin_ms: u64,
    end_ms: u64,
    raw_text: &str,
    segment_revision: u64,
) -> Option<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let timestamps = timestamps?;
    if tokens.is_empty()
        || tokens.len() != timestamps.len()
        || tokens.len() > 100_000
        || raw_text.len() > 1_000_000
        || tokens
            .iter()
            .try_fold(0usize, |total, token| total.checked_add(token.len()))
            .is_none_or(|total| total > 1_000_000)
    {
        return None;
    }
    let mut previous = 0u64;
    let mut output = Vec::with_capacity(tokens.len());
    // The offline API may report durations for some models. Preserve these
    // as backend observations only; their token correspondence and time
    // semantics still need evaluation for each model. Bad durations must not
    // discard otherwise usable token starts.
    let valid_durations = durations.filter(|values| {
        values.len() == tokens.len()
            && values
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
    });
    for (index, (token, seconds)) in tokens.iter().zip(timestamps).enumerate() {
        if !seconds.is_finite() || *seconds < 0. || token.len() > 4096 {
            return None;
        }
        let start = origin_ms.saturating_add((*seconds as f64 * 1000.) as u64);
        if (index > 0 && start < previous) || start > end_ms.saturating_add(1000) {
            return None;
        }
        previous = start;
        let duration = valid_durations.map(|values| values[index]);
        output.push(serde_json::json!({ "token": token, "start_ms": start,
            "backend_duration_raw": duration }));
    }
    Some(
        serde_json::json!({ "timing_precision": "token_model", "timing_method": "sherpa_backend_token_start",
        "raw_text": raw_text, "raw_text_sha256": format!("{:x}", Sha256::digest(raw_text.as_bytes())),
        "recognizer_segment_revision": segment_revision, "display_mapping": null,
        "duration_status": if durations.is_none() { "not_returned" }
            else if valid_durations.is_some() { "unverified_backend_values" }
            else { "invalid_or_count_mismatch" },
        "duration_unit": "unverified",
        "tokens": output }),
    )
}

/// Bind raw backend starts to the exact segment and recognition run that
/// produced them. A later edit may retain the raw evidence for inspection,
/// but it must not make the old timestamps usable for a different timeline.
fn bind_token_evidence(segment: &mut Segment, run: Option<&crate::TranscriptionRun>) {
    let Some(run) = run.filter(|run| !run.run_id.is_empty()) else {
        return;
    };
    let Some(evidence) = segment
        .extra
        .get_mut("raw_token_timing")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    evidence.insert("source_run_id".into(), run.run_id.clone().into());
    evidence.insert("source_segment_id".into(), segment.id.into());
    evidence.insert("source_segment_start_ms".into(), segment.start_ms.into());
    evidence.insert("source_segment_end_ms".into(), segment.end_ms.into());
}

/// Retain independently reported durations even when the backend omitted or
/// returned unusable starts. They remain unmapped and cannot drive playback.
fn raw_duration_evidence(
    tokens: &[String],
    durations: Option<&[f32]>,
    raw_text: &str,
) -> Option<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let durations = durations?;
    if tokens.is_empty()
        || tokens.len() > 100_000
        || tokens.len() != durations.len()
        || raw_text.len() > 1_000_000
        || tokens.iter().any(|token| token.len() > 4096)
        || tokens
            .iter()
            .try_fold(0usize, |total, token| total.checked_add(token.len()))
            .is_none_or(|total| total > 1_000_000)
        || durations
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    Some(serde_json::json!({
        "timing_precision": "token_model", "timing_method": "sherpa_backend_duration_only",
        "duration_status": "unverified_backend_values", "duration_unit": "unverified",
        "raw_text": raw_text, "raw_text_sha256": format!("{:x}", Sha256::digest(raw_text.as_bytes())),
        "recognizer_segment_revision": 1, "display_mapping": null,
        "tokens": tokens.iter().zip(durations).map(|(token, duration)|
            serde_json::json!({"token": token, "backend_duration_raw": duration})).collect::<Vec<_>>()
    }))
}

/// Resolve only explicitly registered managed assets; every other source is
/// external and must never be moved as part of a meeting rename.
fn attach_run_identity(
    transcript: &mut Transcript,
    output: &Path,
    final_outputs: bool,
) -> Result<()> {
    use opencap_core::{
        AssetOwnership, meeting_base, meeting_manifest, register_meeting_asset_checked,
        register_meeting_run_with_input,
    };
    let base = meeting_base(output);
    let Some(manifest) = meeting_manifest(&base)? else {
        return Ok(());
    };
    let Some(run) = transcript.run.as_mut() else {
        return Ok(());
    };
    anyhow::ensure!(
        run.meeting_id
            .as_deref()
            .is_none_or(|id| id == manifest.meeting_id.as_str()),
        "识别运行与当前会议身份不一致"
    );
    let mut input_asset_id = run.input_asset_id.clone();
    if let Some(asset_id) = input_asset_id.as_deref() {
        // A path can be offline or deliberately relinked. The stable asset
        // identity must not silently change merely because its path did.
        let mut matches = manifest
            .assets
            .iter()
            .filter(|asset| asset.asset_id == asset_id);
        anyhow::ensure!(
            matches
                .next()
                .is_some_and(opencap_core::is_input_media_asset)
                && matches.next().is_none(),
            "识别运行引用的媒体资产不存在、角色无效或编号重复，不能自动更换身份"
        );
    } else if let Some(source) = &transcript.source {
        let folder = base.parent().unwrap_or(Path::new("."));
        let normalized_source = source
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .zip(source.file_name())
            .map(|(parent, name)| parent.join(name));
        let mut managed_matches = manifest.assets.iter().filter(|asset| {
            asset.ownership == AssetOwnership::Managed
                && normalized_source
                    .as_ref()
                    .is_some_and(|path| folder.join(&asset.path) == *path)
        });
        let managed = managed_matches.next();
        anyhow::ensure!(
            managed_matches.next().is_none(),
            "媒体路径对应多个受管理资产，无法确定身份"
        );
        input_asset_id = if let Some(asset) = managed {
            anyhow::ensure!(
                opencap_core::is_input_media_asset(asset)
                    && !asset.asset_id.is_empty()
                    && manifest
                        .assets
                        .iter()
                        .filter(|other| other.asset_id == asset.asset_id)
                        .take(2)
                        .count()
                        == 1,
                "媒体路径对应的资产角色无效或编号不唯一"
            );
            Some(asset.asset_id.clone())
        } else {
            let absolute = source.canonicalize().unwrap_or_else(|_| source.clone());
            if absolute.is_absolute() {
                register_meeting_asset_checked(
                    &base,
                    "source_media",
                    AssetOwnership::External,
                    &absolute,
                    Some(&manifest.meeting_id),
                )?
            } else {
                None
            }
        };
    }
    if let Some(id) = input_asset_id.as_deref() {
        let current =
            meeting_manifest(&base)?.ok_or_else(|| anyhow::anyhow!("会议清单已不可用"))?;
        anyhow::ensure!(manifest.meeting_id == current.meeting_id, "会议身份已变化");
        let mut matches = current.assets.iter().filter(|asset| asset.asset_id == id);
        anyhow::ensure!(
            matches
                .next()
                .is_some_and(opencap_core::is_input_media_asset)
                && matches.next().is_none(),
            "媒体资产角色无效或编号不唯一，未登记识别运行"
        );
    }
    register_meeting_run_with_input(
        &base,
        &run.run_id,
        Some(&manifest.meeting_id),
        input_asset_id.as_deref(),
    )?;
    let extensions = if final_outputs {
        &[
            "transcript.json",
            "transcript.txt",
            "transcript.md",
            "transcript.docx",
            "srt",
        ][..]
    } else {
        &["transcript.json"][..]
    };
    for extension in extensions {
        let path = base.with_extension(extension);
        if let Some(name) = path.file_name() {
            register_meeting_asset_checked(
                &base,
                extension,
                AssetOwnership::Managed,
                Path::new(name),
                Some(&manifest.meeting_id),
            )?;
        }
    }
    run.meeting_id = Some(manifest.meeting_id);
    run.input_asset_id = input_asset_id;
    Ok(())
}

/// Retry the same meeting/run and asset registration used at completion.
pub fn reconcile_transcript_identity(transcript: &mut Transcript, base: &Path) -> Result<()> {
    anyhow::ensure!(
        opencap_core::meeting_manifest(base)?.is_some(),
        "会议资产清单不可用，无法核对身份"
    );
    anyhow::ensure!(transcript.run.is_some(), "识别运行记录缺失，无法登记身份");
    attach_run_identity(transcript, base, true)?;
    anyhow::ensure!(
        transcript.source.is_none()
            || transcript
                .run
                .as_ref()
                .is_some_and(|run| run.input_asset_id.is_some()),
        "源媒体未能登记，无法确认会议身份"
    );
    Ok(())
}

/// Revalidate recorded bytes before attaching an earlier input hash to an asset.
pub fn reconcile_asset_fingerprint(transcript: &mut Transcript, base: &Path) -> Result<()> {
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let run = transcript
        .run
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("识别运行记录缺失"))?;
    let manifest = opencap_core::meeting_manifest(base)?
        .ok_or_else(|| anyhow::anyhow!("会议资产清单不可用"))?;
    anyhow::ensure!(
        run.meeting_id.as_deref() == Some(manifest.meeting_id.as_str()),
        "识别运行与当前会议身份不一致，未更新资产指纹"
    );
    anyhow::ensure!(
        run.extra
            .get("input_integrity")
            .and_then(|value| value.as_str())
            == Some("hash_matched_before_and_after"),
        "原识别运行未记录完整的输入指纹复核"
    );
    let expected: opencap_core::MediaFingerprint = serde_json::from_value(
        run.extra
            .get("input_media_fingerprint")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("识别运行未记录媒体指纹"))?,
    )?;
    let asset_id = run
        .input_asset_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("源媒体资产尚未登记"))?;
    let source = opencap_core::resolve_meeting_asset(base, &asset_id)?
        .ok_or_else(|| anyhow::anyhow!("源媒体资产不存在"))?;
    let observed = opencap_core::fingerprint_media(&source, None)?;
    anyhow::ensure!(
        observed == expected,
        "当前媒体与识别时指纹不一致，未更新资产"
    );
    opencap_core::record_asset_fingerprint(
        base,
        &manifest.meeting_id,
        &asset_id,
        &source,
        &expected,
    )?;
    if let Some(run) = &mut transcript.run {
        run.extra.remove("asset_fingerprint_warning");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum AsrEvent {
    Status(String),
    RevisionStatus(RevisionRuntimeState, Option<String>),
    Started(TranscriptionRun),
    Segment(Segment),
    Progress(f64),
    /// End of audio actually consumed by the live recognizer, including silence.
    Processed(u64),
    /// A file-speaker observer fell behind; recognition continues independently.
    SpeakerInputGap(u64, u64),
    /// File decoder has released its speaker input; ASR may still be finishing.
    FileSpeakerInputEnded {
        end_ms: Option<u64>,
        complete: bool,
    },
    Gap(u64, u64),
    Finished(Box<Transcript>),
    Error(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionRuntimeState {
    Loading,
    Ready,
    Failed,
}
impl RevisionRuntimeState {
    pub fn code(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}
#[derive(Default)]
struct MeetingActivity {
    lease: Option<std::fs::File>,
    ended: bool,
}

#[derive(Clone)]
struct HistoricalRunSpec {
    run: TranscriptionRun,
    kind: ModelKind,
    revision_model: ModelKind,
    language: TranscriptionLanguage,
    model_hash: String,
    revision_hash: Option<String>,
    punctuation_hash: Option<String>,
    media: Option<opencap_core::MediaFingerprint>,
    audio_stream_index: Option<u64>,
}

impl HistoricalRunSpec {
    fn parse(run: &TranscriptionRun) -> Result<Self> {
        anyhow::ensure!(
            run.backend == "sherpa-onnx" && run.runtime_version == "1.13.8",
            "旧识别后端或运行时与当前版本不同，不能按原配置重转"
        );
        anyhow::ensure!(
            run.application_version == env!("CARGO_PKG_VERSION"),
            "旧运行的应用版本不同，端点和预处理配置无法确认"
        );
        anyhow::ensure!(
            run.provider == "cpu" && run.threads == 1,
            "旧运行的设备或线程设置无法复现"
        );
        let kind = ModelKind::ALL
            .into_iter()
            .find(|kind| kind.directory() == run.model_id)
            .context("旧识别模型已不受支持")?;
        let language = match run.requested_language.as_str() {
            "zh" => TranscriptionLanguage::Chinese,
            "en" => TranscriptionLanguage::English,
            "auto" => TranscriptionLanguage::Auto,
            _ => anyhow::bail!("旧运行未记录可识别的语言设置"),
        };
        if let Some(recorded_mode) = run.extra.get("language_mode_sent") {
            anyhow::ensure!(
                kind == ModelKind::Offline && recorded_mode.as_str() == Some(language.code()),
                "旧运行的语言模式记录与模型或请求不一致，无法保证原配置"
            );
        }
        if let Some(itn) = run.extra.get("itn_requested") {
            anyhow::ensure!(
                kind == ModelKind::Offline && itn.as_bool() == Some(true),
                "旧运行的文本规范化设置与当前模型配置不一致，无法保证原配置"
            );
        }
        let get_hash = |key: &str| {
            run.extra
                .get(key)
                .and_then(|value| value.get("manifest_sha256"))
                .and_then(|value| value.as_str())
                .filter(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .map(str::to_owned)
        };
        let model_hash =
            get_hash("model_fingerprint").context("旧运行没有完整模型指纹，无法核对原配置")?;
        anyhow::ensure!(
            run.extra
                .get("model_fingerprint_verification")
                .and_then(|value| value.as_str())
                == Some("before_after_load_matched"),
            "旧运行未确认主模型加载前后的指纹，无法保证原配置"
        );
        let (revision_model, revision_hash) = if run.revision_scheduled {
            anyhow::ensure!(kind.is_streaming(), "旧运行的复核配置不完整");
            anyhow::ensure!(
                run.extra
                    .get("revision_effective")
                    .and_then(|value| value.as_str())
                    == Some("ready"),
                "旧运行未记录复核模型实际就绪，无法确认原配置"
            );
            anyhow::ensure!(
                run.extra
                    .get("revision_fingerprint_policy")
                    .and_then(|value| value.as_str())
                    == Some("verify_before_after_load"),
                "旧运行未记录复核模型加载时核对，无法保证原配置"
            );
            let revision = ModelKind::ALL
                .into_iter()
                .find(|kind| {
                    Some(kind.directory()) == run.revision_model.as_deref() && !kind.is_streaming()
                })
                .context("旧复核模型已不受支持")?;
            (
                revision,
                Some(get_hash("revision_model_fingerprint").context("旧运行没有复核模型指纹")?),
            )
        } else {
            (ModelKind::Offline, None)
        };
        if let Some(mode) = run.extra.get("revision_language_mode_configured") {
            anyhow::ensure!(
                run.revision_scheduled
                    && revision_model == ModelKind::Offline
                    && mode.as_str() == Some(language.code()),
                "旧运行的复核语言配置不一致"
            );
        }
        if let Some(itn) = run.extra.get("revision_itn_configured") {
            anyhow::ensure!(
                run.revision_scheduled
                    && revision_model == ModelKind::Offline
                    && itn.as_bool() == Some(true),
                "旧运行的复核文本规范化配置不一致"
            );
        }
        if kind.is_streaming() {
            anyhow::ensure!(
                run.accepted_hotwords.is_some(),
                "旧运行未记录实际接受的热词"
            );
        }
        let punctuation_hash = match run.punctuation_effective.as_str() {
            "model_native" if kind == ModelKind::Offline => None,
            "disabled" if !run.punctuation_requested && kind != ModelKind::Offline => None,
            "sentence_end_fallback" if run.punctuation_requested && kind != ModelKind::Offline => {
                None
            }
            "ct_transformer" if run.punctuation_requested && kind != ModelKind::Offline => {
                anyhow::ensure!(
                    run.extra
                        .get("punctuation_fingerprint_verification")
                        .and_then(|value| value.as_str())
                        == Some("before_after_load_matched"),
                    "旧运行未确认标点模型加载前后的指纹"
                );
                Some(
                    run.extra
                        .get("punctuation_fingerprint")
                        .and_then(|v| v.get("sha256"))
                        .and_then(|v| v.as_str())
                        .filter(|hash| {
                            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                        .context("旧运行没有标点模型指纹")?
                        .to_owned(),
                )
            }
            _ => anyhow::bail!("旧运行的标点配置无法确认"),
        };
        let media = run
            .extra
            .get("input_media_fingerprint")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .context("旧运行的媒体指纹格式无效")?;
        let audio_stream_index = run
            .extra
            .get("input_audio")
            .and_then(|audio| audio.get("audio_stream_index"))
            .map(|value| value.as_u64().context("旧运行的音轨编号格式无效"))
            .transpose()?;
        Ok(Self {
            run: run.clone(),
            kind,
            revision_model,
            language,
            model_hash,
            revision_hash,
            punctuation_hash,
            media,
            audio_stream_index,
        })
    }
}

/// Fast structural validation. Content hashes are checked in the worker, so
/// the UI remains responsive and missing old models are never downloaded.
pub fn validate_historical_run(run: &TranscriptionRun) -> Result<()> {
    HistoricalRunSpec::parse(run).map(|_| ())
}

/// Check all recorded artifacts before creating a new meeting. The worker
/// repeats these checks because external files can change after this preflight.
pub fn verify_historical_inputs(
    root: &Path,
    source: &Path,
    run: &TranscriptionRun,
    cancelled: &AtomicBool,
) -> Result<()> {
    let spec = HistoricalRunSpec::parse(run)?;
    anyhow::ensure!(source.is_file(), "历史媒体不可访问，请先重新定位");
    if let Some(expected) = spec.audio_stream_index {
        crate::audio::select_audio_stream_index(source, Some(expected))?;
    }
    anyhow::ensure!(
        spec.kind.ready(root),
        "历史识别模型未安装；没有自动换用其他模型"
    );
    let model = crate::models::fingerprint_model(spec.kind, root, cancelled, false)?;
    anyhow::ensure!(
        model.manifest_sha256 == spec.model_hash,
        "当前识别模型与历史指纹不一致"
    );
    if let Some(expected) = &spec.revision_hash {
        anyhow::ensure!(spec.revision_model.ready(root), "历史句段复核模型未安装");
        let model = crate::models::fingerprint_model(spec.revision_model, root, cancelled, false)?;
        anyhow::ensure!(
            &model.manifest_sha256 == expected,
            "当前复核模型与历史指纹不一致"
        );
    }
    if let Some(expected) = &spec.punctuation_hash {
        anyhow::ensure!(
            crate::punctuation::punctuation_ready(root),
            "历史标点模型未安装"
        );
        let model = opencap_core::fingerprint_media(
            &crate::punctuation::punctuation_model_path(root),
            Some(cancelled),
        )?;
        anyhow::ensure!(&model.sha256 == expected, "当前标点模型与历史指纹不一致");
    }
    if let Some(expected) = &spec.media {
        anyhow::ensure!(
            &opencap_core::fingerprint_media(source, Some(cancelled))? == expected,
            "所选媒体与历史运行的指纹不一致"
        );
    }
    crate::models::ensure_download_active(cancelled)?;
    Ok(())
}
pub struct AsrSession {
    pub events: Receiver<AsrEvent>,
    input: Option<Sender<PcmAudio>>,
    output: Arc<Mutex<Option<PathBuf>>>,
    activity: Arc<Mutex<MeetingActivity>>,
    cancelled: Arc<AtomicBool>,
    expected_end: Arc<AtomicU64>,
    worker: JoinHandle<()>,
}
impl AsrSession {
    pub fn live(root: PathBuf) -> Result<Self> {
        Self::live_with_hotwords(root, crate::DEFAULT_HOTWORDS.into())
    }
    pub fn live_with_hotwords(root: PathBuf, hotwords: String) -> Result<Self> {
        Self::live_with_revision(root, hotwords, true)
    }
    pub fn live_with_revision(root: PathBuf, hotwords: String, revision: bool) -> Result<Self> {
        Self::spawn(
            root,
            ModelKind::Streaming,
            None,
            None,
            false,
            hotwords,
            revision,
            revision,
            ModelKind::Offline,
            TranscriptionLanguage::default(),
            true,
            None,
            None,
        )
    }
    /// The caller selects only installed models; sentence mode uses the same VAD
    /// engine as file transcription, with live PCM instead of a source file.
    pub fn live_with_plan(
        root: PathBuf,
        hotwords: String,
        plan: crate::LiveModelPlan,
    ) -> Result<Self> {
        Self::live_with_plan_language(root, hotwords, plan, TranscriptionLanguage::default())
    }
    pub fn live_with_plan_language(
        root: PathBuf,
        hotwords: String,
        plan: crate::LiveModelPlan,
        language: TranscriptionLanguage,
    ) -> Result<Self> {
        Self::live_with_plan_options(root, hotwords, plan, language, true)
    }
    pub fn live_with_plan_options(
        root: PathBuf,
        hotwords: String,
        plan: crate::LiveModelPlan,
        language: TranscriptionLanguage,
        punctuation: bool,
    ) -> Result<Self> {
        Self::live_with_plan_request(
            root,
            hotwords,
            plan,
            language,
            punctuation,
            plan.context_revision,
        )
    }
    pub fn live_with_plan_request(
        root: PathBuf,
        hotwords: String,
        plan: crate::LiveModelPlan,
        language: TranscriptionLanguage,
        punctuation: bool,
        revision_requested: bool,
    ) -> Result<Self> {
        Self::spawn(
            root,
            plan.kind,
            None,
            None,
            false,
            hotwords,
            plan.context_revision,
            revision_requested,
            plan.revision_model,
            language,
            punctuation,
            None,
            None,
        )
    }
    pub fn file(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
    ) -> Result<Self> {
        Self::file_with_hotwords(
            root,
            kind,
            source,
            output,
            realtime,
            crate::DEFAULT_HOTWORDS.into(),
        )
    }
    pub fn file_with_hotwords(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
    ) -> Result<Self> {
        Self::file_with_revision(root, kind, source, output, realtime, hotwords, true)
    }
    pub fn file_with_revision(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
    ) -> Result<Self> {
        Self::file_with_language(
            root,
            kind,
            source,
            output,
            realtime,
            hotwords,
            revision,
            TranscriptionLanguage::default(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn file_with_language(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
        language: TranscriptionLanguage,
    ) -> Result<Self> {
        Self::file_with_models(
            root,
            kind,
            source,
            output,
            realtime,
            hotwords,
            revision,
            ModelKind::Offline,
            language,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn file_with_models(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
        revision_model: ModelKind,
        language: TranscriptionLanguage,
    ) -> Result<Self> {
        Self::file_with_options(
            root,
            kind,
            source,
            output,
            realtime,
            hotwords,
            revision,
            revision_model,
            language,
            true,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn file_with_options(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
        revision_model: ModelKind,
        language: TranscriptionLanguage,
        punctuation: bool,
    ) -> Result<Self> {
        Self::file_with_options_observed(
            root,
            kind,
            source,
            output,
            realtime,
            hotwords,
            revision,
            revision_model,
            language,
            punctuation,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn file_with_options_observed(
        root: PathBuf,
        kind: ModelKind,
        source: PathBuf,
        output: PathBuf,
        realtime: bool,
        hotwords: String,
        revision: bool,
        revision_model: ModelKind,
        language: TranscriptionLanguage,
        punctuation: bool,
        observer: Option<Sender<PcmAudio>>,
    ) -> Result<Self> {
        anyhow::ensure!(
            observer
                .as_ref()
                .is_none_or(|sender| sender.capacity().is_some()),
            "音频订阅必须使用有界队列"
        );
        Self::spawn(
            root,
            kind,
            Some(source),
            Some(output),
            realtime,
            hotwords,
            revision,
            revision,
            revision_model,
            language,
            punctuation,
            None,
            observer,
        )
    }
    pub fn file_from_historical_run(
        root: PathBuf,
        source: PathBuf,
        output: PathBuf,
        run: &TranscriptionRun,
    ) -> Result<Self> {
        let spec = HistoricalRunSpec::parse(run)?;
        Self::spawn(
            root,
            spec.kind,
            Some(source),
            Some(output),
            false,
            run.requested_hotwords.clone(),
            run.revision_scheduled,
            run.revision_scheduled,
            spec.revision_model,
            spec.language,
            run.punctuation_requested,
            Some(spec),
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        root: PathBuf,
        kind: ModelKind,
        source: Option<PathBuf>,
        destination: Option<PathBuf>,
        realtime: bool,
        hotwords: String,
        context_revision: bool,
        revision_requested: bool,
        revision_model: ModelKind,
        language: TranscriptionLanguage,
        punctuation_enabled: bool,
        historical: Option<HistoricalRunSpec>,
        mut file_observer: Option<Sender<PcmAudio>>,
    ) -> Result<Self> {
        anyhow::ensure!(!revision_model.is_streaming(), "整句校正需要文件识别模型");
        let (input, rx) = bounded(1400); // about 30 seconds of mixed recording PCM
        let (tx, events) = unbounded();
        let initial_activity = destination
            .as_ref()
            .map(|path| opencap_core::managed_meeting_activity(path))
            .transpose()?
            .flatten();
        let output = Arc::new(Mutex::new(destination));
        let worker_output = output.clone();
        let activity = Arc::new(Mutex::new(MeetingActivity {
            lease: initial_activity,
            ended: false,
        }));
        let worker_activity = activity.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let expected_end = Arc::new(AtomicU64::new(0));
        let worker_end = expected_end.clone();
        let worker = thread::Builder::new()
            .name("opencap-asr-cpu".into())
            .spawn(move || {
                let job_started = Instant::now();
                let started_unix_ms = unix_ms();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _ = tx.send(AsrEvent::Status("正在准备本地识别模型…".into()));
                        if let Some(spec) = &historical {
                            anyhow::ensure!(kind.ready(&root), "历史识别模型未安装；没有自动换用其他模型");
                            if spec.revision_hash.is_some() {
                                anyhow::ensure!(revision_model.ready(&root), "历史句段复核模型未安装；没有自动换用其他模型");
                            }
                        }
                        prepare_cancellable(kind, &root, &worker_cancelled, |bytes, total| {
                            let message = total
                                .map(|total| {
                                    format!(
                                        "下载模型 {:.0}%",
                                        bytes as f64 * 100.0 / total.max(1) as f64
                                    )
                                })
                                .unwrap_or_else(|| {
                                    format!("下载模型 {:.1} MB", bytes as f64 / 1e6)
                                });
                            let _ = tx.send(AsrEvent::Status(message));
                        })?;
                        if kind.is_streaming() && context_revision {
                            let _ = tx.send(AsrEvent::Status("正在准备上下文校正模型…".into()));
                            prepare_cancellable(
                                revision_model,
                                &root,
                                &worker_cancelled,
                                |bytes, total| {
                                    let _ = tx.send(AsrEvent::Status(format!(
                                        "下载校正模型 {:.0}%",
                                        bytes as f64 * 100.0 / total.unwrap_or(bytes.max(1)) as f64
                                    )));
                                },
                            )?;
                        }
                        let revision = if kind.is_streaming() && context_revision {
                            Some(revision_model.path(&root))
                        } else {
                            None
                        };
                        let _ = tx.send(AsrEvent::Status("正在记录本次模型文件指纹…".into()));
                        let mut model_fingerprint = match crate::models::fingerprint_model(
                            kind,
                            &root,
                            &worker_cancelled,
                            false,
                        ) {
                            Ok(value) => Some(value),
                            Err(error) => {
                                crate::models::ensure_download_active(&worker_cancelled)?;
                                let _ = tx.send(AsrEvent::Status(format!(
                                    "模型指纹未记录，识别继续：{error:#}"
                                )));
                                None
                            }
                        };
                        let revision_fingerprint = if revision.is_some() {
                            match crate::models::fingerprint_model(
                                revision_model,
                                &root,
                                &worker_cancelled,
                                false,
                            ) {
                                Ok(value) => Some(value),
                                Err(error) => {
                                    crate::models::ensure_download_active(&worker_cancelled)?;
                                    let _ = tx.send(AsrEvent::Status(format!(
                                        "复核模型指纹未记录：{error:#}"
                                    )));
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        if let Some(spec) = &historical {
                            anyhow::ensure!(model_fingerprint.as_ref().is_some_and(|value| value.manifest_sha256 == spec.model_hash),
                                "当前识别模型与历史指纹不一致；没有开始重转");
                            if let Some(expected) = &spec.revision_hash {
                                anyhow::ensure!(revision_fingerprint.as_ref().is_some_and(|value| &value.manifest_sha256 == expected),
                                    "当前复核模型与历史指纹不一致；没有开始重转");
                            }
                        }
                        // SenseVoice emits punctuation itself. Streaming and
                        // Paraformer still need the optional CT model.
                        let needs_punctuation_model =
                            punctuation_enabled && kind != ModelKind::Offline;
                        let allow_punctuation_model = historical.as_ref()
                            .is_none_or(|spec| spec.run.punctuation_effective == "ct_transformer");
                        if let Some(expected) = historical.as_ref().and_then(|spec| spec.punctuation_hash.as_ref()) {
                            let before = opencap_core::fingerprint_media(
                                &crate::punctuation::punctuation_model_path(&root), Some(&worker_cancelled))?;
                            anyhow::ensure!(&before.sha256 == expected, "加载前标点模型与历史指纹不一致；没有开始重转");
                        }
                        let punctuation_before = if needs_punctuation_model && allow_punctuation_model
                            && crate::punctuation::punctuation_ready(&root) {
                            let result = opencap_core::fingerprint_media(
                                &crate::punctuation::punctuation_model_path(&root), Some(&worker_cancelled));
                            crate::models::ensure_download_active(&worker_cancelled)?;
                            result.ok()
                        } else { None };
                        let punctuation = Arc::new(Punctuator::with_model_policy(
                            &root, needs_punctuation_model, language, allow_punctuation_model));
                        let punctuation_fingerprint = if punctuation.has_model() {
                            match opencap_core::fingerprint_media(&crate::punctuation::punctuation_model_path(&root), Some(&worker_cancelled)) {
                                Ok(value) if punctuation_before.as_ref() == Some(&value) => Some(value),
                                Ok(_) => {
                                    let _ = tx.send(AsrEvent::Status("标点模型加载前后指纹未能确认，本次指纹记为未知".into()));
                                    None
                                }
                                Err(error) => {
                                    crate::models::ensure_download_active(&worker_cancelled)?;
                                    let _ = tx.send(AsrEvent::Status(format!("标点模型指纹未记录：{error:#}")));
                                    None
                                }
                            }
                        } else { None };
                        let punctuation_effective = if kind == ModelKind::Offline { "model_native" }
                            else if punctuation.has_model() { "ct_transformer" }
                            else if punctuation_enabled { "sentence_end_fallback" } else { "disabled" };
                        if let Some(spec) = &historical {
                            anyhow::ensure!(punctuation_effective == spec.run.punctuation_effective, "历史标点方式当前不可用；没有自动降级");
                            if let Some(expected) = &spec.punctuation_hash {
                                anyhow::ensure!(punctuation_fingerprint.as_ref().is_some_and(|value| &value.sha256 == expected),
                                    "当前标点模型与历史指纹不一致；没有开始重转");
                            }
                        }
                        let (mut engine, mut notices, accepted_hotwords) = Engine::new(
                            kind,
                            &kind.path(&root),
                            &hotwords,
                            language,
                            revision.as_deref().map(|dir| (revision_model, dir)),
                            revision_fingerprint.as_ref().map(|fingerprint| fingerprint.manifest_sha256.as_str()),
                            worker_cancelled.clone(),
                            &tx,
                            punctuation.clone(),
                        )?;
                        if let Some(spec) = &historical {
                            let _ = tx.send(AsrEvent::Status("正在核对模型加载后的文件指纹…".into()));
                            let after = crate::models::fingerprint_model(kind, &root, &worker_cancelled, false)?;
                            anyhow::ensure!(after.manifest_sha256 == spec.model_hash,
                                "识别模型在加载期间发生变化；没有开始重转");
                            if let Some(expected) = &spec.revision_hash {
                                let after = crate::models::fingerprint_model(revision_model, &root, &worker_cancelled, false)?;
                                anyhow::ensure!(&after.manifest_sha256 == expected,
                                    "复核模型在准备期间发生变化；没有开始重转");
                            }
                        } else if let Some(before) = &model_fingerprint {
                            let before_hash = before.manifest_sha256.clone();
                            match crate::models::fingerprint_model(kind, &root, &worker_cancelled, false) {
                                Ok(after) if after.manifest_sha256 == before_hash => (),
                                result => {
                                    crate::models::ensure_download_active(&worker_cancelled)?;
                                    let detail = match result {
                                        Ok(_) => "模型文件在加载期间变化".to_string(),
                                        Err(error) => format!("加载后模型复核失败：{error:#}"),
                                    };
                                    // The engine may already hold usable weights, but the
                                    // pre-load hash no longer identifies them reliably.
                                    model_fingerprint = None;
                                    notices.push(format!("{detail}；本次模型指纹记为未知"));
                                }
                            }
                        }
                        let known_weight_match = model_fingerprint.as_ref()
                            .and_then(|fingerprint| crate::models::known_weights_match(kind, fingerprint));
                        let known_revision_match = revision_fingerprint.as_ref()
                            .and_then(|fingerprint| crate::models::known_weights_match(revision_model, fingerprint));
                        let known_punctuation_match = punctuation_fingerprint.as_ref()
                            .map(crate::punctuation::punctuation_reference_match);
                        if known_weight_match == Some(false) {
                            notices.push("模型文件指纹与固定参考权重不同；可能为自定义或已更改文件".into());
                        }
                        if known_revision_match == Some(false) {
                            notices.push("复核模型与固定参考权重不同；可能为自定义或已更改文件".into());
                        }
                        if known_punctuation_match == Some(false) {
                            notices.push("标点模型与固定参考权重不同；可能为自定义或已更改文件".into());
                        }
                        if let Some(spec) = &historical {
                            anyhow::ensure!(accepted_hotwords == spec.run.accepted_hotwords,
                                "当前模型接受的热词与历史记录不一致；没有开始重转");
                        }
                        if needs_punctuation_model && !punctuation.has_model() {
                            notices.push(
                                if historical.is_some() && !allow_punctuation_model {
                                    "按历史方式仅补句末标点".into()
                                } else { "标点模型未安装，仅补句末标点；可在设置下载中英标点模型".into() },
                            );
                        }
                        let status = if source.is_some() {
                            "文件识别已就绪 · 正在处理音频"
                        } else if !kind.is_streaming() {
                            "逐句模式已就绪 · 停顿后出字；流式模型支持边说边显示"
                        } else {
                            "流式识别已就绪 · 边说边显示，当前句可修正"
                        };
                        let _ = tx.send(AsrEvent::Status(if notices.is_empty() {
                            format!("{status} · {}", kind.label())
                        } else {
                            format!("{status} · {} · {}", kind.label(), notices.join("；"))
                        }));
                        let input_before = if let Some(path) = &source {
                            let _ = tx.send(AsrEvent::Status("正在记录输入媒体指纹…".into()));
                            match opencap_core::fingerprint_media(path, Some(&worker_cancelled)) {
                                Ok(value) => Some(value),
                                Err(error) => {
                                    crate::models::ensure_download_active(&worker_cancelled)?;
                                    let _ = tx.send(AsrEvent::Status(format!("媒体指纹未记录，识别继续：{error:#}")));
                                    None
                                }
                            }
                        } else { None };
                        if let Some(spec) = &historical && let Some(expected) = &spec.media {
                            anyhow::ensure!(input_before.as_ref() == Some(expected), "所选媒体与历史运行的指纹不一致；没有开始重转");
                        }
                        let mut transcript = Transcript {
                            source: source.clone(),
                            model: kind.directory().into(),
                            run: Some(TranscriptionRun {
                                run_id: opencap_core::new_stable_id(),
                                started_unix_ms,
                                finished_unix_ms: None,
                                application_version: env!("CARGO_PKG_VERSION").into(),
                                backend: "sherpa-onnx".into(),
                                runtime_version: "1.13.8".into(),
                                model_id: kind.directory().into(),
                                model_sha256: model_fingerprint
                                    .as_ref()
                                    .and_then(|value| value.files.get("model.int8.onnx"))
                                    .map(|file| file.sha256.clone()),
                                provider: "cpu".into(),
                                threads: 1,
                                requested_language: language.code().into(),
                                // The API accepts a mode but does not report the
                                // language actually recognized in each segment.
                                effective_language: None,
                                requested_hotwords: hotwords.clone(),
                                accepted_hotwords,
                                punctuation_requested: punctuation_enabled,
                                punctuation_effective: punctuation_effective.into(),
                                revision_scheduled: revision.is_some(),
                                revision_model: revision
                                    .as_ref()
                                    .map(|_| revision_model.directory().into()),
                                input_kind: if source.is_none() {
                                    "recording"
                                } else if realtime {
                                    "file_realtime"
                                } else {
                                    "file"
                                }
                                .into(),
                                state: "running".into(),
                                meeting_id: None,
                                input_asset_id: None,
                                extra: {
                                    let mut extra = std::collections::BTreeMap::new();
                                    if kind == ModelKind::Offline {
                                        extra.insert("language_mode_sent".into(), language.code().into());
                                        extra.insert("itn_requested".into(), true.into());
                                    }
                                    if revision.is_some() && revision_model == ModelKind::Offline {
                                        extra.insert("revision_language_mode_configured".into(), language.code().into());
                                        extra.insert("revision_itn_configured".into(), true.into());
                                    }
                                    extra.insert("model_fingerprint_verification".into(),
                                        if model_fingerprint.is_some() { "before_after_load_matched" } else { "unknown" }.into());
                                    extra.insert("punctuation_fingerprint_verification".into(),
                                        if punctuation_fingerprint.is_some() { "before_after_load_matched" }
                                        else if punctuation.has_model() { "unknown" } else { "not_applicable" }.into());
                                    if let Some(value) = &model_fingerprint {
                                        extra.insert(
                                            "model_fingerprint".into(),
                                            serde_json::to_value(value)?,
                                        );
                                    }
                                    if let Some(matches) = known_weight_match {
                                        extra.insert("known_model_weights_match".into(), matches.into());
                                    }
                                    if let Some(matches) = known_revision_match {
                                        extra.insert("known_revision_weights_match".into(), matches.into());
                                    }
                                    if let Some(matches) = known_punctuation_match {
                                        extra.insert("known_punctuation_weights_match".into(), matches.into());
                                    }
                                    if let Some(value) = &revision_fingerprint {
                                        extra.insert(
                                            "revision_model_fingerprint".into(),
                                            serde_json::to_value(value)?,
                                        );
                                        extra.insert("revision_fingerprint_policy".into(), "verify_before_after_load".into());
                                    }
                                    if let Some(value) = &punctuation_fingerprint {
                                        extra.insert("punctuation_fingerprint".into(), serde_json::to_value(value)?);
                                    }
                                    extra.insert("revision_requested".into(), revision_requested.into());
                                    extra.insert("revision_effective".into(), engine.revision_state()
                                        .map(RevisionRuntimeState::code)
                                        .unwrap_or(if revision_requested { "not_scheduled" } else { "disabled" }).into());
                                    if let Some(reason) = engine.revision_failure() {
                                        extra.insert("revision_failure".into(), reason.into());
                                    }
                                    if let Some(spec) = &historical {
                                        extra.insert("parent_run_id".into(), spec.run.run_id.clone().into());
                                        // Only these recorded settings were checked.
                                        // File decoding is not equivalent to the original
                                        // live capture/processing chain.
                                        extra.insert("historical_settings_match".into(), false.into());
                                        extra.insert("historical_verified_settings".into(), serde_json::json!([
                                            "application_version", "backend", "runtime_version",
                                            "provider", "threads", "model_fingerprint",
                                            "requested_language", "accepted_hotwords", "punctuation",
                                            "scheduled_revision_model"
                                        ]));
                                        extra.insert("historical_match_scope".into(),
                                            "verified_model_settings_only; input processing may differ".into());
                                        extra.insert("parent_input_kind".into(), spec.run.input_kind.clone().into());
                                        extra.insert("parent_audio_stream_recorded".into(), spec.audio_stream_index.is_some().into());
                                        // Final verification requires a second hash after decoding.
                                        extra.insert("parent_media_verified".into(), false.into());
                                    }
                                    extra.insert(
                                        "hotword_support".into(),
                                        serde_json::json!(if kind.is_streaming() {
                                            "vocabulary_filtered"
                                        } else {
                                            "not_supported_by_current_path"
                                        }),
                                    );
                                    extra.insert("recognizer_time_mapping".into(), serde_json::json!({
                                        "method": "normalized_sample_clock_v1",
                                        "origin": "first_input_packet_pts_per_contiguous_epoch",
                                        "sample_rate_hz": 16000,
                                        "filter_delay": "not_independently_measured",
                                        "precision": "sample_clock_estimate"
                                    }));
                                    if source.is_none() {
                                        extra.insert("input_audio".into(), serde_json::json!({
                                            "source":"mixed_recording", "sample_rate":48000, "channels":1,
                                            "timeline":"recording_media_clock", "file_fingerprint":"not_available_for_live_pcm"
                                        }));
                                    }
                                    extra
                                },
                            }),
                            ..Default::default()
                        };
                        if let Some(run) = transcript.run.clone() {
                            let _ = tx.send(AsrEvent::Started(run));
                        }
                        let mut normalizer = Normalizer::new();
                        let mut normalized_clock = NormalizedClock::default();
                        // Live PCM is generated on the recording timeline from
                        // zero. Starting at zero also detects a dropped first
                        // packet; imported media may legitimately start later.
                        let live_input = source.is_none();
                        let mut expected_us: Option<i64> = live_input.then_some(0);
                        // Mixer packet timestamps use integer microseconds;
                        // leave a small rounding allowance without hiding a
                        // full 1024-sample packet (~21 ms).
                        let gap_tolerance_us = if live_input { 2_000 } else { 200_000 };
                        let mut audio_generation: Option<u64> = None;
                        let mut input_rate: Option<u32> = None;
                        let began = Instant::now();
                        let mut last_progress = Instant::now();
                        let mut last_checkpoint = Instant::now();
                        let mut checkpoint_dirty = false;
                        let mut time_reversals = Vec::new();
                        let mut time_reversal_count = 0u64;
                        let mut normalize_timing = BlockTiming::default();
                        let mut feed_timing = BlockTiming::default();
                        let mut observed_frames = 0u64;
                        let mut observed_audio_us = 0u64;
                        let read_result = {
                            let mut consume = |audio: Option<PcmAudio>,
                                               duration: Option<f64>|
                             -> Result<()> {
                                engine.poll_corrections(&mut transcript, &tx);
                                let Some(audio) = audio else { return Ok(()) };
                                anyhow::ensure!(audio.sample_rate > 0 && audio.channels > 0
                                    && audio.samples.len() % usize::from(audio.channels) == 0,
                                    "识别输入音频格式无效");
                                if let Some(seconds) = duration.filter(|s| s.is_finite() && *s > 0.)
                                {
                                    transcript.source_duration_ms = Some((seconds * 1000.) as u64);
                                }
                                anyhow::ensure!(
                                    !worker_cancelled.load(Ordering::Relaxed),
                                    "转录已取消"
                                );
                                let generation_changed = audio_generation
                                    .is_some_and(|previous| previous != audio.generation);
                                let rate_changed = !audio.samples.is_empty()
                                    && input_rate.is_some_and(|previous| previous != audio.sample_rate);
                                // Explicit decoder discontinuities must not be
                                // hidden by the wider timestamp tolerance used
                                // for ordinary imported-media packets.
                                let boundary_tolerance_us = if generation_changed { 2_000 } else { gap_tolerance_us };
                                let time_reversed = expected_us.is_some_and(|expected|
                                    expected > audio.pts_us.saturating_add(boundary_tolerance_us));
                                if time_reversed && let Some(expected) = expected_us {
                                    let from = expected.max(0) as u64 / 1000;
                                    let to = audio.pts_us.max(0) as u64 / 1000;
                                    time_reversal_count = time_reversal_count.saturating_add(1);
                                    if time_reversals.len() < 1024 {
                                        time_reversals.push(serde_json::json!({ "from_ms": from, "to_ms": to }));
                                    }
                                    let _ = tx.send(AsrEvent::Status(format!(
                                        "音频时间从 {from}ms 回退到 {to}ms；保留后续文字，结果标记为部分完成。"
                                    )));
                                }
                                // An unmarked backwards jump must also end the
                                // recognizer and second-pass input. Otherwise
                                // samples from two media epochs would be joined
                                // and assigned the old stream's time origin.
                                if audio.samples.is_empty() || generation_changed || time_reversed || rate_changed {
                                    if let Some(expected) = expected_us
                                        && audio.pts_us > expected.saturating_add(boundary_tolerance_us)
                                    {
                                        let gap = (
                                            expected.max(0) as u64 / 1000,
                                            audio.pts_us.max(0) as u64 / 1000,
                                        );
                                        transcript.gaps.push(gap);
                                        let _ = tx.send(AsrEvent::Gap(gap.0, gap.1));
                                    }
                                    let tail = normalizer.flush()?;
                                    engine.feed(
                                        &tail,
                                        normalized_clock.next_ms(expected_us.unwrap_or(0)),
                                        &mut transcript,
                                        &tx,
                                    )?;
                                    normalized_clock.advance(tail.len());
                                    engine.boundary(&mut transcript, &tx)?;
                                    normalizer = Normalizer::new();
                                    normalized_clock = NormalizedClock::default();
                                    input_rate = None;
                                    expected_us = Some(audio.pts_us);
                                    if audio.samples.is_empty() {
                                        audio_generation = Some(audio.generation);
                                        return Ok(());
                                    }
                                }
                                audio_generation = Some(audio.generation);
                                if let Some(expected) = expected_us
                                    && audio.pts_us > expected.saturating_add(gap_tolerance_us)
                                {
                                    // Drain the resampling filter before closing the old
                                    // timeline; resetting first silently drops its tail.
                                    let tail = normalizer.flush()?;
                                    engine.feed(&tail, normalized_clock.next_ms(expected), &mut transcript, &tx)?;
                                    normalized_clock.advance(tail.len());
                                    engine.boundary(&mut transcript, &tx)?;
                                    let gap = (
                                        expected.max(0) as u64 / 1000,
                                        audio.pts_us.max(0) as u64 / 1000,
                                    );
                                    transcript.gaps.push(gap);
                                    let _ = tx.send(AsrEvent::Gap(gap.0, gap.1));
                                    normalizer = Normalizer::new();
                                    normalized_clock = NormalizedClock::default();
                                }
                                let end_us = audio.pts_us
                                    + audio.samples.len() as i64 * 1_000_000
                                        / audio.channels as i64
                                        / audio.sample_rate as i64;
                                let frames = (audio.samples.len() / usize::from(audio.channels)) as u64;
                                observed_frames = observed_frames.saturating_add(frames);
                                normalized_clock.begin(audio.pts_us);
                                let started = Instant::now();
                                let normalized = normalizer.convert(&audio);
                                normalize_timing.record(started.elapsed());
                                let normalized = normalized?;
                                input_rate = Some(audio.sample_rate);
                                observed_audio_us = observed_audio_us.saturating_add(
                                    ((u128::from(frames) * 1_000_000) / u128::from(audio.sample_rate)).min(u128::from(u64::MAX)) as u64);
                                let started = Instant::now();
                                let feed_result = engine.feed(
                                    &normalized,
                                    normalized_clock.next_ms(audio.pts_us),
                                    &mut transcript,
                                    &tx,
                                );
                                feed_timing.record(started.elapsed());
                                feed_result?;
                                normalized_clock.advance(normalized.len());
                                expected_us = Some(end_us);
                                if live_input && last_progress.elapsed() >= Duration::from_secs(1) {
                                    let _ =
                                        tx.send(AsrEvent::Processed(end_us.max(0) as u64 / 1000));
                                    last_progress = Instant::now();
                                }
                                if let Some(duration) = duration
                                    && last_progress.elapsed() >= Duration::from_millis(300)
                                {
                                    let _ = tx.send(AsrEvent::Progress(
                                        (end_us as f64 / 1e6 / duration).clamp(0.0, 1.0),
                                    ));
                                    last_progress = Instant::now();
                                }
                                if realtime {
                                    let remaining = Duration::from_micros(end_us.max(0) as u64)
                                        .saturating_sub(began.elapsed());
                                    if !remaining.is_zero() {
                                        thread::sleep(remaining.min(Duration::from_millis(200)));
                                    }
                                }
                                checkpoint_dirty |= engine.take_committed();
                                // Never hold the UI-shared path mutex while serializing or writing.
                                if checkpoint_dirty
                                    && last_checkpoint.elapsed() >= Duration::from_secs(5)
                                {
                                    let base = worker_output.lock().unwrap().clone();
                                    if let Some(base) = base {
                                        if transcript.source.is_none() {
                                            transcript.source = Some(base.clone());
                                        }
                                        if let Err(error) =
                                            attach_run_identity(&mut transcript, &base, false)
                                        {
                                            let _ = tx.send(AsrEvent::Status(format!(
                                                "识别继续；会议身份记录失败：{error:#}"
                                            )));
                                        }
                                        transcript.checkpoint(&base)?;
                                        checkpoint_dirty = false;
                                        last_checkpoint = Instant::now();
                                    }
                                }
                                Ok(())
                            };
                            if let Some(source) = &source {
                                let had_observer = file_observer.is_some();
                                let mut decoded_end_us = None;
                                let decoded = decode_file_with_expected_stream(source, historical.as_ref().and_then(|spec| spec.audio_stream_index), |audio, duration| {
                                    let frames = audio.samples.len() / usize::from(audio.channels.max(1));
                                    decoded_end_us = Some(audio.pts_us.saturating_add(
                                        (frames as i64).saturating_mul(1_000_000) / i64::from(audio.sample_rate.max(1))));
                                    // The speaker consumer must never delay recognition.
                                    let sent = file_observer.as_ref().map(|observer| observer.try_send(audio.clone()));
                                    match sent {
                                        Some(Err(crossbeam_channel::TrySendError::Full(_))) => {
                                            let start = audio.pts_us.max(0) as u64 / 1000;
                                            let frames = audio.samples.len() / usize::from(audio.channels.max(1));
                                            let end = start.saturating_add(frames as u64 * 1000 / u64::from(audio.sample_rate.max(1)));
                                            let _ = tx.send(AsrEvent::SpeakerInputGap(start, end));
                                            file_observer = None;
                                        }
                                        Some(Err(crossbeam_channel::TrySendError::Disconnected(_))) => {
                                            file_observer = None;
                                            // Cancellation or a stopped observer can release the
                                            // producer before decode reaches EOF. Never report
                                            // that prefix as a complete file-speaker input.
                                            let _ = tx.send(AsrEvent::FileSpeakerInputEnded {
                                                end_ms: None,
                                                complete: false,
                                            });
                                        }
                                        _ => (),
                                    }
                                    consume(Some(audio), duration)
                                });
                                let delivered = file_observer.take().is_some();
                                if had_observer && delivered {
                                    let end_ms = decoded_end_us.map(|end: i64| end.max(0) as u64 / 1000);
                                    let _ = tx.send(AsrEvent::FileSpeakerInputEnded {
                                        end_ms,
                                        complete: decoded.is_ok() && end_ms.is_some(),
                                    });
                                }
                                decoded.map(Some)
                            } else {
                                let mut result = Ok(());
                                loop {
                                    match rx.recv_timeout(Duration::from_millis(100)) {
                                        Ok(audio) => {
                                            if let Err(e) = consume(Some(audio), None) {
                                                result = Err(e);
                                                break;
                                            }
                                        }
                                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                                            consume(None, None)?;
                                            if worker_cancelled.load(Ordering::Relaxed) {
                                                break;
                                            }
                                        }
                                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                            break;
                                        }
                                    }
                                }
                                result.map(|()| None)
                            }
                        };
                        // Finalization errors must not discard already accepted
                        // transcript segments or skip pending second-pass work.
                        // The unknown tail is explicitly reported as incomplete;
                        // its exact interval cannot safely be inferred here.
                        let mut drain_errors = Vec::new();
                        match normalizer.flush() {
                            Ok(tail) => {
                                if let Err(error) = engine.feed(
                                    &tail,
                                    normalized_clock.next_ms(expected_us.unwrap_or(0)),
                                    &mut transcript,
                                    &tx,
                                ) {
                                    drain_errors.push(format!("尾部识别失败：{error:#}"));
                                }
                            }
                            Err(error) => drain_errors.push(format!("重采样尾部排空失败：{error:#}")),
                        }
                        if let Err(error) = engine.boundary(&mut transcript, &tx) {
                            drain_errors.push(format!("识别流结束失败：{error:#}"));
                        }
                        engine.finish_corrections(&mut transcript, &tx);
                        let unconfirmed_segments = transcript.mark_terminal_unconfirmed();
                        if !drain_errors.is_empty() {
                            let message = drain_errors.join("；");
                            if let Some(run) = &mut transcript.run {
                                run.extra.insert("drain_errors".into(), serde_json::json!(&drain_errors));
                            }
                            let _ = tx.send(AsrEvent::Status(format!(
                                "识别收尾未完整完成，已确认文字将保存：{message}"
                            )));
                        }
                        transcript.cancelled = worker_cancelled.load(Ordering::Relaxed);
                        if let Ok(Some(info)) = &read_result
                            && let Some(run) = &mut transcript.run
                        {
                            run.extra.insert("input_audio".into(), serde_json::to_value(info)?);
                        }
                        let read_result = read_result.and_then(|decoded| {
                            if let (Some(spec), Some(info)) = (&historical, decoded.as_ref())
                                && let Some(expected) = spec.audio_stream_index
                            {
                                anyhow::ensure!(
                                    u64::try_from(info.audio_stream_index).ok() == Some(expected),
                                    "所选音轨与历史运行不一致；结果不能标记为完整重转"
                                );
                            }
                            Ok(decoded)
                        });
                        if let Err(error) = &read_result
                            && let Some(run) = &mut transcript.run
                        {
                            run.extra.insert("input_error".into(), format!("{error:#}").into());
                        }
                        let mut source_changed = false;
                        let mut verified_fingerprint = None;
                        if let Some(path) = &source {
                            let (state, fingerprint) = if let Some(before) = input_before {
                                if transcript.cancelled || read_result.is_err() {
                                    ("not_checked_after_partial_input", None)
                                } else {
                                    let _ = tx.send(AsrEvent::Status("正在复核输入媒体指纹…".into()));
                                    match opencap_core::fingerprint_media(path, Some(&worker_cancelled)) {
                                        Ok(after) if before == after => ("hash_matched_before_and_after", Some(after)),
                                        Ok(_) => { source_changed = true; ("changed_during_transcription", None) },
                                        Err(_) => ("verification_unavailable_after_transcription", None),
                                    }
                                }
                            } else { ("fingerprint_unavailable", None) };
                            if let Some(run) = &mut transcript.run {
                                run.extra.insert("input_integrity".into(), state.into());
                                if let Ok(canonical) = path.canonicalize() {
                                    run.extra.insert("canonical_media_origin".into(), serde_json::to_value(canonical)?);
                                }
                                if let Some(fingerprint) = &fingerprint {
                                    run.extra.insert("input_media_fingerprint".into(), serde_json::to_value(fingerprint)?);
                                }
                            }
                            verified_fingerprint = fingerprint;
                            if source_changed {
                                let _ = tx.send(AsrEvent::Status("媒体内容在转录期间发生变化，结果已标为部分完成；请检查原文件。".into()));
                            }
                        }
                        transcript.cancelled = worker_cancelled.load(Ordering::Relaxed);
                        if !time_reversals.is_empty()
                            && let Some(run) = &mut transcript.run
                        {
                            run.extra.insert("input_time_reversals".into(), time_reversals.clone().into());
                            run.extra.insert("input_time_reversal_count".into(), time_reversal_count.into());
                        }
                        let last_ms = expected_us.unwrap_or(0).max(0) as u64 / 1000;
                        let wanted = worker_end.load(Ordering::Relaxed);
                        let tail_tolerance_ms = if live_input { 2 } else { 200 };
                        if wanted > last_ms.saturating_add(tail_tolerance_ms) {
                            transcript.gaps.push((last_ms, wanted));
                            let _ = tx.send(AsrEvent::Gap(last_ms, wanted));
                        }
                        transcript.complete = read_result.is_ok()
                            && !transcript.cancelled
                            && !source_changed
                            && drain_errors.is_empty()
                            && unconfirmed_segments == 0
                            && time_reversal_count == 0
                            && transcript.gaps.is_empty();
                        if let Some(spec) = &historical {
                            let media_outcome = if spec.media.is_none() {
                                "no_historical_fingerprint"
                            } else if spec.run.extra.get("input_integrity").and_then(|value| value.as_str())
                                != Some("hash_matched_before_and_after") {
                                "historical_integrity_unverified"
                            } else if verified_fingerprint.as_ref() == spec.media.as_ref()
                                && !transcript.cancelled && read_result.is_ok() {
                                "matched_before_and_after"
                            } else if source_changed {
                                "changed_during_transcription"
                            } else if transcript.cancelled || read_result.is_err() {
                                "partial_input"
                            } else {
                                "verification_unavailable"
                            };
                            let incomplete_reviews = transcript.segments.iter().filter(|segment| {
                                segment.review.as_ref().is_some_and(|review| matches!(review.state,
                                    crate::ReviewState::Pending | crate::ReviewState::Deferred | crate::ReviewState::Failed))
                            }).count();
                            let revision_outcome = if spec.revision_hash.is_none() {
                                "not_required"
                            } else if engine.revision_state() == Some(RevisionRuntimeState::Ready) && incomplete_reviews == 0 {
                                "ready_without_incomplete_reviews"
                            } else {
                                "incomplete"
                            };
                            if let Some(run) = &mut transcript.run {
                                run.extra.insert("parent_media_verified".into(), (media_outcome == "matched_before_and_after").into());
                                run.extra.insert("historical_media_outcome".into(), media_outcome.into());
                                run.extra.insert("historical_revision_outcome".into(), revision_outcome.into());
                                run.extra.insert("historical_incomplete_review_count".into(), (incomplete_reviews as u64).into());
                            }
                        }
                        transcript.processing_seconds = Some(job_started.elapsed().as_secs_f64());
                        if let Some(run) = &mut transcript.run {
                            run.extra.insert("block_processing_timing".into(), serde_json::json!({
                                "normalization":normalize_timing.report(), "recognizer_feed":feed_timing.report(),
                                "input_frames":observed_frames,
                                "input_audio_us_for_feed":observed_audio_us,
                                "feed_wall_time_over_audio":(observed_audio_us > 0).then(|| feed_timing.total_us as f64 / observed_audio_us as f64),
                                "scope":"wall time of synchronous nonempty PCM blocks; excludes boundary flush, asynchronous review, decoding and checkpoint I/O",
                                "percentiles":"histogram upper bounds", "gpu_completion":"not measured"
                            }));
                            run.finished_unix_ms = Some(unix_ms());
                            run.state = if transcript.cancelled {
                                "cancelled"
                            } else if transcript.complete {
                                "completed"
                            } else {
                                "partial"
                            }
                            .into();
                        }
                        let base = worker_output.lock().unwrap().clone();
                        if let Some(base) = base {
                            if transcript.source.is_none() {
                                transcript.source = Some(base.clone());
                            }
                            let identity_error =
                                attach_run_identity(&mut transcript, &base, true).err();
                            if identity_error.is_some() && verified_fingerprint.is_some()
                                && let Some(run) = &mut transcript.run
                            {
                                run.extra.insert("asset_fingerprint_warning".into(),
                                    "会议身份尚未登记，资产指纹未写入".into());
                            }
                            if identity_error.is_none()
                                && let (Some(fingerprint), Some(source), Some(run)) =
                                (verified_fingerprint.as_ref(), source.as_ref(), transcript.run.as_mut())
                                && let Some(asset_id) = run.input_asset_id.as_deref()
                                && let Some(meeting_id) = run.meeting_id.as_deref()
                                && let Err(error) = opencap_core::record_asset_fingerprint(&base, meeting_id, asset_id, source, fingerprint)
                            {
                                run.extra.insert("asset_fingerprint_warning".into(), format!("{error:#}").into());
                            }
                            if let Some(reason) = transcript.run.as_ref()
                                .and_then(|run| run.extra.get("asset_fingerprint_warning"))
                                .and_then(|value| value.as_str())
                            {
                                transcript.output_warnings.push(format!("媒体资产指纹登记失败：{reason}"));
                            }
                            transcript.sync_outputs(&base);
                            let archive_digest = {
                                use sha2::{Digest, Sha256};
                                serde_json::to_vec_pretty(&transcript).ok()
                                    .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
                            };
                            let mut final_metadata_changed = false;
                            if let Err(error) = crate::archive_completed_run(&base, &transcript) {
                                if let (Some(run), Some(digest)) = (&mut transcript.run, archive_digest) {
                                    run.extra.insert("pending_archive_sha256".into(), digest.into());
                                }
                                transcript.output_warnings.push(format!("识别历史版本未归档：{error:#}"));
                                final_metadata_changed = true;
                            }
                            if let Some(error) = identity_error {
                                transcript
                                    .output_warnings
                                    .push(format!("会议身份记录失败：{error:#}"));
                                final_metadata_changed = true;
                            }
                            if final_metadata_changed && let Err(error) = transcript.checkpoint(&base) {
                                transcript.primary_save_pending = true;
                                transcript.output_warnings.push(format!("最终运行状态未保存：{error:#}"));
                            }
                        }
                        let _ = tx.send(AsrEvent::Finished(Box::new(transcript)));
                        if !worker_cancelled.load(Ordering::Relaxed) {
                            read_result?;
                        }
                        Ok(())
                    }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(_) => Some("识别线程异常，原录制文件已保留".into()),
                };
                if let Some(error) = error {
                    let _ = tx.send(AsrEvent::Error(error));
                }
                // The UI can retain a completed AsrSession; the lease belongs
                // to the running task, not the lifetime of that UI handle.
                let mut activity = worker_activity.lock().unwrap();
                activity.ended = true;
                activity.lease.take();
            })?;
        Ok(Self {
            events,
            input: Some(input),
            output,
            activity,
            cancelled,
            expected_end,
            worker,
        })
    }
    pub fn audio_sender(&self) -> Sender<PcmAudio> {
        self.input.as_ref().expect("live input open").clone()
    }
    pub fn set_output(&self, output: PathBuf) -> Result<()> {
        let activity = opencap_core::managed_meeting_activity(&output)?;
        let mut state = self.activity.lock().unwrap();
        anyhow::ensure!(!state.ended, "识别任务已结束");
        state.lease = activity;
        *self.output.lock().unwrap() = Some(output);
        Ok(())
    }
    pub fn finish_input(&mut self) {
        self.input.take();
    }
    pub fn finish_at(&mut self, milliseconds: u64) {
        self.expected_end.store(milliseconds, Ordering::Relaxed);
        self.finish_input();
    }
    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.finish_input();
    }
    pub fn cancel_requested(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn finished(&self) -> bool {
        self.worker.is_finished()
    }
}
pub(crate) fn offline_recognizer(
    kind: ModelKind,
    dir: &Path,
    language: TranscriptionLanguage,
) -> Result<OfflineRecognizer> {
    let mut config = OfflineRecognizerConfig::default();
    let model = Some(dir.join("model.int8.onnx").to_string_lossy().into_owned());
    match kind {
        ModelKind::Offline => {
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model,
                language: Some(language.code().into()),
                use_itn: true,
            }
        }
        ModelKind::ParaformerSmall | ModelKind::ParaformerLarge => {
            config.model_config.paraformer = OfflineParaformerModelConfig { model };
        }
        _ => anyhow::bail!("{} 不是文件识别模型", kind.label()),
    }
    config.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into_owned());
    config.model_config.provider = Some("cpu".into());
    config.model_config.num_threads = 1;
    OfflineRecognizer::create(&config).with_context(|| format!("无法加载{}", kind.label()))
}
impl Drop for AsrSession {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.input.take();
    }
}

fn supported_hotwords<'a>(
    input: &'a str,
    vocabulary: &std::collections::HashSet<&str>,
) -> (Vec<&'a str>, usize) {
    let words = input
        .split([',', '，', ';', '；', '\n', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(50);
    let (accepted, rejected): (Vec<_>, Vec<_>) = words.partition(|word| {
        word.chars()
            .all(|c| vocabulary.contains(c.to_string().as_str()))
    });
    (accepted, rejected.len())
}

#[cfg(test)]
mod hotword_tests {
    use super::supported_hotwords;

    #[test]
    fn unknown_hotwords_do_not_disable_known_words_or_recognition() {
        let vocabulary = ["会", "议", "转", "写"].into_iter().collect();
        let (accepted, skipped) = supported_hotwords("会议,录屏；转写 OpenCap", &vocabulary);
        assert_eq!(accepted, vec!["会议", "转写"]);
        assert_eq!(skipped, 2);
    }
}

enum Engine {
    Online {
        recognizer: OnlineRecognizer,
        stream: OnlineStream,
        origin: Option<u64>,
        id: u64,
        revision: u64,
        previous: String,
        end: u64,
        committed: bool,
        correction: Option<Reviser>,
        utterance_audio: Option<Vec<f32>>,
        punctuation: Arc<Punctuator>,
        partial_punctuation: PartialPunctuation,
    },
    Offline {
        recognizer: OfflineRecognizer,
        vad: VoiceActivityDetector,
        history: SpeechHistory,
        origin: Option<u64>,
        id: u64,
        end: u64,
        committed: bool,
        punctuation: Arc<Punctuator>,
    },
}
impl Engine {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: ModelKind,
        dir: &Path,
        hotwords: &str,
        language: TranscriptionLanguage,
        revision_model: Option<(ModelKind, &Path)>,
        revision_expected_fingerprint: Option<&str>,
        cancelled: Arc<AtomicBool>,
        events: &Sender<AsrEvent>,
        punctuation: Arc<Punctuator>,
    ) -> Result<(Self, Vec<String>, Option<Vec<String>>)> {
        let file = |name: &str| Some(dir.join(name).to_string_lossy().into_owned());
        let mut notices = Vec::new();
        let mut accepted_hotwords = None;
        let engine = match kind {
            ModelKind::Streaming | ModelKind::StreamingStandard | ModelKind::StreamingEnhanced => {
                let mut config = OnlineRecognizerConfig {
                    decoding_method: Some("modified_beam_search".into()),
                    max_active_paths: 4,
                    enable_endpoint: true,
                    rule1_min_trailing_silence: 2.4,
                    rule2_min_trailing_silence: 0.8,
                    // A clock boundary can land in the middle of a word. Keep
                    // encoder/decoder context until real trailing silence or EOF.
                    rule3_min_utterance_length: f32::MAX,
                    ..Default::default()
                };
                config.model_config.transducer = OnlineTransducerModelConfig {
                    encoder: file(kind.files()[0]),
                    decoder: file(kind.files()[1]),
                    joiner: file(kind.files()[2]),
                };
                config.model_config.tokens = file("tokens.txt");
                config.model_config.provider = Some("cpu".into());
                config.model_config.num_threads = 1;
                let vocabulary = std::fs::read_to_string(dir.join("tokens.txt"))?;
                let vocabulary: std::collections::HashSet<_> = vocabulary
                    .lines()
                    .filter_map(|line| line.split_whitespace().next())
                    .collect();
                let (hotwords, unsupported) = supported_hotwords(hotwords, &vocabulary);
                accepted_hotwords = Some(hotwords.iter().map(|word| (*word).to_owned()).collect());
                if unsupported > 0 {
                    notices.push(format!("{unsupported} 个热词不在当前模型词表中，已跳过"));
                }
                if !hotwords.is_empty() {
                    config.hotwords_buf = Some(hotwords.join("\n").into_bytes());
                    config.hotwords_score = 3.0;
                    config.model_config.modeling_unit = Some("cjkchar".into());
                }
                let recognizer = OnlineRecognizer::create(&config).context("无法加载流式模型")?;
                let stream = recognizer.create_stream();
                Self::Online {
                    recognizer,
                    stream,
                    origin: None,
                    id: 0,
                    revision: 0,
                    previous: String::new(),
                    end: 0,
                    committed: false,
                    correction: revision_model
                        .map(|(kind, dir)| {
                            Reviser::new(
                                kind,
                                dir.to_path_buf(),
                                language,
                                cancelled,
                                events.clone(),
                                punctuation.clone(),
                                revision_expected_fingerprint.map(str::to_owned),
                            )
                        })
                        .transpose()?,
                    utterance_audio: revision_model.map(|_| Vec::new()),
                    punctuation,
                    partial_punctuation: PartialPunctuation::default(),
                }
            }
            ModelKind::Offline | ModelKind::ParaformerSmall | ModelKind::ParaformerLarge => {
                let recognizer = offline_recognizer(kind, dir, language)?;
                let config = VadModelConfig {
                    silero_vad: SileroVadModelConfig {
                        model: file("silero_vad.onnx"),
                        threshold: 0.5,
                        min_silence_duration: 0.5,
                        min_speech_duration: 0.25,
                        window_size: 512,
                        max_speech_duration: 15.0,
                    },
                    sample_rate: 16000,
                    num_threads: 1,
                    provider: Some("cpu".into()),
                    ..Default::default()
                };
                let vad = VoiceActivityDetector::create(&config, 30.0)
                    .context("无法加载语音活动检测模型")?;
                Self::Offline {
                    recognizer,
                    vad,
                    history: SpeechHistory::new(),
                    origin: None,
                    id: 0,
                    end: 0,
                    committed: false,
                    punctuation,
                }
            }
        };
        Ok((engine, notices, accepted_hotwords))
    }
    fn feed(
        &mut self,
        samples: &[f32],
        pts_ms: i64,
        t: &mut Transcript,
        tx: &Sender<AsrEvent>,
    ) -> Result<()> {
        self.poll_corrections(t, tx);
        if samples.is_empty() {
            return Ok(());
        }
        match self {
            Self::Online {
                stream,
                utterance_audio,
                origin,
                end,
                ..
            } => {
                origin.get_or_insert(pts_ms.max(0) as u64);
                *end = pts_ms.max(0) as u64 + samples.len() as u64 * 1000 / 16000;
                stream.accept_waveform(16000, samples);
                if let Some(audio) = utterance_audio {
                    if audio.len() + samples.len() <= MAX_REVISION_SAMPLES {
                        audio.extend_from_slice(samples);
                    } else {
                        // Keep all native streaming words on long uninterrupted
                        // speech; only the optional second pass is bounded.
                        *utterance_audio = None;
                    }
                }
            }
            Self::Offline {
                vad,
                history,
                origin,
                end,
                ..
            } => {
                origin.get_or_insert(pts_ms.max(0) as u64);
                *end = pts_ms.max(0) as u64 + samples.len() as u64 * 1000 / 16000;
                history.push(samples);
                vad.accept_waveform(samples);
            }
        }
        self.decode(false, t, tx)
    }
    fn decode(
        &mut self,
        force_final: bool,
        t: &mut Transcript,
        tx: &Sender<AsrEvent>,
    ) -> Result<()> {
        match self {
            Self::Online {
                recognizer,
                stream,
                origin,
                id,
                revision,
                previous,
                end,
                committed,
                correction,
                utterance_audio,
                punctuation,
                partial_punctuation,
            } => {
                while recognizer.is_ready(stream) {
                    recognizer.decode(stream);
                }
                let final_now = force_final || recognizer.is_endpoint(stream);
                let result = recognizer
                    .get_result(stream)
                    .context("流式识别没有返回结果")?;
                let text = result.text.trim().to_owned();
                if !text.is_empty() && (text != *previous || final_now) {
                    *revision += 1;
                    let base = origin.unwrap_or(0)
                        + (result.start_time.unwrap_or(0.0).max(0.0) * 1000.0) as u64;
                    let start = base
                        + result
                            .timestamps
                            .as_ref()
                            .and_then(|ts| ts.first())
                            .map(|v| (*v).max(0.0) * 1000.0)
                            .unwrap_or(0.0) as u64;
                    let last = result
                        .timestamps
                        .as_ref()
                        .and_then(|ts| ts.last())
                        .map(|v| base + ((*v).max(0.0) * 1000.0) as u64 + 100)
                        .unwrap_or(*end);
                    let rendered = if final_now {
                        punctuation.finish(&text)
                    } else {
                        partial_punctuation.render(&text, punctuation)
                    };
                    let mut extra = std::collections::BTreeMap::new();
                    if let Some(evidence) = raw_token_evidence(
                        &result.tokens,
                        result.timestamps.as_deref(),
                        None,
                        base,
                        *end,
                        &text,
                        *revision,
                    ) {
                        extra.insert("raw_token_timing".into(), evidence);
                    }
                    let mut segment = Segment {
                        extra,
                        id: *id,
                        revision: *revision,
                        start_ms: start.min(*end),
                        end_ms: last.min(*end).max(start.min(*end)),
                        text: rendered.clone(),
                        finalized: final_now,
                        correcting: false,
                        review: None,
                    };
                    bind_token_evidence(&mut segment, t.run.as_ref());
                    if final_now && let Some(correction) = correction {
                        let audio = utterance_audio.take();
                        let mut review = SentenceReview {
                            extra: Default::default(),
                            state: ReviewState::Pending,
                            original_text: rendered,
                            model: correction.model().into(),
                            revised_text: None,
                            reason: None,
                            audio_ms: audio.as_ref().map(|a| a.len() as u64 * 1000 / 16000),
                            queue_ms: None,
                            processing_ms: None,
                        };
                        match audio {
                            Some(audio) if !audio.is_empty() => {
                                segment.correcting = true;
                                segment.finalized = false;
                                segment.review = Some(review);
                                if !correction.submit(&segment, audio) {
                                    // Retain the original text and record why this
                                    // sentence could not enter the bounded queue.
                                    segment.correcting = false;
                                    segment.finalized = true;
                                    let review = segment.review.as_mut().expect("review exists");
                                    review.state = ReviewState::Deferred;
                                    review.reason = Some("queue_full".into());
                                }
                            }
                            other => {
                                review.state = ReviewState::Deferred;
                                review.reason = Some(
                                    if other.is_none() {
                                        "utterance_too_long"
                                    } else {
                                        "empty_audio"
                                    }
                                    .into(),
                                );
                                segment.review = Some(review);
                            }
                        }
                    }
                    t.apply(segment.clone());
                    let _ = tx.send(AsrEvent::Segment(segment));
                    *previous = text;
                    if final_now {
                        *committed = true;
                    }
                }
                if final_now {
                    recognizer.reset(stream);
                    *id += 1;
                    *revision = 0;
                    previous.clear();
                    partial_punctuation.clear();
                    *utterance_audio = correction.as_ref().map(|_| Vec::new());
                }
            }
            Self::Offline {
                recognizer,
                vad,
                history,
                origin,
                id,
                end,
                committed,
                punctuation,
            } => {
                while let Some(speech) = vad.front() {
                    let (begin, samples) =
                        history.segment(speech.start().max(0) as u64, speech.samples());
                    if samples.is_empty() {
                        vad.pop();
                        continue;
                    }
                    let stream = recognizer.create_stream();
                    stream.accept_waveform(16000, &samples);
                    recognizer.decode(&stream);
                    let result = stream.get_result().context("离线识别没有返回结果")?;
                    let start = origin.unwrap_or(0) + begin * 1000 / 16000;
                    let stop = (start + samples.len() as u64 * 1000 / 16000).min(*end);
                    let text = punctuation.finish(result.text.trim());
                    if !text.is_empty() {
                        let mut extra = std::collections::BTreeMap::new();
                        if let Some(evidence) = raw_token_evidence(
                            &result.tokens,
                            result.timestamps.as_deref(),
                            result.durations.as_deref(),
                            start,
                            stop,
                            result.text.trim(),
                            1,
                        ) {
                            extra.insert("raw_token_timing".into(), evidence);
                        } else if let Some(evidence) = raw_duration_evidence(
                            &result.tokens,
                            result.durations.as_deref(),
                            result.text.trim(),
                        ) {
                            extra.insert("raw_token_duration".into(), evidence);
                        }
                        let mut segment = Segment {
                            extra,
                            id: *id,
                            revision: 1,
                            start_ms: start,
                            end_ms: stop.max(start),
                            text,
                            finalized: true,
                            correcting: false,
                            review: None,
                        };
                        bind_token_evidence(&mut segment, t.run.as_ref());
                        t.apply(segment.clone());
                        let _ = tx.send(AsrEvent::Segment(segment));
                        *committed = true;
                        *id += 1;
                    }
                    vad.pop();
                }
            }
        }
        Ok(())
    }
    fn boundary(&mut self, t: &mut Transcript, tx: &Sender<AsrEvent>) -> Result<()> {
        match self {
            Self::Online { stream, origin, .. } if origin.is_some() => {
                stream.accept_waveform(16000, &vec![0.0; 16000]);
                stream.input_finished();
            }
            Self::Offline { vad, .. } => vad.flush(),
            _ => return Ok(()),
        }
        self.decode(true, t, tx)?;
        match self {
            Self::Online {
                recognizer,
                stream,
                origin,
                ..
            } => {
                *stream = recognizer.create_stream();
                *origin = None;
            }
            Self::Offline {
                vad,
                history,
                origin,
                ..
            } => {
                vad.reset();
                history.reset();
                *origin = None;
            }
        }
        Ok(())
    }
    fn take_committed(&mut self) -> bool {
        let value = match self {
            Self::Online { committed, .. } | Self::Offline { committed, .. } => committed,
        };
        std::mem::take(value)
    }
    fn poll_corrections(&mut self, t: &mut Transcript, tx: &Sender<AsrEvent>) {
        if let Self::Online {
            correction: Some(correction),
            committed,
            ..
        } = self
        {
            *committed |= correction.poll(t, tx);
            if let Some(run) = &mut t.run {
                run.extra.insert(
                    "revision_effective".into(),
                    correction.health().code().into(),
                );
                if let Some(reason) = correction.failure() {
                    run.extra.insert("revision_failure".into(), reason.into());
                }
            }
        }
    }
    fn revision_state(&self) -> Option<RevisionRuntimeState> {
        match self {
            Self::Online {
                correction: Some(correction),
                ..
            } => Some(correction.health()),
            _ => None,
        }
    }
    fn revision_failure(&self) -> Option<String> {
        match self {
            Self::Online {
                correction: Some(correction),
                ..
            } => correction.failure(),
            _ => None,
        }
    }
    fn finish_corrections(&mut self, t: &mut Transcript, tx: &Sender<AsrEvent>) {
        if let Self::Online {
            correction: Some(correction),
            ..
        } = self
        {
            correction.finish(t, tx);
            if let Some(run) = &mut t.run {
                run.extra.insert(
                    "revision_effective".into(),
                    correction.health().code().into(),
                );
                if let Some(reason) = correction.failure() {
                    run.extra.insert("revision_failure".into(), reason.into());
                }
            }
        }
    }
}

#[cfg(test)]
mod live_integration_tests {
    use super::*;
    use crate::audio::decode_file;
    #[test]
    #[ignore = "requires installed models and a 20–30s continuous synthetic OPENCAP_LIVE_TEST_AUDIO"]
    fn streaming_updates_while_speaking_and_preserves_context_past_twenty_seconds() {
        let input = PathBuf::from(std::env::var_os("OPENCAP_LIVE_TEST_AUDIO").unwrap());
        let root = crate::model_root();
        assert!(ModelKind::Streaming.ready(&root) && ModelKind::Offline.ready(&root));
        let mut samples = Vec::new();
        decode_file(&input, |audio, _| {
            samples.extend(audio.samples.iter().copied());
            Ok(())
        })
        .unwrap();
        assert!((21 * 16000..30 * 16000).contains(&samples.len()));
        let end_ms = samples.len() as u64 * 1000 / 16000;
        let mut session = AsrSession::live_with_plan(
            root,
            String::new(),
            crate::LiveModelPlan {
                kind: ModelKind::Streaming,
                context_revision: true,
                revision_model: ModelKind::Offline,
            },
        )
        .unwrap();
        // Exclude model loading from steady-state speech latency.
        loop {
            match session
                .events
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
            {
                AsrEvent::Status(s) if s.starts_with("流式识别已就绪") => break,
                AsrEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        let sender = session.audio_sender();
        let began = Instant::now();
        let producer = thread::spawn(move || {
            for (i, chunk) in samples.chunks(1600).enumerate() {
                let deadline = began + Duration::from_millis(i as u64 * 100);
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                sender
                    .send(PcmAudio {
                        pts_us: i as i64 * 100_000,
                        sample_rate: 16000,
                        channels: 1,
                        source: opencap_core::AudioSource::ImportedMedia,
                        generation: 0,
                        samples: chunk.to_vec().into(),
                    })
                    .unwrap();
            }
        });
        let mut first_partial = None;
        let mut revisions = 0;
        let mut past_twenty = false;
        let expect_punctuation = crate::punctuation_ready(&crate::model_root());
        let mut partial_has_punctuation = false;
        let mut pending_correction = false;
        let mut visible = Transcript::default();
        // Keep input open throughout speech; finalization happens only afterwards.
        while began.elapsed() < Duration::from_millis(end_ms + 500) {
            match session.events.recv_timeout(Duration::from_millis(100)) {
                Ok(AsrEvent::Segment(s)) => {
                    assert_eq!(
                        s.id, 0,
                        "continuous speech was cut at a clock boundary: {s:?}"
                    );
                    if !s.finalized && !s.correcting {
                        first_partial.get_or_insert(began.elapsed());
                        revisions += 1;
                        past_twenty |= s.end_ms > 20_000;
                        partial_has_punctuation |=
                            s.text.chars().any(|c| "，。！？,.!?".contains(c));
                    }
                    pending_correction |= s.correcting;
                    visible.apply(s);
                }
                Ok(AsrEvent::Error(e)) => panic!("{e}"),
                _ => {}
            }
        }
        producer.join().unwrap();
        assert!(
            first_partial.is_some_and(|d| d < Duration::from_secs(4)),
            "first partial: {first_partial:?}"
        );
        assert!(
            revisions > 10 && past_twenty,
            "revisions={revisions}, past20={past_twenty}"
        );
        if expect_punctuation {
            assert!(
                partial_has_punctuation,
                "live text never received punctuation before the sentence ended"
            );
        }
        let native = visible.segments[0].text.clone();
        assert!(
            native.contains("预算") && native.contains("万元"),
            "boundary words missing: {native}"
        );
        session.finish_at(end_ms);
        loop {
            match session
                .events
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
            {
                AsrEvent::Segment(s) => {
                    pending_correction |= s.correcting;
                    visible.apply(s);
                }
                AsrEvent::Finished(t) => {
                    assert!(pending_correction);
                    assert!(t.complete && t.gaps.is_empty());
                    assert!(t.segments.iter().all(|s| s.finalized && !s.correcting));
                    assert_eq!(visible.segments, t.segments);
                    assert_eq!(t.segments.len(), 1);
                    eprintln!(
                        "first_partial_ms={} partial_revisions={revisions} native={native} final={}",
                        first_partial.unwrap().as_millis(),
                        t.text()
                    );
                    break;
                }
                AsrEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
    }
    #[test]
    #[ignore = "requires already installed local models and OPENCAP_LIVE_TEST_AUDIO (synthetic speech)"]
    fn installed_models_emit_live_segments_before_input_closes() {
        let input = PathBuf::from(
            std::env::var_os("OPENCAP_LIVE_TEST_AUDIO").expect("synthetic test audio required"),
        );
        let root = crate::model_root();
        assert!(
            ModelKind::Offline.ready(&root),
            "install models explicitly before this check"
        );
        let dir = std::env::temp_dir().join(format!(
            "opencap-live-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for kind in ModelKind::ALL {
            if !kind.ready(&root) {
                continue;
            }
            let mut session = AsrSession::live_with_plan(
                root.clone(),
                String::new(),
                crate::LiveModelPlan {
                    kind,
                    context_revision: kind.is_streaming(),
                    revision_model: if ModelKind::ParaformerSmall.ready(&root) {
                        ModelKind::ParaformerSmall
                    } else {
                        ModelKind::Offline
                    },
                },
            )
            .unwrap();
            let base = dir.join(format!("{kind:?}.m4a"));
            session.set_output(base.clone()).unwrap();
            let sender = session.audio_sender();
            let mut end_ms = 0;
            decode_file(&input, |audio, _| {
                end_ms = (audio.pts_us.max(0) as u64
                    + audio.samples.len() as u64 * 1_000_000
                        / audio.channels as u64
                        / audio.sample_rate as u64)
                    / 1000;
                sender.send(audio)?;
                Ok(())
            })
            .unwrap();
            drop(sender);
            // Own input remains open: a segment must be visible before EOF/finalization.
            let mut emitted = false;
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match session
                    .events
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                {
                    AsrEvent::Segment(s) if !s.text.is_empty() => {
                        emitted = true;
                        break;
                    }
                    AsrEvent::Error(e) => panic!("{kind:?}: {e}"),
                    _ => {}
                }
            }
            assert!(emitted, "{kind:?} did not emit live text");
            session.finish_at(end_ms);
            loop {
                match session
                    .events
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap()
                {
                    AsrEvent::Finished(t) => {
                        assert!(t.complete, "{t:?}");
                        assert!(!t.text().is_empty());
                        assert_eq!(t.model, kind.directory());
                        assert!(t.segments.iter().all(|s| s.finalized && !s.correcting));
                        assert!(
                            t.segments.iter().all(|s| {
                                s.text
                                    .chars()
                                    .last()
                                    .is_some_and(|c| "。，！？；：,.!?;:".contains(c))
                            }),
                            "{kind:?} omitted sentence punctuation: {:?}",
                            t.segments
                        );
                        if kind.is_streaming() {
                            let reviews: Vec<_> = t
                                .segments
                                .iter()
                                .filter_map(|segment| segment.review.as_ref())
                                .collect();
                            assert!(!reviews.is_empty(), "{kind:?} has no review provenance");
                            assert!(reviews.iter().all(|review| {
                                !review.original_text.is_empty()
                                    && !review.model.is_empty()
                                    && review.state != crate::ReviewState::Pending
                            }));
                            assert!(
                                reviews.iter().any(|review| {
                                    review.state == crate::ReviewState::Reviewed
                                        && review
                                            .revised_text
                                            .as_ref()
                                            .is_some_and(|text| !text.is_empty())
                                        && review.audio_ms.is_some()
                                        && review.queue_ms.is_some()
                                        && review.processing_ms.is_some()
                                }),
                                "{kind:?} did not complete a measured second pass: {reviews:?}"
                            );
                        }
                        assert!(base.with_extension("transcript.docx").exists());
                        break;
                    }
                    AsrEvent::Error(e) => panic!("{kind:?}: {e}"),
                    _ => {}
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
