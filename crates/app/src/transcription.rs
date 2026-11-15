use super::*;
use gpui::component::input::Input;
use opencap_asr::{AsrEvent, AsrSession, ModelKind, Transcript, model_root};

/// Read and validate without touching UI state, so callers can run it on a
/// worker and decide separately whether the result is still relevant.
struct LoadedTranscript {
    transcript: Transcript,
    sha256: String,
    size: u64,
    modified: std::time::SystemTime,
    identity: same_file::Handle,
}
struct TranscriptLoadPermit(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for TranscriptLoadPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
/// Compare serialized state incrementally without allocating a second copy.
/// A changed prefix stops serialization immediately.
pub(super) fn transcript_matches_snapshot(transcript: &Transcript, expected: &[u8]) -> bool {
    struct Compare<'a> {
        expected: &'a [u8],
        offset: usize,
    }
    impl std::io::Write for Compare<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let end = self
                .offset
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("状态长度溢出"))?;
            if self.expected.get(self.offset..end) != Some(bytes) {
                return Err(std::io::Error::other("状态已变化"));
            }
            self.offset = end;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut compare = Compare {
        expected,
        offset: 0,
    };
    serde_json::to_writer(&mut compare, transcript).is_ok() && compare.offset == expected.len()
}
impl LoadedTranscript {
    fn still_current(&self, source: &std::path::Path) -> bool {
        let Ok(metadata) = std::fs::metadata(source) else {
            return false;
        };
        if !metadata.is_file()
            || metadata.len() != self.size
            || metadata.modified().ok() != Some(self.modified)
        {
            return false;
        }
        opencap_core::open_regular_file_for_read(source)
            .and_then(|file| same_file::Handle::from_file(file).map_err(Into::into))
            .is_ok_and(|identity| identity == self.identity)
    }
}
fn read_transcript_file(
    source: &std::path::Path,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> anyhow::Result<LoadedTranscript> {
    use std::io::Read;
    const LIMIT: u64 = 64 * 1024 * 1024;
    let mut file = opencap_core::open_regular_file_for_read(source)?;
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file() && before.len() <= LIMIT,
        "逐字稿不是普通文件或超过 64 MiB"
    );
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    let mut input = (&mut file).take(LIMIT + 1);
    let mut buffer = [0u8; 128 * 1024];
    loop {
        anyhow::ensure!(
            !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
            "逐字稿读取已取消"
        );
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "逐字稿超过 64 MiB");
    let after = file.metadata()?;
    let current = std::fs::metadata(source)?;
    anyhow::ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && after.len() == current.len()
            && after.modified()? == current.modified()?,
        "逐字稿读取期间发生变化，请重试"
    );
    anyhow::ensure!(
        identity
            == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(source)?)?,
        "逐字稿读取期间被替换，请重试"
    );
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
        "逐字稿读取已取消"
    );
    let transcript = serde_json::from_slice(&bytes)?;
    let sha256 = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    };
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
        "逐字稿读取已取消"
    );
    Ok(LoadedTranscript {
        transcript,
        sha256,
        size: after.len(),
        modified: after.modified()?,
        identity,
    })
}

fn resolve_loaded_source(source: &std::path::Path, transcript: &mut Transcript) -> bool {
    if transcript
        .extra
        .get("history_snapshot_kind")
        .and_then(|value| value.as_str())
        == Some("manual_recovery_unbound")
    {
        return false;
    }
    let Some(run) = transcript.run.as_ref() else {
        return false;
    };
    let Some(asset_id) = run.input_asset_id.as_deref() else {
        return false;
    };
    let base = opencap_core::meeting_base(source);
    let resolved = opencap_core::read_manifest_snapshot(&base)
        .ok()
        .flatten()
        .and_then(|(manifest, fingerprint)| {
            if run.meeting_id.as_deref() != Some(manifest.meeting_id.as_str()) {
                return None;
            }
            let mut assets = manifest
                .assets
                .iter()
                .filter(|asset| asset.asset_id == asset_id);
            if assets
                .next()
                .is_none_or(|asset| !opencap_core::is_input_media_asset(asset))
                || assets.next().is_some()
            {
                return None;
            }
            let path = opencap_core::resolve_asset_in_manifest(&base, &manifest, asset_id)
                .ok()
                .flatten()?;
            let (_, after) = opencap_core::read_manifest_snapshot(&base).ok().flatten()?;
            (after == fingerprint).then_some(path)
        });
    let unverified = resolved.is_none();
    transcript.source = resolved;
    unverified
}

/// Reject concurrent changes before a retry replaces the primary transcript.
pub(super) fn verify_transcript_retry(
    base: &std::path::Path,
    pending: &Transcript,
    expected_sha256: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(manifest) = opencap_core::meeting_manifest(base)? {
        anyhow::ensure!(
            pending
                .run
                .as_ref()
                .and_then(|run| run.meeting_id.as_deref())
                == Some(manifest.meeting_id.as_str()),
            "逐字稿与会议清单身份不一致或未记录"
        );
    }
    let path = base.with_extension("transcript.json");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| anyhow::anyhow!("无法核对磁盘主稿，请另存恢复副本：{error}"))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "磁盘主稿不是普通文件"
    );
    let loaded = read_transcript_file(&path, None)?;
    anyhow::ensure!(loaded.still_current(&path), "磁盘主稿在核对期间发生变化");
    let expected = expected_sha256
        .ok_or_else(|| anyhow::anyhow!("缺少可靠的磁盘基线，请另存恢复副本后重新加载"))?;
    anyhow::ensure!(
        loaded.sha256 == expected,
        "磁盘主稿自上次读取或保存后发生变化"
    );
    let disk = &loaded.transcript;
    anyhow::ensure!(
        disk.run.as_ref().map(|run| run.run_id.as_str())
            == pending.run.as_ref().map(|run| run.run_id.as_str()),
        "磁盘主稿属于其他识别运行"
    );
    let mut segments = std::collections::BTreeMap::new();
    for segment in &pending.segments {
        anyhow::ensure!(
            segments.insert(segment.id, segment).is_none(),
            "内存逐字稿存在重复句段 ID"
        );
    }
    let mut disk_ids = std::collections::BTreeSet::new();
    for segment in &disk.segments {
        anyhow::ensure!(disk_ids.insert(segment.id), "磁盘逐字稿存在重复句段 ID");
        let current = segments
            .get(&segment.id)
            .ok_or_else(|| anyhow::anyhow!("磁盘主稿包含内存中不存在的句段"))?;
        anyhow::ensure!(
            segment.revision <= current.revision,
            "磁盘主稿包含更新的句段版本"
        );
        if segment.revision == current.revision && segment != *current {
            // A timing pass adds evidence without changing the text
            // revision. No other same-revision difference is allowed.
            let mut saved = segment.clone();
            let mut updated = (**current).clone();
            saved.extra.remove("word_alignment_v1");
            updated.extra.remove("word_alignment_v1");
            anyhow::ensure!(
                saved == updated
                    && opencap_asr::validated_word_alignment(current, pending.run.as_ref())
                        .is_some(),
                "同版本句段内容不同，不能覆盖磁盘主稿"
            );
        }
    }
    Ok(())
}

pub(super) fn saved_transcript_digest(transcript: &Transcript) -> Option<String> {
    use sha2::{Digest, Sha256};
    if transcript.primary_save_pending {
        return None;
    }
    serde_json::to_vec_pretty(transcript)
        .ok()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}

pub(super) enum PrepareEvent {
    Progress(String, Option<f64>),
    Done(ModelKind, anyhow::Result<PathBuf>),
    PunctuationDone(anyhow::Result<PathBuf>),
}

#[derive(Default)]
struct DownloadMeter(std::collections::VecDeque<(std::time::Instant, u64)>);
impl DownloadMeter {
    fn describe(&mut self, label: &str, bytes: u64, total: Option<u64>) -> (String, Option<f64>) {
        let now = std::time::Instant::now();
        if self.0.back().is_some_and(|(_, previous)| bytes < *previous) {
            self.0.clear();
        }
        self.0.push_back((now, bytes));
        while self.0.len() > 1
            && self
                .0
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at).as_secs_f64() > 5.)
        {
            self.0.pop_front();
        }
        let speed = self.0.front().and_then(|(at, first)| {
            let elapsed = now.duration_since(*at).as_secs_f64();
            (elapsed >= 1. && bytes > *first)
                .then(|| (bytes - *first) as f64 / elapsed / 1_000_000.)
        });
        let fraction = total
            .filter(|n| *n > 0 && bytes < *n)
            .map(|n| bytes as f64 / n as f64);
        let count = total.filter(|n| *n > 0).map_or_else(
            || format!("{:.1} MB", bytes as f64 / 1_000_000.),
            |n| {
                format!(
                    "{:.1} / {:.1} MB",
                    bytes as f64 / 1_000_000.,
                    n as f64 / 1_000_000.
                )
            },
        );
        let speed = speed
            .map(|value| format!(" · {value:.1} MB/s"))
            .unwrap_or_default();
        let stage = if total.is_some_and(|n| n > 0 && bytes >= n) {
            "下载完成，正在校验和准备"
        } else {
            "下载"
        };
        (format!("{stage}{label} · {count}{speed}"), fraction)
    }
}
pub(super) fn final_recording_path(path: &std::path::Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if let Some(stem) = name.strip_suffix(".partial.mp4") {
        path.with_file_name(format!("{stem}.mp4"))
    } else if let Some(stem) = name.strip_suffix(".partial.m4a") {
        path.with_file_name(format!("{stem}.m4a"))
    } else {
        path.to_path_buf()
    }
}

fn transcription_output(
    root: &std::path::Path,
    source: &std::path::Path,
    backfill: bool,
) -> anyhow::Result<PathBuf> {
    let original = opencap_core::meeting_base(source);
    let recording = source.with_extension("recording.json").exists();
    if recording && (backfill || !original.with_extension("transcript.json").exists()) {
        return Ok(original);
    }
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let label = if recording {
        format!("{stem}-模型重转")
    } else {
        stem.into_owned()
    };
    opencap_core::create_meeting_base(root, &label).map(|base| opencap_core::meeting_base(&base))
}

pub(super) fn transcript_history_files(
    base: &std::path::Path,
) -> anyhow::Result<(Vec<PathBuf>, std::collections::HashSet<PathBuf>)> {
    let parent = base.parent().unwrap_or(std::path::Path::new("."));
    let prefix = format!(
        "{}.",
        base.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut unavailable = std::collections::HashSet::new();
    let mut files = if let Some(manifest) = opencap_core::meeting_manifest(base)? {
        let mut id_counts = std::collections::HashMap::<String, usize>::new();
        let mut path_counts = std::collections::HashMap::<PathBuf, usize>::new();
        for asset in &manifest.assets {
            *id_counts.entry(asset.asset_id.clone()).or_default() += 1;
            if asset.ownership == opencap_core::AssetOwnership::Managed {
                *path_counts.entry(asset.path.clone()).or_default() += 1;
            }
        }
        manifest
            .assets
            .into_iter()
            .filter(|asset| {
                (asset.role == "transcript_history"
                    || asset
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.strip_prefix(&prefix))
                        .is_some_and(|suffix| {
                            suffix.starts_with("transcript.run-")
                                && suffix.ends_with(".json")
                                && asset.role == suffix
                        }))
                    && asset.ownership == opencap_core::AssetOwnership::Managed
                    && asset
                        .path
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
            })
            .map(|asset| {
                let path = parent.join(&asset.path);
                if asset.asset_id.is_empty()
                    || id_counts.get(&asset.asset_id) != Some(&1)
                    || path_counts.get(&asset.path) != Some(&1)
                    || opencap_core::checked_managed_asset_path(base, &asset.path).is_err()
                {
                    unavailable.insert(path.clone());
                }
                path
            })
            .collect::<Vec<_>>()
    } else {
        // Legacy histories predate the manifest; read only the exact archive naming convention.
        let prefix = format!(
            "{}.transcript.run-",
            base.file_name().unwrap_or_default().to_string_lossy()
        );
        std::fs::read_dir(parent)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with(&prefix)
                    && entry.file_name().to_string_lossy().ends_with(".json")
            })
            .map(|entry| entry.path())
            .filter(|path| {
                std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
            })
            .collect::<Vec<_>>()
    };
    files.sort_by_cached_key(|path| {
        (
            std::cmp::Reverse(
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
            ),
            path.clone(),
        )
    });
    files.dedup();
    unavailable.extend(files.iter().filter(|path| !path.is_file()).cloned());
    Ok((files, unavailable))
}

/// Verify a single history entry without rescanning or sorting the directory.
pub(super) fn verify_transcript_history_member(
    base: &std::path::Path,
    archive: &std::path::Path,
) -> anyhow::Result<Option<String>> {
    let parent = base
        .parent()
        .ok_or_else(|| anyhow::anyhow!("会议目录无效"))?;
    let relative = archive
        .strip_prefix(parent)
        .map_err(|_| anyhow::anyhow!("历史版本不在会议目录"))?;
    anyhow::ensure!(
        relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "历史版本相对路径无效"
    );
    let prefix = format!(
        "{}.transcript.run-",
        base.file_name().unwrap_or_default().to_string_lossy()
    );
    let legacy_role = relative
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(&prefix))
        .filter(|suffix| suffix.ends_with(".json"))
        .map(|suffix| format!("transcript.run-{suffix}"));
    if let Some(manifest) = opencap_core::meeting_manifest(base)? {
        opencap_core::checked_managed_asset_path(base, relative)?;
        let mut matches = manifest.assets.iter().filter(|asset| {
            asset.path == relative
                && asset.ownership == opencap_core::AssetOwnership::Managed
                && (asset.role == "transcript_history"
                    || legacy_role.as_deref() == Some(asset.role.as_str()))
        });
        let asset = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("历史版本未在当前会议清单登记"))?;
        anyhow::ensure!(
            matches.next().is_none()
                && !asset.asset_id.is_empty()
                && manifest
                    .assets
                    .iter()
                    .filter(|other| other.asset_id == asset.asset_id)
                    .take(2)
                    .count()
                    == 1
                && manifest
                    .assets
                    .iter()
                    .filter(
                        |other| other.ownership == opencap_core::AssetOwnership::Managed
                            && other.path == relative
                    )
                    .take(2)
                    .count()
                    == 1,
            "历史版本路径或资产编号不唯一"
        );
        anyhow::ensure!(
            archive.canonicalize()?.starts_with(parent.canonicalize()?),
            "历史版本指向会议目录之外"
        );
        Ok(Some(manifest.meeting_id))
    } else {
        anyhow::ensure!(
            relative.components().count() == 1
                && legacy_role.is_some()
                && std::fs::symlink_metadata(archive)?.file_type().is_file(),
            "旧历史版本不可访问或路径无效"
        );
        Ok(None)
    }
}
impl Recorder {
    fn invalidate_transcript_history(&mut self) {
        self.transcript_history_loaded_base = None;
        self.transcript_history_epoch = self.transcript_history_epoch.wrapping_add(1);
    }
    pub(super) fn sync_transcript_history(&mut self, cx: &mut Context<Self>) {
        let Some(base) = self.transcript_output.clone() else {
            self.transcript_history_loaded_base = None;
            self.transcript_history_files.clear();
            self.transcript_history_unavailable.clear();
            self.transcript_history_error = None;
            return;
        };
        if !self.transcript_tools_open {
            return;
        }
        if self.transcript_history_loaded_base.as_ref() == Some(&base)
            || self.transcript_history_loading_base.is_some()
        {
            return;
        }
        self.transcript_history_loading_base = Some(base.clone());
        let generation = self.transcript_load_generation;
        let epoch = self.transcript_history_epoch;
        self.transcript_history_files.clear();
        self.transcript_history_unavailable.clear();
        self.transcript_history_error = None;
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let result = cx
                .background_executor()
                .spawn(async move { transcript_history_files(&task_base) })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.transcript_history_loading_base = None;
                if this.transcript_output.as_ref() == Some(&base)
                    && this.transcript_load_generation == generation
                    && this.transcript_history_epoch == epoch
                {
                    this.transcript_history_loaded_base = Some(base);
                    match result {
                        Ok((files, unavailable)) => {
                            this.transcript_history_files = files;
                            this.transcript_history_unavailable = unavailable;
                            this.transcript_history_error = None;
                        }
                        Err(error) => {
                            this.transcript_history_files.clear();
                            this.transcript_history_unavailable.clear();
                            this.transcript_history_error = Some(format!("{error:#}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn retry_unstarted_file(
        &mut self,
        base: Option<PathBuf>,
        source: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.quit_pending
            || self.active()
            || self.transcription_busy()
            || self.transcript.primary_save_pending
            || self.transcript_output != base
            || self.transcript.source.as_ref() != Some(&source)
            || self.transcript.run.is_some()
            || !self.transcript.segments.is_empty()
            || self
                .asr
                .as_ref()
                .is_none_or(|asr| !asr.finished() || !asr.events.is_empty())
        {
            self.transcription_status = "任务或源文件已变化，请重新选择文件。".into();
            cx.notify();
            return;
        }
        self.historical_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.historical_cancelled.clone();
        let generation = self.transcript_load_generation;
        self.historical_checking = true;
        self.transcription_status = "正在后台检查源文件…".into();
        cx.notify();
        cx.spawn(async move |entity, cx| {
            let path = source.clone();
            let available = cx
                .background_executor()
                .spawn(async move { path.is_file() })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.historical_checking = false;
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    this.transcription_status = "已取消源文件检查。".into();
                } else if !available {
                    this.transcription_status = "源文件不可用，请重新选择。".into();
                } else if this.quit_pending
                    || this.active()
                    || this.transcription_busy()
                    || this.transcript_load_generation != generation
                    || this.transcript_output != base
                    || this.transcript.source.as_ref() != Some(&source)
                    || this.transcript.primary_save_pending
                    || this.transcript.run.is_some()
                    || !this.transcript.segments.is_empty()
                    || this
                        .asr
                        .as_ref()
                        .is_none_or(|asr| !asr.finished() || !asr.events.is_empty())
                {
                    this.transcription_status = "任务或源文件已变化，请重新选择文件。".into();
                } else {
                    #[cfg(feature = "online-speaker")]
                    let speaker_waiting = this.config.online_speakers
                        && this
                            .online_speakers
                            .as_ref()
                            .is_some_and(|controller| !controller.can_replace());
                    #[cfg(not(feature = "online-speaker"))]
                    let speaker_waiting = false;
                    if speaker_waiting {
                        this.transcription_status =
                            "在线说话人结果仍在收尾或保存，请完成后重试。".into();
                    } else {
                        this.persist_notes(cx);
                        if this.notes_have_unsaved_work(cx)
                            || this.persist_minutes_draft(cx)
                            || this
                                .live_notes
                                .as_ref()
                                .is_some_and(|notes| notes.has_unsaved_work())
                        {
                            this.transcription_status =
                                "笔记或纪要草稿尚未保存，未开始重试。".into();
                        } else {
                            this.start_file_transcription(source, false, false);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn start_historical_check(
        &mut self,
        base: PathBuf,
        source: PathBuf,
        run: opencap_asr::TranscriptionRun,
        cx: &mut Context<Self>,
    ) {
        if self.transcript.primary_save_pending {
            self.transcription_status = "当前逐字稿尚未保存，请先重试同步。".into();
            cx.notify();
            return;
        }
        if self.quit_pending || self.active() || self.transcription_busy() {
            self.transcription_status = "请在当前录制和识别任务结束后再重转。".into();
            cx.notify();
            return;
        }
        if let Err(error) = opencap_asr::validate_historical_run(&run) {
            self.transcription_status = format!("无法使用历史配置：{error:#}");
            cx.notify();
            return;
        }
        let baseline = match serde_json::to_vec(&self.transcript) {
            Ok(value) => value,
            Err(error) => {
                self.transcription_status = format!("无法核对当前逐字稿：{error}");
                cx.notify();
                return;
            }
        };
        let media_unverified = !run.extra.contains_key("input_media_fingerprint")
            || run
                .extra
                .get("input_integrity")
                .and_then(|value| value.as_str())
                != Some("hash_matched_before_and_after")
            || run
                .extra
                .get("input_audio")
                .and_then(|value| value.get("audio_stream_index"))
                .and_then(|value| value.as_u64())
                .is_none();
        self.historical_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.historical_cancelled.clone();
        self.historical_checking = true;
        self.transcription_status = if media_unverified {
            "正在核对历史模型；旧运行的媒体完整性或音轨未完整记录，无法确认输入完全一致。"
        } else {
            "正在后台核对历史模型与媒体指纹…"
        }
        .into();
        let title = if self.meeting_title.is_empty() {
            self.transcript.title()
        } else {
            self.meeting_title.clone()
        };
        cx.notify();
        cx.spawn(async move |entity, cx| {
            let task_source = source.clone();
            let task_base = base.clone();
            let task_run = run.clone();
            let task_cancel = cancelled.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let check_registration = || -> anyhow::Result<()> {
                        if let Some(id) = task_run.input_asset_id.as_deref() {
                            let manifest = opencap_core::meeting_manifest(&task_base)?
                                .ok_or_else(|| anyhow::anyhow!("会议资产清单不可用"))?;
                            anyhow::ensure!(
                                task_run.meeting_id.as_deref()
                                    == Some(manifest.meeting_id.as_str()),
                                "历史运行与会议身份不一致"
                            );
                            let mut assets =
                                manifest.assets.iter().filter(|asset| asset.asset_id == id);
                            anyhow::ensure!(
                                assets
                                    .next()
                                    .is_some_and(opencap_core::is_input_media_asset)
                                    && assets.next().is_none(),
                                "历史媒体资产角色无效或编号不唯一"
                            );
                            anyhow::ensure!(
                                opencap_core::resolve_asset_in_manifest(&task_base, &manifest, id)?
                                    .as_ref()
                                    == Some(&task_source),
                                "历史媒体登记已变化，请重新选择版本"
                            );
                        }
                        Ok(())
                    };
                    check_registration()?;
                    opencap_asr::verify_historical_inputs(
                        &model_root(),
                        &task_source,
                        &task_run,
                        &task_cancel,
                    )?;
                    check_registration()
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.historical_checking = false;
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    this.transcription_status = "已取消历史配置核对，原会议未更改。".into();
                    cx.notify();
                    return;
                }
                if let Err(error) = result {
                    this.transcription_status = format!("未开始历史配置重转：{error:#}");
                    cx.notify();
                    return;
                }
                if this.quit_pending
                    || this.transcript_output.as_ref() != Some(&base)
                    || this.active()
                    || this.transcription_busy()
                    || this.transcript.primary_save_pending
                    || !transcript_matches_snapshot(&this.transcript, &baseline)
                {
                    this.transcription_status =
                        "会议或任务状态已变化，请重新发起历史配置重转。".into();
                    cx.notify();
                    return;
                }
                this.persist_notes(cx);
                let notes_unsaved = this.notes_have_unsaved_work(cx);
                if notes_unsaved
                    || this.persist_minutes_draft(cx)
                    || this
                        .live_notes
                        .as_ref()
                        .is_some_and(|notes| notes.has_unsaved_work())
                {
                    this.transcription_status = "当前笔记或纪要草稿尚未保存，未开始重转。".into();
                    cx.notify();
                    return;
                }
                let output = match opencap_core::create_meeting_base(
                    &this.config.output_dir,
                    &format!("{title}-历史配置重转"),
                ) {
                    Ok(path) => opencap_core::meeting_base(&path),
                    Err(error) => {
                        this.transcription_status = format!("无法创建重转副本：{error:#}");
                        cx.notify();
                        return;
                    }
                };
                match AsrSession::file_from_historical_run(
                    model_root(),
                    source,
                    output.clone(),
                    &run,
                ) {
                    Ok(asr) => {
                        this.stash_live_notes_before_switch();
                        #[cfg(feature = "online-speaker")]
                        {
                            this.backfill_speaker_history = None;
                        }
                        this.asr = Some(asr);
                        this.asr_live = false;
                        this.revision_runtime = None;
                        this.revision_failure = None;
                        this.transcription_clock.start();
                        this.speaker_clock = task::TaskClock::default();
                        this.transcript = Transcript::default();
                        this.transcript_disk_baseline = None;
                        this.transcript_dirty = true;
                        this.transcript_new_document = true;
                        this.transcript_history_page = 0;
                        this.invalidate_transcript_history();
                        this.text_document = None;
                        this.transcript_output = Some(output);
                        this.reset_minutes();
                        this.transcription_progress = 0.0;
                        this.transcription_tab = true;
                        this.settings = false;
                        this.meeting_page = workspace::MeetingPage::Transcript;
                        this.transcription_status = if media_unverified {
                            "正在另存重转；历史媒体完整性或音轨未确认，文件输入与实时流也可能不同。"
                        } else {
                            "正在按已核对的历史模型参数另存重转；输入处理可能与原运行不同。"
                        }
                        .into();
                    }
                    Err(error) => {
                        this.transcription_status =
                            format!("无法启动重转，已创建的空会议目录保留：{error:#}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_historical_archive(
        &mut self,
        base: PathBuf,
        archive: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.quit_pending || self.active() || self.transcription_busy() {
            return;
        }
        self.historical_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.historical_cancelled.clone();
        self.historical_checking = true;
        self.transcription_status = "正在读取历史识别版本…".into();
        cx.notify();
        cx.spawn(async move |entity, cx| {
            let task_archive = archive.clone();
            let task_base = base.clone();
            let task_cancel = cancelled.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let meeting_id = verify_transcript_history_member(&task_base, &task_archive)?;
                    let loaded = read_transcript_file(&task_archive, Some(&task_cancel))?;
                    anyhow::ensure!(
                        verify_transcript_history_member(&task_base, &task_archive)? == meeting_id,
                        "读取期间历史版本的会议身份发生变化"
                    );
                    if let Some(meeting_id) = meeting_id {
                        anyhow::ensure!(
                            loaded
                                .transcript
                                .run
                                .as_ref()
                                .and_then(|run| run.meeting_id.as_deref())
                                == Some(meeting_id.as_str()),
                            "历史版本的会议身份缺失或不匹配"
                        );
                    }
                    let run = loaded
                        .transcript
                        .run
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("历史版本没有识别配置"))?;
                    let source = match run.input_asset_id.as_deref() {
                        Some(id) => {
                            let manifest = opencap_core::meeting_manifest(&task_base)?
                                .ok_or_else(|| anyhow::anyhow!("历史版本的资产清单不可用"))?;
                            anyhow::ensure!(
                                run.meeting_id.as_deref() == Some(manifest.meeting_id.as_str()),
                                "历史版本与媒体清单的会议身份不匹配"
                            );
                            let mut assets =
                                manifest.assets.iter().filter(|asset| asset.asset_id == id);
                            anyhow::ensure!(
                                assets
                                    .next()
                                    .is_some_and(opencap_core::is_input_media_asset)
                                    && assets.next().is_none(),
                                "历史媒体资产角色无效或编号不唯一"
                            );
                            Some(
                                opencap_core::resolve_asset_in_manifest(&task_base, &manifest, id)?
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "历史版本记录的媒体资产无法唯一解析，请先修复或重定位"
                                        )
                                    })?,
                            )
                        }
                        None => loaded.transcript.source.clone(),
                    };
                    Ok::<_, anyhow::Error>((loaded, source))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.historical_checking = false;
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    this.transcription_status = "已取消读取历史版本。".into();
                    cx.notify();
                    return;
                }
                if this.quit_pending {
                    this.transcription_status = "正在退出，未开始历史配置核对。".into();
                    cx.notify();
                    return;
                }
                if this.transcript_output.as_ref() != Some(&base) {
                    this.transcription_status = "已切换会议，请重新选择历史版本。".into();
                    cx.notify();
                    return;
                }
                match result {
                    Ok((loaded, source)) => {
                        if !loaded.still_current(&archive) {
                            this.transcription_status = "历史版本读取后发生变化，请重试。".into();
                            cx.notify();
                            return;
                        }
                        let Some(run) = loaded.transcript.run else {
                            this.transcription_status = "历史版本没有识别配置。".into();
                            cx.notify();
                            return;
                        };
                        if let Some(source) = source {
                            this.start_historical_check(base, source, run, cx);
                        } else {
                            this.transcription_status = "历史版本没有记录原媒体。".into();
                            cx.notify();
                        }
                    }
                    Err(error) => {
                        this.transcription_status = format!("无法读取历史版本：{error:#}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
    #[cfg(feature = "online-speaker")]
    fn online_speaker_caption(&self) -> Option<String> {
        (!self.online_speaker_status.is_empty()).then(|| self.online_speaker_status.clone())
    }
    #[cfg(not(feature = "online-speaker"))]
    fn online_speaker_caption(&self) -> Option<String> {
        None
    }
    pub(super) fn select_sentence_review(
        &mut self,
        selection: &super::transcript_view::ReviewSelection,
        cx: &mut Context<Self>,
    ) {
        if self.active() || self.transcription_busy() {
            self.transcription_status = "请在录制和识别结束后切换句子版本".into();
        } else if self.transcript.correction.is_some() {
            self.transcription_status = "请先恢复 AI 校正前的逐字稿，再切换句子版本".into();
        } else if self
            .transcript
            .select_sentence_review(selection.id, selection.original)
        {
            self.transcript_dirty = true;
            if self.transcript_output.is_some() {
                self.transcript.primary_save_pending = true;
                self.retry_transcript_sync();
            } else {
                self.transcription_status = "句子版本已切换".into();
            }
        }
        cx.notify();
    }

    /// Recognition, correction or speaker edits are changing the visible
    /// transcript. These must stop a second writer and block version switches.
    pub(super) fn transcript_write_busy(&self) -> bool {
        self.asr
            .as_ref()
            .is_some_and(|a| !a.finished() || !a.events.is_empty())
            || self.transcript_sync.is_some()
            || self.speaker_task.is_some()
            || self.ai.correction_task.is_some()
            || self.voice_busy
    }
    /// A minutes request/reload is working on this meeting's minutes files.
    /// It does not block recognizing or browsing the transcript.
    pub(super) fn minutes_task_busy(&self) -> bool {
        self.summary_task.is_some()
            || self.ai.document_task.is_some()
            || self.ai.minutes_reload_busy
            || self.ai.draft_recovery_busy
    }
    /// Model download or preparation; confined to the model directory and
    /// therefore independent of meeting content and playback.
    pub(super) fn model_prepare_busy(&self) -> bool {
        self.preparing_model.is_some()
    }
    /// Any long task that reads or rewrites files of the current meeting.
    pub(super) fn meeting_task_busy(&self) -> bool {
        self.transcript_write_busy()
            || self.minutes_task_busy()
            || self.historical_checking
            || self.relink_busy
            || self.ai.credentials_busy
            || self.notes_recovery_busy
            || self.notes_reload_busy
    }
    /// Sum of every operation that currently has a UI-visible task. Callers
    /// that only need one concern should use the narrower predicates above.
    pub(super) fn transcription_busy(&self) -> bool {
        self.meeting_task_busy() || self.model_prepare_busy()
    }
    pub(super) fn start_live_asr(&mut self) -> Option<crossbeam_channel::Sender<PcmAudio>> {
        if self.transcript.primary_save_pending {
            self.transcription_status = "当前逐字稿尚未保存；录制可继续，实时转写暂不可用。".into();
            return None;
        }
        #[cfg(feature = "online-speaker")]
        self.cancel_word_timing_for_document_change();
        self.stash_live_notes_before_switch();
        self.text_document = None;
        self.revision_runtime = None;
        self.revision_failure = None;
        // Start a new workspace only after recording preflight has succeeded.
        // A recording without ASR must not show the preceding meeting's words.
        self.transcript = Transcript::default();
        self.transcript_disk_baseline = None;
        self.transcript_dirty = true;
        self.transcript_new_document = true;
        self.transcript_output = None;
        self.transcription_clock = task::TaskClock::default();
        self.speaker_clock = task::TaskClock::default();
        self.transcription_progress = 0.;
        self.asr_processed_ms = None;
        self.reset_minutes();
        if !self.config.realtime_transcription {
            self.transcription_status = "本次未开启实时转录；录制保存后可在会议文件中补转".into();
            return None;
        }
        self.asr_models = opencap_asr::ModelAvailability::inspect(&model_root());
        let Some(plan) = self.asr_models.live_plan_for(
            self.config.live_asr_model,
            self.config.offline_asr_model,
            self.config.transcription_context_revision,
        ) else {
            self.transcription_status =
                "所选实时模型未安装；请在设置 → 识别模型下载。录制文件会正常保存".into();
            return None;
        };
        match AsrSession::live_with_plan_request(
            model_root(),
            self.config.transcription_hotwords.clone(),
            plan,
            self.config.transcription_language,
            self.config.transcription_punctuation,
            self.config.transcription_context_revision,
        ) {
            Ok(asr) => {
                let sender = asr.audio_sender();
                self.transcription_clock.start();
                self.asr = Some(asr);
                self.asr_live = true;
                self.transcription_status = format!("正在启动：{}", plan.description());
                Some(sender)
            }
            Err(e) => {
                self.transcription_status = format!("识别启动失败；录制继续：{e:#}");
                None
            }
        }
    }
    pub(super) fn prepare_asr_model(&mut self, kind: ModelKind) {
        // Downloads are independent of viewing, playback and AI minutes, but
        // they share the progress display with a transcript writer.
        if self.model_prepare_busy() || self.transcript_write_busy() || self.active() {
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        self.transcription_clock.start();
        self.transcription_progress = 0.;
        self.preparing_model = Some(rx);
        self.prepare_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.prepare_cancelled.clone();
        self.transcription_status = format!("正在下载{}…", kind.label());
        std::thread::spawn(move || {
            let mut meter = DownloadMeter::default();
            let result = opencap_asr::prepare_cancellable(
                kind,
                &model_root(),
                &cancelled,
                |bytes, total| {
                    let (text, fraction) = meter.describe(kind.label(), bytes, total);
                    let _ = tx.send(PrepareEvent::Progress(text, fraction));
                },
            );
            let _ = tx.send(PrepareEvent::Done(kind, result));
        });
    }
    pub(super) fn prepare_punctuation_model(&mut self) {
        if self.model_prepare_busy() || self.transcript_write_busy() || self.active() {
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        self.transcription_clock.start();
        self.transcription_progress = 0.;
        self.preparing_model = Some(rx);
        self.prepare_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.prepare_cancelled.clone();
        self.transcription_status = "正在下载中英标点模型…".into();
        std::thread::spawn(move || {
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut meter = DownloadMeter::default();
            let result = opencap_asr::prepare_punctuation_cancellable(
                &model_root(),
                &cancelled,
                |bytes, total| {
                    if last.elapsed() >= std::time::Duration::from_millis(300)
                        || total == Some(bytes)
                    {
                        let (message, fraction) = meter.describe("中英标点模型", bytes, total);
                        let _ = tx.send(PrepareEvent::Progress(message, fraction));
                        last = std::time::Instant::now();
                    }
                },
            );
            let _ = tx.send(PrepareEvent::PunctuationDone(result));
        });
    }
    pub(super) fn choose_transcription_file(&mut self, realtime: bool, cx: &mut Context<Self>) {
        // Preparing a model download does not conflict with importing a file;
        // any task that touches the current meeting's files does.
        if self.active() || self.meeting_task_busy() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择音频或视频文件".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(path) = paths.first()
            {
                let path = path.clone();
                let _ = entity.update(cx, |this, cx| {
                    if this.quit_pending || this.active() || this.meeting_task_busy() {
                        this.transcription_status =
                            "录制、退出或其他会议任务正在进行，未开始文件转写。".into();
                    } else {
                        this.persist_notes(cx);
                        if this.notes_have_unsaved_work(cx)
                            || this.persist_minutes_draft(cx)
                            || this
                                .live_notes
                                .as_ref()
                                .is_some_and(|notes| notes.has_unsaved_work())
                        {
                            this.transcription_status =
                                "笔记或纪要仍有未保存内容，未切换会议。".into();
                        } else if path.file_name().is_some_and(|name| {
                            name.to_string_lossy().ends_with(".transcript.json")
                        }) {
                            this.load_transcript_async(path, cx, false, |_, _| {});
                        } else {
                            this.start_file_transcription(path, realtime, false);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    #[cfg(feature = "online-speaker")]
    fn choose_word_timing_model(&mut self, cx: &mut Context<Self>) {
        if self.quit_pending
            || self.active()
            || self.transcription_busy()
            || self.transcript.primary_save_pending
            || self.word_timing_cancel.is_some()
        {
            return;
        }
        let (Some(base), Some(source), Some(library)) = (
            self.transcript_output.clone(),
            self.transcript.source.clone(),
            self.config.online_speaker_library.clone(),
        ) else {
            self.word_timing_status =
                "需要可用的原媒体，并在识别模型设置中选择可信的 NeMo 原生库。".into();
            cx.notify();
            return;
        };
        if !self.transcript.is_complete()
            || self.transcript.run.is_none()
            || self
                .transcript_disk_baseline
                .as_ref()
                .is_none_or(|(saved, _)| saved != &base)
        {
            self.word_timing_status = "需要已完成并保存的文件识别运行，且原媒体身份可核对。".into();
            cx.notify();
            return;
        }
        let baseline = match serde_json::to_vec(&self.transcript) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.word_timing_status = format!("无法检查逐字稿：{error}");
                cx.notify();
                return;
            }
        };
        let disk_baseline = self.transcript_disk_baseline.clone();
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择 Nemotron 3.5 ASR Streaming 固定版本模型".into()),
        });
        cx.spawn(async move |entity, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else { return; };
            let Some(model) = paths.into_iter().next() else { return; };
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let started = entity.update(cx, |this, cx| {
                if this.quit_pending || this.active() || this.transcription_busy()
                    || this.transcript.primary_save_pending || this.word_timing_cancel.is_some()
                    || this.transcript_output.as_ref() != Some(&base)
                    || this.transcript.source.as_ref() != Some(&source)
                    || this.config.online_speaker_library.as_ref() != Some(&library)
                    || this.transcript_disk_baseline != disk_baseline
                    || !transcript_matches_snapshot(&this.transcript, &baseline) {
                    this.word_timing_status = "会议、原媒体或任务状态已变化，请重新选择。".into();
                    cx.notify();
                    return None;
                }
                this.word_timing_cancel = Some(cancelled.clone());
                this.word_timing_status = "正在核对原媒体并生成模型词时间；长录音可能需要较长时间。".into();
                cx.notify();
                Some(this.transcript.clone())
            }).ok().flatten();
            let Some(transcript) = started else { return; };
            let task_base = base.clone();
            let task_cancelled = cancelled.clone();
            let result = cx.background_executor().spawn(async move {
                // The user explicitly selected this trusted native library.
                unsafe { opencap_asr::generate_nemo_word_timings(
                    &task_base, &transcript, &library, &model, &task_cancelled) }
            }).await;
            let _ = entity.update(cx, |this, cx| {
                if this.word_timing_cancel.as_ref().is_none_or(|current|
                    !std::sync::Arc::ptr_eq(current, &cancelled)) { return; }
                this.word_timing_cancel = None;
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    this.word_timing_status = "词时间任务已停止；逐字稿未修改。".into();
                } else if this.quit_pending || this.active() || this.transcription_busy()
                    || this.transcript.primary_save_pending || this.transcript_output.as_ref() != Some(&base)
                    || this.transcript.source.as_ref() != Some(&source)
                    || this.transcript_disk_baseline != disk_baseline
                    || !transcript_matches_snapshot(&this.transcript, &baseline) {
                    this.word_timing_status = "会议或逐字稿在生成期间发生变化；未应用旧结果。".into();
                } else {
                    match result {
                        Ok(alignments) if alignments.is_empty() => {
                            this.word_timing_status = "模型词与当前句段未能完整匹配；继续使用句段回放。".into();
                        }
                        Ok(alignments) => {
                            let prepared: anyhow::Result<Vec<_>> = alignments.into_iter().map(|alignment|
                                Ok((alignment.segment_id, serde_json::to_value(alignment)?))).collect();
                            match prepared {
                                Ok(prepared) => {
                                    let count = prepared.len();
                                    for (id, value) in prepared {
                                        if let Some(segment) = this.transcript.segments.iter_mut().find(|segment| segment.id == id) {
                                            segment.extra.insert("word_alignment_v1".into(), value);
                                        }
                                    }
                                    this.transcript_dirty = true;
                                    this.transcript.primary_save_pending = true;
                                    this.word_timing_status = format!("已为 {count} 段生成模型词时间，保存状态见下方；其他句段仍按句回放。词时间精度尚未验收。");
                                    this.retry_transcript_sync();
                                }
                                Err(error) => this.word_timing_status = format!("词时间结果无法保存：{error:#}"),
                            }
                        }
                        Err(error) => this.word_timing_status = format!("词时间未生成：{error:#}；逐字稿未修改。"),
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    #[cfg(feature = "online-speaker")]
    fn cancel_word_timing_for_document_change(&mut self) {
        if let Some(cancelled) = &self.word_timing_cancel {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            self.word_timing_status = "会议已切换，正在停止上一轮词时间任务。".into();
        }
    }
    pub(super) fn load_transcript_file(&mut self, source: &std::path::Path) -> bool {
        if self.transcript.primary_save_pending {
            self.transcription_status = "当前逐字稿尚未保存，请先重试同步。".into();
            return false;
        }
        if let Some(cancel) = self.transcript_load_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
        let mut loaded = match read_transcript_file(source, None) {
            Ok(loaded) if loaded.still_current(source) => loaded,
            Ok(_) => {
                self.transcription_status = "逐字稿读取后发生变化，请重试。".into();
                return false;
            }
            Err(error) => {
                self.transcription_status = format!("无法载入转录稿：{error:#}");
                return false;
            }
        };
        let source_unverified = resolve_loaded_source(source, &mut loaded.transcript);
        self.apply_loaded_transcript(
            source,
            loaded.transcript,
            Some(loaded.sha256),
            source_unverified,
        );
        true
    }
    pub(super) fn apply_loaded_transcript(
        &mut self,
        source: &std::path::Path,
        mut transcript: Transcript,
        disk_sha256: Option<String>,
        source_unverified: bool,
    ) {
        #[cfg(feature = "online-speaker")]
        self.cancel_word_timing_for_document_change();
        if let Some(cancel) = self.transcript_load_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
        self.stash_live_notes_before_switch();
        self.text_document = None;
        self.transcript_history_page = 0;
        self.invalidate_transcript_history();
        let unbound_recovery = transcript
            .extra
            .get("history_snapshot_kind")
            .and_then(|value| value.as_str())
            == Some("manual_recovery_unbound");
        // A user-selected recovery filename is not a meeting identity. Do
        // not resolve assets, restore a speaker checkpoint, or write sidecars
        // under that basename just because a neighboring manifest exists.
        self.transcript_output = (!unbound_recovery).then(|| opencap_core::meeting_base(source));
        self.transcript_disk_baseline = self
            .transcript_output
            .as_ref()
            .zip(disk_sha256)
            .map(|(base, digest)| (base.clone(), digest));
        #[cfg(feature = "online-speaker")]
        if let Some(base) = self.transcript_output.clone() {
            self.restore_online_for_transcript(&base, &mut transcript);
        }
        self.transcript = transcript;
        self.transcript_dirty = true;
        self.transcript_new_document = true;
        self.transcription_clock = task::TaskClock::default();
        self.speaker_clock = task::TaskClock::default();
        self.reset_minutes();
        self.transcription_tab = true;
        self.settings = false;
        self.load_minutes();
        self.meeting_page = workspace::MeetingPage::Transcript;
        self.remember_meeting();
        self.transcription_status = if source_unverified {
            "已载入逐字稿；媒体身份未能确认，未自动关联原媒体，请检查会议清单。"
        } else if unbound_recovery {
            "已打开未绑定会议的转写恢复副本；内容可查看和复制，未自动关联会议资产。"
        } else if self
            .transcript
            .run
            .as_ref()
            .is_some_and(|run| run.state == "running")
        {
            "已载入识别检查点；记录状态为运行中，是否仍有其他进程识别尚未核实，未定稿文字需核对。"
        } else if !self.transcript.is_complete() {
            "已载入未完整完成的逐字稿；暂存文字需核对，原媒体可用于重转。"
        } else {
            "已载入转录、纪要版本与对话，可继续修改"
        }
        .into();
    }
    pub(super) fn load_transcript_async_then<F>(
        &mut self,
        source: PathBuf,
        cx: &mut Context<Self>,
        after: F,
    ) where
        F: FnOnce(&mut Self, &mut Context<Self>) + 'static,
    {
        self.load_transcript_async(source, cx, true, after);
    }
    fn load_transcript_async<F>(
        &mut self,
        source: PathBuf,
        cx: &mut Context<Self>,
        require_selected_target: bool,
        after: F,
    ) where
        F: FnOnce(&mut Self, &mut Context<Self>) + 'static,
    {
        if self.transcript.primary_save_pending {
            self.transcription_status = "当前逐字稿尚未保存，请先重试同步。".into();
            if self.meeting_page == workspace::MeetingPage::Library {
                self.message = self.transcription_status.clone();
            }
            cx.notify();
            return;
        }
        if let Some(cancel) = self.transcript_load_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
        let generation = self.transcript_load_generation;
        let workers = self.transcript_load_workers.clone();
        if workers
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count < 2).then_some(count + 1),
            )
            .is_err()
        {
            self.transcription_status = "先前的逐字稿读取尚未结束，请稍后重试。".into();
            if self.meeting_page == workspace::MeetingPage::Library {
                self.message = self.transcription_status.clone();
            }
            cx.notify();
            return;
        }
        let permit = TranscriptLoadPermit(workers);
        let baseline = match serde_json::to_vec(&self.transcript) {
            Ok(value) => value,
            Err(error) => {
                self.transcription_status = format!("无法保存当前界面状态用于加载校验：{error}");
                if self.meeting_page == workspace::MeetingPage::Library {
                    self.message = self.transcription_status.clone();
                }
                cx.notify();
                return;
            }
        };
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.transcript_load_cancel = Some(cancelled.clone());
        let original_base = self.transcript_output.clone();
        let target_base = opencap_core::meeting_base(&source);
        let original_selection = self.meeting_selected_base.clone();
        let original_page = self.meeting_page;
        let original_list_page = self.meeting_list_page;
        let original_query = self.meeting_search_query.clone();
        let original_filters = self.meeting_search_filters.clone();
        let original_semantic = self.semantic_enabled;
        self.transcription_status = "正在后台读取逐字稿…".into();
        cx.spawn(async move |entity, cx| {
            let task_source = source.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _permit = permit;
                    let mut loaded = read_transcript_file(&task_source, Some(&cancelled))?;
                    let source_unverified =
                        resolve_loaded_source(&task_source, &mut loaded.transcript);
                    Ok::<_, anyhow::Error>((loaded, source_unverified))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.transcript_load_generation != generation {
                    return;
                }
                this.transcript_load_cancel = None;
                if this.quit_pending
                    || this.active()
                    || this.transcription_busy()
                    || this.transcript.primary_save_pending
                    || this.transcript_output != original_base
                    || this.meeting_selected_base != original_selection
                    || (require_selected_target
                        && this
                            .meeting_selected_base
                            .as_ref()
                            .is_some_and(|base| base != &target_base))
                    || this.meeting_page != original_page
                    || (require_selected_target
                        && original_page == workspace::MeetingPage::Library
                        && this.meeting_list_page != original_list_page)
                    || this.meeting_search_query != original_query
                    || this.meeting_search_filters != original_filters
                    || this.semantic_enabled != original_semantic
                    || !transcript_matches_snapshot(&this.transcript, &baseline)
                {
                    this.transcription_status =
                        "会议或逐字稿在加载期间发生变化；未覆盖当前内容。".into();
                } else {
                    match result {
                        Ok((loaded, source_unverified)) => {
                            this.persist_notes(cx);
                            let notes_unsaved = this.notes_have_unsaved_work(cx);
                            if notes_unsaved
                                || this.persist_minutes_draft(cx)
                                || this
                                    .live_notes
                                    .as_ref()
                                    .is_some_and(|notes| notes.has_unsaved_work())
                            {
                                this.transcription_status =
                                    "当前笔记或纪要草稿尚未保存，未切换会议。".into();
                            } else if !loaded.still_current(&source) {
                                this.transcription_status =
                                    "逐字稿读取后发生变化，未切换会议；请重试。".into();
                            } else {
                                this.apply_loaded_transcript(
                                    &source,
                                    loaded.transcript,
                                    Some(loaded.sha256),
                                    source_unverified,
                                );
                                after(this, cx);
                            }
                        }
                        Err(error) => {
                            this.transcription_status = format!("无法载入转录稿：{error:#}")
                        }
                    }
                }
                if this.meeting_page == workspace::MeetingPage::Library {
                    this.message = this.transcription_status.clone();
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn start_file_transcription(
        &mut self,
        source: PathBuf,
        realtime: bool,
        backfill: bool,
    ) {
        self.start_file_transcription_with_archive(source, realtime, backfill, None);
    }
    pub(super) fn start_file_transcription_guarded(
        &mut self,
        source: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.persist_notes(cx);
        if self.notes_have_unsaved_work(cx)
            || self.persist_minutes_draft(cx)
            || self
                .live_notes
                .as_ref()
                .is_some_and(|notes| notes.has_unsaved_work())
        {
            self.transcription_status = "笔记或纪要仍有未保存内容，未切换会议。".into();
            return;
        }
        if source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".transcript.json"))
        {
            self.load_transcript_async_then(source, cx, |_, _| {});
            return;
        }
        self.start_file_transcription(source, false, false);
    }
    fn start_file_transcription_with_archive(
        &mut self,
        source: PathBuf,
        realtime: bool,
        backfill: bool,
        prepared_archive: Option<PathBuf>,
    ) {
        if self.quit_pending || self.active() || self.transcription_busy() {
            self.transcription_status = "录制、退出或其他任务正在进行，未开始文件转写。".into();
            return;
        }
        #[cfg(feature = "online-speaker")]
        self.cancel_word_timing_for_document_change();
        if self.transcript.primary_save_pending {
            self.transcription_status = "当前逐字稿尚未保存，请先重试同步。".into();
            return;
        }
        if let Some(cancel) = self.transcript_load_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
        if source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".transcript.json"))
        {
            self.load_transcript_file(&source);
            return;
        }
        let output = match transcription_output(&self.config.output_dir, &source, backfill) {
            Ok(output) => output,
            Err(error) => {
                self.transcription_status = format!("无法准备会议文件夹：{error:#}");
                return;
            }
        };
        if let Some(archive) = &prepared_archive
            && (!backfill
                || opencap_core::meeting_base(&source) != output
                || archive.parent() != output.parent()
                || !archive.is_file())
        {
            self.transcription_status = "补转目标或已准备的原稿归档发生变化，未开始补转。".into();
            return;
        }
        if backfill && prepared_archive.is_none() {
            match opencap_asr::archive_transcript_before_backfill(&output) {
                Ok(Some(_)) => (),
                Ok(None) => {
                    self.transcription_status = "未开始补转：找不到需要保留的原稿。".into();
                    return;
                }
                Err(error) => {
                    self.transcription_status = format!("未开始补转，原稿备份失败：{error:#}");
                    return;
                }
            }
        }
        self.stash_live_notes_before_switch();
        #[cfg(feature = "online-speaker")]
        let previous_speakers = if backfill && !self.transcript.speaker_turns.is_empty() {
            let mut history = self.transcript.speaker_history.clone();
            let mut extra = std::collections::BTreeMap::new();
            if let Some(run) = &self.transcript.run {
                extra.insert("source_run_id".into(), run.run_id.clone().into());
            }
            if let Some(source) = &self.transcript.source {
                extra.insert(
                    "source_path".into(),
                    serde_json::to_value(source).unwrap_or_default(),
                );
            }
            history.push(opencap_asr::SpeakerAnalysisVersion {
                version_id: opencap_core::new_stable_id(),
                created_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
                turns: self.transcript.speaker_turns.clone(),
                names: self.transcript.speaker_names.clone(),
                voiceprints: self.transcript.speaker_voiceprints.clone(),
                status: "补转前说话人版本；原媒体内容尚未校验，未自动套用到新稿".into(),
                extra,
            });
            Some(history)
        } else {
            None
        };
        let offline_model = ModelKind::from_offline(self.config.offline_asr_model);
        let kind = if realtime {
            self.asr_models
                .live_plan_for(
                    self.config.live_asr_model,
                    self.config.offline_asr_model,
                    self.config.transcription_context_revision,
                )
                .map(|plan| plan.kind)
                .unwrap_or_else(|| {
                    ModelKind::from_live(self.config.live_asr_model).unwrap_or(offline_model)
                })
        } else {
            offline_model
        };
        #[cfg(feature = "online-speaker")]
        let speaker_started = self.start_file_online_speakers(output.clone(), source.clone());
        let speaker_sender: Option<crossbeam_channel::Sender<PcmAudio>> = None;
        match AsrSession::file_with_options_observed(
            model_root(),
            kind,
            source.clone(),
            output.clone(),
            realtime,
            self.config.transcription_hotwords.clone(),
            self.config.transcription_context_revision,
            offline_model,
            self.config.transcription_language,
            self.config.transcription_punctuation,
            speaker_sender,
        ) {
            Ok(asr) => {
                self.text_document = None;
                self.transcript_history_page = 0;
                self.invalidate_transcript_history();
                #[cfg(feature = "online-speaker")]
                {
                    self.backfill_speaker_history = previous_speakers;
                }
                self.transcription_clock.start();
                self.speaker_clock = task::TaskClock::default();
                self.asr = Some(asr);
                self.asr_live = false;
                self.revision_runtime = None;
                self.revision_failure = None;
                self.transcript = Transcript::default();
                self.transcript_disk_baseline = None;
                self.transcript.source = Some(source);
                self.transcript_dirty = true;
                self.transcript_new_document = true;
                self.transcript_output = Some(output);
                self.reset_minutes();
                // Re-transcribing a recording must retain previous AI versions.
                self.load_minutes();
                self.transcription_progress = 0.0;
                self.transcription_status = if backfill {
                    "正在从完整录制补转；已有实时稿已保留为历史版本…"
                } else if realtime {
                    "按音频原速试播所选模型…"
                } else {
                    "正在转录文件，首次使用将下载离线模型…"
                }
                .into();
                self.transcription_tab = true;
                self.settings = false;
                self.meeting_page = workspace::MeetingPage::Transcript;
            }
            Err(e) => {
                #[cfg(feature = "online-speaker")]
                if speaker_started && let Some(controller) = &mut self.online_speakers {
                    controller.finish_file_transcription(Some(&output), None, false);
                    controller.cancel("文件转录未启动，在线说话人已停止。");
                }
                self.transcription_status = format!("无法开始转录：{e:#}");
            }
        }
    }
    pub(super) fn poll_transcription(
        &mut self,
        cx: &mut Context<Self>,
        allow_backfill: bool,
    ) -> bool {
        let mut changed = false;
        let mut disconnected = false;
        let mut events = Vec::new();
        if let Some(rx) = &self.preparing_model {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for event in events {
            changed = true;
            match event {
                PrepareEvent::Progress(s, fraction) => {
                    if !self
                        .prepare_cancelled
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        self.transcription_status = s;
                        self.transcription_progress = fraction.unwrap_or(0.);
                        if let Some(progress) = fraction {
                            self.transcription_clock.progress(progress);
                        } else {
                            self.transcription_clock.clear_progress();
                        }
                    }
                }
                PrepareEvent::Done(kind, result) => {
                    self.asr_models = opencap_asr::ModelAvailability::inspect(&model_root());
                    self.preparing_model = None;
                    self.transcription_clock.finish();
                    self.transcription_progress = if result.is_ok() { 1. } else { 0. };
                    self.transcription_status = match result {
                        Ok(_) => format!("{}已就绪", kind.label()),
                        Err(e)
                            if self
                                .prepare_cancelled
                                .load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            format!("下载已取消：{e:#}")
                        }
                        Err(e) => format!("下载失败，可重试：{e:#}"),
                    };
                }
                PrepareEvent::PunctuationDone(result) => {
                    self.asr_models = opencap_asr::ModelAvailability::inspect(&model_root());
                    self.preparing_model = None;
                    self.transcription_clock.finish();
                    self.transcription_progress = if result.is_ok() { 1. } else { 0. };
                    self.transcription_status = match result {
                        Ok(_) => "中英标点模型已就绪".into(),
                        Err(e)
                            if self
                                .prepare_cancelled
                                .load(std::sync::atomic::Ordering::Relaxed) =>
                        {
                            format!("下载已取消：{e:#}")
                        }
                        Err(e) => format!("下载失败，可重试：{e:#}"),
                    };
                }
            }
        }
        if disconnected && self.preparing_model.take().is_some() {
            self.transcription_clock.finish();
            self.transcription_status = "下载线程意外退出；可重新下载。".into();
            changed = true;
        }
        let events: Vec<_> = self
            .asr
            .as_ref()
            .map(|a| a.events.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            changed = true;
            match event {
                AsrEvent::Status(s) => {
                    self.transcription_status =
                        if self.asr.as_ref().is_some_and(|asr| asr.cancel_requested()) {
                            format!("正在取消转录；当前处理：{s}")
                        } else {
                            s
                        };
                }
                AsrEvent::RevisionStatus(state, reason) => {
                    if self
                        .transcript
                        .run
                        .as_ref()
                        .is_some_and(|run| run.state != "running")
                    {
                        continue;
                    }
                    self.revision_runtime = Some(state);
                    self.revision_failure = reason.clone();
                    if let Some(run) = &mut self.transcript.run {
                        run.extra
                            .insert("revision_effective".into(), state.code().into());
                        if let Some(reason) = &reason {
                            run.extra
                                .insert("revision_failure".into(), reason.clone().into());
                        }
                    }
                    if state == opencap_asr::RevisionRuntimeState::Failed {
                        self.transcription_status = format!(
                            "本次句段复核不可用，流式原文会保留。{}",
                            reason
                                .map(|reason| format!(" {reason}"))
                                .unwrap_or_default()
                        );
                    }
                }
                AsrEvent::Started(mut run) => {
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers
                        && let Err(error) =
                            controller.bind_file_run(self.transcript_output.as_ref(), &run.run_id)
                    {
                        self.online_speaker_status =
                            format!("说话人运行未绑定：{error:#}；旧结果继续单独保存。");
                    }
                    if let Some(state) = self.revision_runtime {
                        run.extra
                            .insert("revision_effective".into(), state.code().into());
                    }
                    if let Some(reason) = &self.revision_failure {
                        run.extra
                            .insert("revision_failure".into(), reason.clone().into());
                    }
                    if let Some(base) = &self.transcript_output
                        && let Ok(Some(manifest)) = opencap_core::meeting_manifest(base)
                    {
                        if run.meeting_id.is_none() {
                            run.meeting_id = Some(manifest.meeting_id);
                        } else if run.meeting_id.as_deref() != Some(manifest.meeting_id.as_str()) {
                            self.transcription_status =
                                "识别运行与当前会议身份不一致；已保留原运行信息，请核对。".into();
                        }
                    }
                    self.transcript.run = Some(run);
                }
                AsrEvent::Segment(s) => {
                    self.transcript_dirty |= self.transcript.apply(s);
                }
                AsrEvent::Progress(p) => {
                    self.transcription_progress = p;
                    self.transcription_clock.progress(p);
                }
                AsrEvent::Processed(ms) => self.asr_processed_ms = Some(ms),
                AsrEvent::SpeakerInputGap(start, end) => {
                    let message = format!(
                        "在线说话人处理跟不上文件转录，{}–{} 的输入未送达；转录继续，已有标签待核对。",
                        opencap_asr::timestamp(start, '.'),
                        opencap_asr::timestamp(end, '.')
                    );
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers
                        && controller.base == self.transcript_output
                        && self.transcript.run.as_ref().is_some_and(|run| {
                            controller.belongs_to(self.transcript_output.as_ref(), &run.run_id)
                        })
                    {
                        controller.finish_file_input(None, Some(message.clone()));
                    }
                    self.transcription_status = message;
                }
                AsrEvent::FileSpeakerInputEnded { end_ms, complete } => {
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers
                        && controller.base == self.transcript_output
                        && self.transcript.run.as_ref().is_some_and(|run| {
                            controller.belongs_to(self.transcript_output.as_ref(), &run.run_id)
                        })
                    {
                        controller.finish_file_input(
                            end_ms,
                            (!complete).then(|| {
                                "文件解码未完整结束；在线说话人已有结果保留，尾部待核对。".into()
                            }),
                        );
                    }
                    #[cfg(not(feature = "online-speaker"))]
                    let _ = (end_ms, complete);
                }
                AsrEvent::Gap(start, end) => {
                    let live = self
                        .transcript
                        .run
                        .as_ref()
                        .is_some_and(|run| run.input_kind == "recording");
                    self.transcription_status = if live {
                        format!(
                            "实时识别存在缺口，{}–{} 可从保存文件补转",
                            opencap_asr::timestamp(start, '.'),
                            opencap_asr::timestamp(end, '.')
                        )
                    } else {
                        format!(
                            "输入音频存在时间缺口 {}–{}，转录将标记为部分完成。",
                            opencap_asr::timestamp(start, '.'),
                            opencap_asr::timestamp(end, '.')
                        )
                    };
                }
                AsrEvent::Finished(t) => {
                    let mut t = t;
                    self.transcript_disk_baseline = self
                        .transcript_output
                        .as_ref()
                        .zip(saved_transcript_digest(&t))
                        .map(|(base, digest)| (base.clone(), digest));
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers
                        && let Some(run) = &t.run
                    {
                        controller.verify_file_transcription(self.transcript_output.as_ref(), run);
                    }
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers {
                        controller.finish_file_transcription(
                            self.transcript_output.as_ref(),
                            t.run.as_ref().map(|run| run.run_id.as_str()),
                            t.is_complete(),
                        );
                    }
                    #[cfg(feature = "online-speaker")]
                    if let Some(run) = &t.run
                        && run
                            .extra
                            .get("input_integrity")
                            .and_then(|value| value.as_str())
                            == Some("changed_during_transcription")
                        && let Some(controller) = &mut self.online_speakers
                    {
                        controller
                            .mark_file_input_changed(self.transcript_output.as_ref(), &run.run_id);
                    }
                    #[cfg(feature = "online-speaker")]
                    if let Some(history) = self.backfill_speaker_history.take() {
                        t.speaker_history.extend(history);
                        t.primary_save_pending = true;
                        self.pending_speaker_sync = true;
                    }
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &self.online_speakers
                        && controller.applies_to(self.transcript_output.as_ref(), &t)
                    {
                        match controller.apply(&mut t) {
                            Ok(()) => {
                                // The ASR worker saved `t` before this UI-side
                                // merge; persist the speaker data separately.
                                t.primary_save_pending = true;
                                self.pending_speaker_sync = true;
                            }
                            Err(error) => {
                                self.online_speaker_status =
                                    format!("在线说话人未合入最终稿：{error:#}")
                            }
                        }
                    }
                    self.transcription_clock.finish();
                    if self.transcript_output.is_none() {
                        t.primary_save_pending = true;
                    }
                    let backfill = if !t.cancelled && !t.gaps.is_empty() {
                        t.run
                            .as_ref()
                            .filter(|run| run.input_kind == "recording")
                            .and_then(|run| {
                                Some((
                                    t.source.clone()?,
                                    self.transcript_output.clone()?,
                                    run.run_id.clone(),
                                    serde_json::to_vec(&*t).ok()?,
                                ))
                            })
                            .filter(|(source, _, _, _)| source.exists())
                    } else {
                        None
                    };
                    let deferred = t
                        .segments
                        .iter()
                        .filter(|segment| {
                            segment.review.as_ref().is_some_and(|review| {
                                matches!(
                                    review.state,
                                    opencap_asr::ReviewState::Deferred
                                        | opencap_asr::ReviewState::Failed
                                )
                            })
                        })
                        .count();
                    self.transcription_status = if t.cancelled {
                        "已取消，已确认文本与原媒体保留"
                    } else if t.is_complete() && self.transcript_output.is_some() {
                        "转录完成，已保存完整稿和字幕"
                    } else if t.is_complete() {
                        "转录完成，但没有可用的保存位置；结果仍在内存中"
                    } else {
                        "转录尚未完整完成，原文件保留，可重新导入"
                    }
                    .into();
                    if t.is_complete() && deferred > 0 {
                        self.transcription_status = if self.transcript_output.is_some() {
                            format!("转录已保存；{deferred} 段未完成复核，可使用当前模型另存重转")
                        } else {
                            format!("转录结果尚无保存位置；{deferred} 段未完成复核")
                        };
                    }
                    if let Some(errors) = t
                        .run
                        .as_ref()
                        .and_then(|run| run.extra.get("drain_errors"))
                        .and_then(|value| value.as_array())
                        && !errors.is_empty()
                    {
                        self.transcription_status.push_str("；音频尾部或识别流收尾失败，已确认文字保留，请查看运行详情或重新转录。");
                    }
                    if let Some(count) = t
                        .run
                        .as_ref()
                        .and_then(|run| run.extra.get("terminal_unconfirmed_segments"))
                        .and_then(|value| value.as_u64())
                        .filter(|count| *count > 0)
                    {
                        self.transcription_status.push_str(&format!(
                            "；{count} 段暂存文字未定稿，已保留在逐字稿中，未纳入已确认内容。"
                        ));
                    }
                    if t.run
                        .as_ref()
                        .and_then(|run| run.extra.get("historical_revision_outcome"))
                        .and_then(|value| value.as_str())
                        == Some("incomplete")
                    {
                        self.transcription_status
                            .push_str("；历史配置重转的复核未完整就绪或未完成，保留原始文字。");
                    }
                    if let Some(outcome) = t
                        .run
                        .as_ref()
                        .and_then(|run| run.extra.get("historical_media_outcome"))
                        .and_then(|value| value.as_str())
                        && outcome != "matched_before_and_after"
                    {
                        self.transcription_status
                            .push_str("；无法完整确认历史媒体与本次重转输入一致。");
                    }
                    if t.run
                        .as_ref()
                        .and_then(|run| run.extra.get("parent_audio_stream_recorded"))
                        .and_then(|value| value.as_bool())
                        == Some(false)
                    {
                        self.transcription_status
                            .push_str("；历史音轨未记录，本次使用当前默认音轨。");
                    }
                    if !t.output_warnings.is_empty() {
                        self.transcription_status = format!(
                            "{}；{} 个文件未同步，请查看详情并重试保存",
                            self.transcription_status,
                            t.output_warnings.len()
                        );
                    }
                    if t.primary_save_pending {
                        self.transcription_status
                            .push_str(if self.transcript_output.is_some() {
                                "；最终稿有后续修改，正在等待同步保存。"
                            } else {
                                "；请先另存恢复副本。"
                            });
                    }
                    let complete = t.is_complete();
                    self.transcript = *t;
                    if backfill.is_some() {
                        self.pending_backfill = backfill;
                    }
                    self.transcript_dirty = true;
                    if complete {
                        self.transcription_progress = 1.0;
                    }
                    self.remember_meeting();
                }
                AsrEvent::Error(e) => {
                    #[cfg(feature = "online-speaker")]
                    if let Some(controller) = &mut self.online_speakers {
                        controller.finish_file_transcription(
                            self.transcript_output.as_ref(),
                            self.transcript.run.as_ref().map(|run| run.run_id.as_str()),
                            false,
                        );
                    }
                    self.transcription_clock.finish();
                    let cancelled = self.asr.as_ref().is_some_and(|asr| asr.cancel_requested());
                    let interrupted = self
                        .transcript
                        .run
                        .as_ref()
                        .is_none_or(|run| run.state == "running");
                    let mut unconfirmed = 0;
                    if interrupted {
                        self.transcript.complete = false;
                        self.transcript.cancelled = cancelled;
                        unconfirmed = self.transcript.mark_terminal_unconfirmed();
                        if let Some(run) = &mut self.transcript.run {
                            run.state = if cancelled { "cancelled" } else { "failed" }.into();
                            run.extra
                                .insert("termination_error".into(), e.clone().into());
                            run.finished_unix_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .ok()
                                .and_then(|duration| u64::try_from(duration.as_millis()).ok());
                        }
                        self.transcript_dirty = true;
                    }
                    if interrupted
                        && (!self.transcript.segments.is_empty() || self.transcript.run.is_some())
                    {
                        self.transcript.primary_save_pending = true;
                    }
                    self.transcription_status = if cancelled {
                        format!("取消期间任务已结束，原媒体保留；处理信息：{e}")
                    } else {
                        format!("转录错误，原媒体保留：{e}")
                    };
                    if unconfirmed > 0 {
                        self.transcription_status
                            .push_str(&format!("；{unconfirmed} 段未定稿文字已标记为暂存待核对。"));
                    }
                    if self.transcript.primary_save_pending {
                        self.transcription_status
                            .push_str("；当前稿件尚未保存，请重试同步。");
                    }
                }
            }
        }
        #[cfg(feature = "online-speaker")]
        let online_ready_for_backfill = self.online_speakers.as_ref().is_none_or(|controller| {
            controller.base != self.transcript_output || controller.ready_for_backfill()
        });
        #[cfg(not(feature = "online-speaker"))]
        let online_ready_for_backfill = true;
        #[cfg(feature = "online-speaker")]
        let speaker_saved_for_backfill = !self.pending_speaker_sync;
        #[cfg(not(feature = "online-speaker"))]
        let speaker_saved_for_backfill = true;
        if allow_backfill
            && !self.quit_pending
            && !self.active()
            && !self.transcription_busy()
            && !self.backfill_checking
            && !self.transcript.primary_save_pending
            && !self.notes_have_unsaved_work(cx)
            && !self.persist_minutes_draft(cx)
            && self
                .live_notes
                .as_ref()
                .is_none_or(|notes| !notes.has_unsaved_work())
            && online_ready_for_backfill
            && speaker_saved_for_backfill
            && let Some((source, base, run_id, baseline)) = self.pending_backfill.take()
        {
            let same_run = self.transcript_output.as_ref() == Some(&base)
                && self
                    .transcript
                    .run
                    .as_ref()
                    .is_some_and(|run| run.run_id == run_id)
                && !self.transcript.primary_save_pending
                && opencap_core::meeting_base(&source) == base
                && source.with_extension("recording.json").is_file()
                && transcript_matches_snapshot(&self.transcript, &baseline);
            if !same_run {
                self.transcription_status =
                    "原会议或逐字稿已变化，未自动补转；可从原会议手动重转。".into();
            } else {
                self.backfill_checking = true;
                self.transcription_status = "正在后台核对补转前保存的原稿…".into();
                cx.spawn(async move |entity, cx| {
                    let transcript_path = base.with_extension("transcript.json");
                    let task_path = transcript_path.clone();
                    let task_base = base.clone();
                    let baseline = std::sync::Arc::new(baseline);
                    let task_baseline = baseline.clone();
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            let loaded = read_transcript_file(&task_path, None)?;
                            anyhow::ensure!(
                                transcript_matches_snapshot(&loaded.transcript, &task_baseline),
                                "磁盘原稿与当前逐字稿不一致"
                            );
                            anyhow::ensure!(
                                loaded.still_current(&task_path),
                                "磁盘原稿在归档前发生变化"
                            );
                            let archive =
                                opencap_asr::archive_transcript_before_backfill(&task_base)?
                                    .ok_or_else(|| anyhow::anyhow!("找不到需要保留的原稿"))?;
                            let archived = read_transcript_file(&archive, None)?;
                            anyhow::ensure!(
                                transcript_matches_snapshot(&archived.transcript, &task_baseline),
                                "归档内容与当前逐字稿不一致"
                            );
                            anyhow::ensure!(
                                loaded.still_current(&task_path)
                                    && archived.still_current(&archive),
                                "原稿或归档在核对期间发生变化"
                            );
                            Ok::<_, anyhow::Error>((loaded, archive, archived))
                        })
                        .await;
                    let _ = entity.update(cx, |this, cx| {
                        this.backfill_checking = false;
                        let current = !this.quit_pending
                            && !this.active()
                            && !this.transcription_busy()
                            && !this.transcript.primary_save_pending
                            && this.transcript_output.as_ref() == Some(&base)
                            && this
                                .transcript
                                .run
                                .as_ref()
                                .is_some_and(|run| run.run_id == run_id)
                            && opencap_core::meeting_base(&source) == base
                            && source.with_extension("recording.json").is_file()
                            && transcript_matches_snapshot(&this.transcript, &baseline);
                        if !current {
                            this.transcription_status =
                                "核对期间会议、任务或逐字稿已变化，未自动补转。".into();
                        } else if let Err(error) = &result {
                            this.transcription_status =
                                format!("原稿尚未安全保存，未自动补转：{error:#}");
                        } else if !result.as_ref().is_ok_and(|(loaded, archive, archived)| {
                            loaded.still_current(&transcript_path)
                                && archived.still_current(archive)
                        }) {
                            this.transcription_status =
                                "原稿或归档在核对后发生变化，未自动补转。".into();
                        } else {
                            this.persist_notes(cx);
                            let notes_unsaved = this.notes_have_unsaved_work(cx);
                            if notes_unsaved
                                || this.persist_minutes_draft(cx)
                                || this
                                    .live_notes
                                    .as_ref()
                                    .is_some_and(|notes| notes.has_unsaved_work())
                            {
                                this.pending_backfill = Some((
                                    source.clone(),
                                    base.clone(),
                                    run_id.clone(),
                                    (*baseline).clone(),
                                ));
                                this.transcription_status =
                                    "笔记或纪要草稿尚未保存；自动补转将等待保存后重新核对。".into();
                            } else {
                                if let Ok((_, archive, _)) = result {
                                    this.start_file_transcription_with_archive(
                                        source,
                                        false,
                                        true,
                                        Some(archive),
                                    );
                                }
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            changed = true;
        }
        #[cfg(feature = "online-speaker")]
        if self.pending_speaker_sync && !self.active() && !self.transcription_busy() {
            self.pending_speaker_sync = false;
            self.retry_transcript_sync();
            changed = true;
        }
        if let Some(rx) = &self.ai.events {
            for event in rx.try_iter() {
                match event {
                    opencap_asr::MinutesEvent::Status(status) => {
                        if self.ai.correction_task.is_some() {
                            self.transcription_status = status.clone();
                        }
                        self.ai.status = status;
                    }
                    opencap_asr::MinutesEvent::Delta(text) => self.ai.stream.push_str(&text),
                }
                self.ai.chat_dirty = true;
                changed = true;
            }
        }
        if let Some(result) = self
            .summary_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("AI 工作线程已退出；已有文件保留")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.summary_task = None;
            let task_base = self.summary_task_base.take();
            self.ai_clock.finish();
            self.ai.events = None;
            self.ai.chat_dirty = true;
            changed = true;
            match result {
                Ok(history) => {
                    if let Some(base) = &task_base {
                        let _ = std::fs::remove_file(base.with_extension("summary.request.json"));
                    }
                    if task_base.as_ref() != self.transcript_output.as_ref() {
                        if let Some(base) = task_base {
                            self.meeting_scan_extra.insert(base);
                            self.notes_index_pending = Some(std::time::Instant::now());
                        }
                        self.ai.status = "先前会议的纪要已保存；当前会议内容未更改。".into();
                    } else {
                        let status = history.export_warning.clone().unwrap_or_else(|| {
                            if history.exports_pending() {
                                "AI 正文已保存；派生文件可在文件与版本中重试导出。".into()
                            } else {
                                "AI 结果已保存，可继续提问或修改；旧版本保留".into()
                            }
                        });
                        self.summary_text = history.latest().to_string();
                        self.ai.history = history;
                        self.ai.stream.clear();
                        self.ai.pending_request.clear();
                        self.ai.editor_sync = true;
                        self.remember_meeting();
                        self.ai.status = status;
                    }
                }
                Err(e) => self.ai.status = format!("AI 处理未完成，已有稿件保留：{e:#}"),
            }
        }
        if let Some(result) = self
            .ai
            .correction_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("全文校正线程已退出；原文保留")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.ai.correction_task = None;
            self.ai.events = None;
            self.ai_clock.finish();
            match result {
                Ok(transcript) => {
                    self.transcript_disk_baseline = self
                        .transcript_output
                        .as_ref()
                        .zip(saved_transcript_digest(&transcript))
                        .map(|(base, digest)| (base.clone(), digest));
                    let status = if transcript.primary_save_pending {
                        "全文结果已生成但主稿未保存；请核对冲突或另存恢复副本。".into()
                    } else if let Some(c) = &transcript.correction {
                        format!(
                            "全文校正完成 · 本次修改 {} 段 · 原文可恢复。之后生成纪要会使用校正稿；已有纪要需发送修改要求更新。",
                            c.changed_segments
                        )
                    } else {
                        "已恢复原始识别文字，转写文件已同步".into()
                    };
                    self.ai.status = if transcript.output_warnings.is_empty() {
                        status
                    } else {
                        format!(
                            "{status}；部分文件未同步：{}",
                            transcript.output_warnings.join("；")
                        )
                    };
                    self.transcript = transcript;
                    self.transcript_dirty = true;
                    self.transcript_new_document = true;
                    self.text_document = None;
                    self.remember_meeting();
                }
                Err(e) => self.ai.status = format!("全文校正未完成，原文保留：{e:#}"),
            }
            self.transcription_status = self.ai.status.clone();
            changed = true;
        }
        if let Some(result) = self
            .speaker_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("说话人分析线程已退出；已有文件保留")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.speaker_task = None;
            self.speaker_clock.finish();
            self.ai.progress = None;
            changed = true;
            match result {
                Ok(t) => {
                    let speakers = t.speaker_count();
                    // Auto detection can over-split far-field audio. Report the
                    // count truthfully but suggest an explicit count instead of
                    // silently merging people locally.
                    let auto_count = self
                        .speaker_count
                        .read(cx)
                        .value()
                        .trim()
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(0)
                        == 0;
                    let count_hint = if auto_count && speakers >= 12 {
                        "；自动人数可能过高，如实际人数较少请在说话人面板填写已知人数后重新分析"
                    } else {
                        ""
                    };
                    self.transcript_disk_baseline = self
                        .transcript_output
                        .as_ref()
                        .zip(saved_transcript_digest(&t))
                        .map(|(base, digest)| (base.clone(), digest));
                    self.transcript = t;
                    self.transcript_dirty = true;
                    self.remember_meeting();
                    self.transcription_status = if self.transcript.primary_save_pending {
                        format!(
                            "说话人分析完成 · {speakers} 组{count_hint}；主稿未保存，请核对冲突或另存恢复副本。"
                        )
                    } else if self.transcript.output_warnings.is_empty() {
                        format!(
                            "说话人分析完成 · {speakers} 组{count_hint} · 已同步逐字稿 JSON、TXT、字幕和 Word。AI 纪要可在对话中要求更新。"
                        )
                    } else {
                        format!(
                            "说话人分析完成 · {speakers} 组{count_hint}；部分文件未同步，结果已显示，可重试同步。"
                        )
                    };
                }
                Err(e) => {
                    self.transcription_status = format!("说话人分析未完成；已有稿件保留：{e:#}")
                }
            }
        }
        if let Some(result) = self
            .transcript_sync
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(t) => Some(Ok(t)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(())),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            })
        {
            self.transcript_sync = None;
            changed = true;
            match result {
                Ok(t) => {
                    if let Some(digest) = saved_transcript_digest(&t)
                        && let Some(base) = &self.transcript_output
                    {
                        self.transcript_disk_baseline = Some((base.clone(), digest));
                    }
                    self.transcript = t;
                    self.transcript_dirty = true;
                    self.transcription_status = if self.transcript.output_warnings.is_empty() {
                        "逐字稿与说话人已同步到全部文件".into()
                    } else {
                        "部分文件仍未同步，请检查文件占用或文件夹权限".into()
                    };
                    self.remember_meeting();
                }
                Err(()) => {
                    self.transcript.primary_save_pending = true;
                    self.transcription_status =
                        "同步线程已退出，已有分析结果保留；请重试保存".into();
                }
            }
        }
        if let Some(rx) = &self.ai.progress {
            while let Ok(status) = rx.try_recv() {
                if self.speaker_task.is_some() {
                    self.transcription_status = status;
                } else {
                    self.ai.status = status;
                }
                changed = true;
            }
        }
        changed
    }
    pub(super) fn retry_transcript_sync(&mut self) {
        if self.active() || self.transcription_busy() {
            return;
        }
        let Some(base) = self.transcript_output.clone() else {
            return;
        };
        let mut transcript = self.transcript.clone();
        let expected = self
            .transcript_disk_baseline
            .as_ref()
            .filter(|(saved_base, _)| saved_base == &base)
            .map(|(_, digest)| digest.clone());
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.transcript_sync = Some(rx);
        self.transcription_status = "正在同步文档，无需重新识别…".into();
        std::thread::spawn(move || {
            let guard = opencap_core::meeting_metadata_guard(&base);
            let checked = guard
                .as_ref()
                .map_err(|error| anyhow::anyhow!("无法锁定会议元数据：{error:#}"))
                .and_then(|_| verify_transcript_retry(&base, &transcript, expected.as_deref()));
            if let Err(error) = checked {
                transcript.primary_save_pending = true;
                transcript
                    .output_warnings
                    .retain(|warning| !warning.starts_with("逐字稿并发核对失败："));
                transcript
                    .output_warnings
                    .push(format!("逐字稿并发核对失败：{error:#}"));
                let _ = tx.send(transcript);
                return;
            }
            let identity_pending = transcript
                .output_warnings
                .iter()
                .any(|warning| warning.starts_with("会议身份记录失败："));
            let fingerprint_pending = transcript
                .run
                .as_ref()
                .is_some_and(|run| run.extra.contains_key("asset_fingerprint_warning"));
            let mut identity_ready = true;
            if identity_pending {
                transcript
                    .output_warnings
                    .retain(|warning| !warning.starts_with("会议身份记录失败："));
                if let Err(error) =
                    opencap_asr::reconcile_transcript_identity(&mut transcript, &base)
                {
                    transcript
                        .output_warnings
                        .push(format!("会议身份记录失败：{error:#}"));
                    identity_ready = false;
                }
            }
            if fingerprint_pending && identity_ready {
                transcript
                    .output_warnings
                    .retain(|warning| !warning.starts_with("媒体资产指纹登记失败："));
                if let Err(error) = opencap_asr::reconcile_asset_fingerprint(&mut transcript, &base)
                {
                    if let Some(run) = &mut transcript.run {
                        run.extra.insert(
                            "asset_fingerprint_warning".into(),
                            format!("{error:#}").into(),
                        );
                    }
                    transcript
                        .output_warnings
                        .push(format!("媒体资产指纹登记失败：{error:#}"));
                }
            }
            transcript.sync_outputs(&base);
            let _ = tx.send(transcript);
        });
    }
    /// Save an in-memory result to a new, unbound recovery file.
    fn save_unbound_transcript_recovery(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.transcription_busy() || !self.transcript.primary_save_pending {
            return;
        }
        let expected_base = self.transcript_output.clone();
        let snapshot = match serde_json::to_vec(&self.transcript) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.transcription_status = format!("无法准备恢复副本：{error}");
                return;
            }
        };
        let mut recovery = self.transcript.clone();
        recovery.extra.insert(
            "history_snapshot_kind".into(),
            "manual_recovery_unbound".into(),
        );
        let bytes = match serde_json::to_vec_pretty(&recovery) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.transcription_status = format!("无法准备恢复副本：{error}");
                return;
            }
        };
        let prompt = cx.prompt_for_new_path(
            &self.config.output_dir,
            Some("转写恢复副本.transcript.json"),
        );
        cx.spawn(async move |entity, cx| {
            let selected = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                error => {
                    let _ = entity.update(cx, |this, cx| {
                        this.transcription_status = format!("无法选择恢复位置：{error:?}");
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(name) = selected.file_name() else {
                return;
            };
            let destination = if name.to_string_lossy().ends_with(".transcript.json") {
                selected.clone()
            } else {
                let mut name = name.to_os_string();
                name.push(".transcript.json");
                selected.with_file_name(name)
            };
            let ready = entity
                .update(cx, |this, cx| {
                    if this.quit_pending
                        || this.active()
                        || this.transcription_busy()
                        || this.transcript_output != expected_base
                        || !this.transcript.primary_save_pending
                        || !transcript_matches_snapshot(&this.transcript, &snapshot)
                    {
                        this.transcription_status =
                            "逐字稿或任务状态已变化；请重新保存恢复副本。".into();
                        cx.notify();
                        false
                    } else {
                        this.historical_checking = true;
                        this.transcription_status = "正在保存转写恢复副本…".into();
                        cx.notify();
                        true
                    }
                })
                .unwrap_or(false);
            if !ready {
                return;
            }
            let path = destination.clone();
            let result = cx
                .background_executor()
                .spawn(async move { opencap_core::atomic_create_bytes(&path, &bytes) })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.historical_checking = false;
                match result {
                    Ok(()) => {
                        if this.transcript_output == expected_base
                            && transcript_matches_snapshot(&this.transcript, &snapshot)
                        {
                            this.transcript.primary_save_pending = false;
                            this.transcription_status = format!(
                                "恢复副本已保存至 {}；会议主稿未因此覆盖，请核对。",
                                destination.display()
                            );
                        } else {
                            this.transcription_status = format!(
                                "恢复副本已保存至 {}；当前稿已变化，请继续保存。",
                                destination.display()
                            );
                        }
                    }
                    Err(error) => {
                        this.transcription_status =
                            format!("恢复副本未保存，内存结果保留：{error:#}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn start_speakers(&mut self, cx: &App) {
        if self.active()
            || self.transcription_busy()
            || self.transcript.primary_save_pending
            || !self.transcript.is_complete()
        {
            return;
        }
        let Some(output) = self.transcript_output.clone() else {
            return;
        };
        let Some(expected_digest) = self
            .transcript_disk_baseline
            .as_ref()
            .filter(|(base, _)| base == &output)
            .map(|(_, digest)| digest.clone())
        else {
            self.transcription_status = "逐字稿缺少可靠磁盘基线；请重新加载后再分析说话人。".into();
            return;
        };
        let count = match self.speaker_count.read(cx).value().trim().parse::<u32>() {
            Ok(n) if n <= 20 => n,
            _ => {
                self.transcription_status = "人数请输入 0（自动）或 1–20".into();
                return;
            }
        };
        let transcript = self.transcript.clone();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threshold = match self
            .speaker_threshold
            .read(cx)
            .value()
            .trim()
            .parse::<f32>()
        {
            Ok(n) if n.is_finite() && (0.1..=1.0).contains(&n) => n,
            _ => {
                self.transcription_status = "阈值请输入 0.1–1.0；越大越容易合并为同一人".into();
                return;
            }
        };
        self.speaker_cancelled = cancelled.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        self.speaker_clock.start();
        let threads = self.speaker_threads;
        self.speaker_task = Some(rx);
        self.ai.progress = Some(progress_rx);
        self.transcription_status = "开始会后说话人分析；首次会下载本地模型…".into();
        std::thread::spawn(move || {
            let _ = tx.send(opencap_asr::distinguish_speakers_guarded(
                transcript,
                &model_root(),
                &output,
                count,
                threshold,
                threads,
                &cancelled,
                &expected_digest,
                |s| {
                    let _ = progress_tx.send(s);
                },
            ));
        });
    }
    pub(super) fn live_transcript_card(&self, cx: &Context<Self>) -> Div {
        if self.active() && self.recording_without_live_due_to_task {
            return ui::card().child(label("实时文字暂不可用")).child(label(
                "另一场会议的任务仍在运行，本次媒体会继续保存。录制结束后可从会议库补转。",
            ));
        }
        ui::card()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label("实时文字"))
                    .child(button("open-live-text", "查看完整逐字稿", false).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.transcription_tab = true;
                            this.settings = false;
                            this.meeting_page = workspace::MeetingPage::Transcript;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "recording-live-toggle",
                            if self.config.realtime_transcription {
                                "实时转录：开"
                            } else {
                                "实时转录：关"
                            },
                            self.config.realtime_transcription,
                        )
                        .disabled(self.active() || self.transcription_busy())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.realtime_transcription =
                                !this.config.realtime_transcription;
                            this.save();
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "recording-revision-toggle",
                            if self.config.transcription_context_revision {
                                "断句校正：开"
                            } else {
                                "断句校正：关"
                            },
                            self.config.transcription_context_revision,
                        )
                        .disabled(self.active() || self.transcription_busy())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.transcription_context_revision =
                                !this.config.transcription_context_revision;
                            this.save();
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "recording-high-quality",
                            if self.asr.as_ref().is_some_and(|asr| !asr.finished()) {
                                match self.transcript.run.as_ref() {
                                    Some(run) if run.revision_scheduled => {
                                        match self.revision_runtime {
                                            Some(opencap_asr::RevisionRuntimeState::Ready) => {
                                                "本次断句复核：模型已就绪"
                                            }
                                            Some(opencap_asr::RevisionRuntimeState::Failed) => {
                                                "本次断句复核：不可用，保留原文"
                                            }
                                            _ => "本次断句复核：正在加载",
                                        }
                                    }
                                    Some(run)
                                        if run
                                            .extra
                                            .get("revision_requested")
                                            .and_then(|value| value.as_bool())
                                            == Some(true) =>
                                    {
                                        "本次断句复核：已请求，未安排"
                                    }
                                    Some(_) => "本次断句复核：未请求",
                                    None => "本次识别模型：准备中",
                                }
                            } else if self.config.realtime_transcription
                                && self.config.transcription_context_revision
                            {
                                "下次高质量：已请求 · 点击关闭复核"
                            } else {
                                "启用实时高质量"
                            },
                            self.config.realtime_transcription
                                && self.config.transcription_context_revision,
                        )
                        .disabled(self.active() || self.transcription_busy())
                        .on_click(cx.listener(|this, _, _, cx| {
                            let requested = this.config.realtime_transcription
                                && this.config.transcription_context_revision;
                            this.config.realtime_transcription = true;
                            this.config.transcription_context_revision = !requested;
                            this.save();
                            this.transcription_status = if requested {
                                "已关闭下次录制的断句复核；实时转录保持开启".into()
                            } else {
                                "已请求下次录制启用实时转录与断句复核；实际方式取决于模型是否就绪"
                                    .into()
                            };
                            cx.notify();
                        })),
                    ),
            )
            .when(!self.active(), |d| {
                d.child(label(
                    self.asr_models
                        .live_plan_for(
                            self.config.live_asr_model,
                            self.config.offline_asr_model,
                            self.config.transcription_context_revision,
                        )
                        .map(|p| p.description())
                        .unwrap_or("所选语音模型未安装，请前往设置下载"),
                ))
            })
            .when(
                self.config.realtime_transcription
                    && self.config.transcription_context_revision
                    && !self
                        .asr_models
                        .live_plan_for(
                            self.config.live_asr_model,
                            self.config.offline_asr_model,
                            true,
                        )
                        .is_some_and(|plan| plan.context_revision),
                |d| {
                    d.child(label(
                        "当前双模型复核未就绪：请安装或选择流式模型及所选文件模型。",
                    ))
                },
            )
            .child(label(self.transcription_status.clone()))
            .when_some(self.online_speaker_caption(), |d, status| {
                d.child(label(status))
            })
            .when(self.active() || self.live_notes.is_some(), |d| {
                d.child(self.live_notes_panel(cx))
            })
            .when(self.asr_live && self.active(), |d| {
                d.when_some(self.asr_processed_ms, |d, processed| {
                    let recorded_ms = (self.stats.duration_seconds.max(0.) * 1000.) as u64;
                    let delay = recorded_ms.saturating_sub(processed);
                    d.child(label(if delay < 2000 {
                        format!(
                            "已处理至 {} · 基本同步",
                            opencap_asr::timestamp(processed, '.')
                        )
                    } else {
                        format!(
                            "已处理至 {} · 落后录音约 {:.1} 秒",
                            opencap_asr::timestamp(processed, '.'),
                            delay as f64 / 1000.
                        )
                    }))
                })
            })
            .child(
                div()
                    .h(px(
                        if !self.config.realtime_transcription
                            && self.transcript.segments.is_empty()
                        {
                            64.
                        } else if self.config.audio_only {
                            380.
                        } else {
                            320.
                        },
                    ))
                    .child(self.transcript_view.clone()),
            )
    }
    fn speaker_history_comparison(&self, cx: &Context<Self>) -> Div {
        const PAGE_SIZE: usize = 16;
        let history = &self.transcript.speaker_history;
        let mut panel = div().flex().flex_col().gap_2();
        let Some(latest) = history.last() else {
            return panel;
        };
        let selected = self
            .speaker_compare_version
            .as_ref()
            .and_then(|id| history.iter().position(|item| &item.version_id == id));
        let Some(index) = selected else {
            let id = latest.version_id.clone();
            return panel.child(
                button(
                    "compare-speaker-history",
                    format!("对照历史说话人版本 · {} 份", history.len()),
                    false,
                )
                .ghost()
                .disabled(self.active() || self.transcription_busy())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.speaker_compare_version = Some(id.clone());
                    this.speaker_compare_page = 0;
                    cx.notify();
                })),
            );
        };
        let version = &history[index];
        let prior_id = history[index.saturating_sub(1)].version_id.clone();
        let next_id = history[(index + 1).min(history.len() - 1)]
            .version_id
            .clone();
        let total = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.is_confirmed())
            .count();
        let pages = total.saturating_add(PAGE_SIZE - 1) / PAGE_SIZE;
        let page = self.speaker_compare_page.min(pages.saturating_sub(1));
        let old = opencap_asr::Transcript {
            speaker_turns: version.turns.clone(),
            speaker_names: version.names.clone(),
            ..Default::default()
        };
        let old_timeline = old.speaker_timeline();
        let current_timeline = self.transcript.speaker_timeline();
        let rows: Vec<_> = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.is_confirmed())
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .map(|segment| {
                let old_label = old_timeline.label(segment.start_ms, segment.end_ms);
                let current_label = current_timeline.label(segment.start_ms, segment.end_ms);
                let excerpt: String = segment.text.chars().take(120).collect();
                (
                    opencap_asr::timestamp(segment.start_ms, '.'),
                    excerpt,
                    if old_label.is_empty() {
                        "未分析".into()
                    } else {
                        old_label
                    },
                    if current_label.is_empty() {
                        "未分析".into()
                    } else {
                        current_label
                    },
                )
            })
            .collect();
        panel = panel
            .child(label(format!("历史版本 {} / {} · {}", index + 1, history.len(), version.status)))
            .child(label("按当前稿的句段时间并列显示；编号、时间重合和姓名均不能单独证明是同一人。不同来源的时间轴需自行核对。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("speaker-version-prev", "上一版本", false).ghost().disabled(index == 0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.speaker_compare_version = Some(prior_id.clone());
                        this.speaker_compare_page = 0;
                        cx.notify();
                    })))
                .child(button("speaker-version-next", "下一版本", false).ghost().disabled(index + 1 >= history.len())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.speaker_compare_version = Some(next_id.clone());
                        this.speaker_compare_page = 0;
                        cx.notify();
                    })))
                .child(button("speaker-version-close", "关闭对照", false).ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.speaker_compare_version = None;
                        cx.notify();
                    }))));
        if rows.is_empty() {
            panel = panel.child(label("当前稿没有可按时间对照的已确认句段。"));
        }
        for (time, excerpt, before, current) in rows {
            panel = panel.child(
                div()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_2()
                    .child(label(format!("{time} · {excerpt}")))
                    .child(label(format!("历史：{before}")))
                    .child(label(format!("当前：{current}"))),
            );
        }
        panel.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    button("speaker-compare-page-prev", "上一页", false)
                        .ghost()
                        .disabled(page == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.speaker_compare_page = page.saturating_sub(1);
                            cx.notify();
                        })),
                )
                .child(label(format!("{} / {}", page + 1, pages.max(1))))
                .child(
                    button("speaker-compare-page-next", "下一页", false)
                        .ghost()
                        .disabled(page + 1 >= pages)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.speaker_compare_page = page + 1;
                            cx.notify();
                        })),
                ),
        )
    }

    pub(super) fn transcription_view(&self, width: f32, cx: &Context<Self>) -> Div {
        if let Some(document) = &self.text_document {
            return div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    button("close-text-document", "返回逐字稿", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.text_document = None;
                            cx.notify();
                        },
                    )),
                )
                .h_full()
                .min_h(px(0.))
                .child(div().flex_1().min_h(px(0.)).child(document.clone()));
        }
        let busy = self.transcription_busy();
        let ready = self.transcript.is_complete();
        let speakers = self.transcript.speaker_count();
        let mut toolbar = div().flex().flex_wrap().items_center().gap_2();
        #[cfg(feature = "online-speaker")]
        if ready || self.word_timing_cancel.is_some() || !self.word_timing_status.is_empty() {
            let running = self.word_timing_cancel.is_some();
            let cancelling = self
                .word_timing_cancel
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));
            toolbar = toolbar.child(div().w_full().p_3().rounded_lg().bg(rgb(SOFT))
                .child(label("模型词时间 · 实验：需要可信 NeMo 库和固定版本 ASR 模型；仅应用文字一致且时间落在句段内的结果，词边界精度尚未验收。"))
                .when(!self.word_timing_status.is_empty(), |d| d.child(label(self.word_timing_status.clone())))
                .when(running, |d| d.child(button("cancel-word-timing", if cancelling { "正在停止…" } else { "停止生成词时间" }, false)
                    .ghost().disabled(cancelling).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(flag) = &this.word_timing_cancel {
                            flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            this.word_timing_status = "正在停止，需等待当前模型调用结束。".into();
                        }
                        cx.notify();
                    }))))
                .when(!running, |d| d.child(button("generate-word-timing", "生成模型词时间…", false)
                    .ghost().disabled(!ready || busy || self.active() || self.transcript.primary_save_pending
                        || self.transcript_output.is_none() || self.transcript.source.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.choose_word_timing_model(cx))))));
        }
        let mut untimed = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.is_confirmed() && segment.end_ms <= segment.start_ms);
        if let Some(first) = untimed.next() {
            let first_id = first.id;
            let count = 1 + untimed.count();
            toolbar = toolbar.child(
                div()
                    .w_full()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(SOFT))
                    .child(label(format!(
                        "{count} 段文字没有有效时间范围；文字保留，字幕不包含这些段。"
                    )))
                    .child(
                        button("show-untimed-segment", "查看首段", false)
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.transcript_view
                                    .update(cx, |view, cx| view.reveal_segment(first_id, cx));
                                cx.notify();
                            })),
                    ),
            );
        }
        if !self.transcript.gaps.is_empty()
            && self
                .transcript
                .run
                .as_ref()
                .is_some_and(|run| run.input_kind == "recording")
        {
            let message = if self
                .pending_backfill
                .as_ref()
                .is_some_and(|(_, base, run_id, _)| {
                    self.transcript_output.as_ref() == Some(base)
                        && self
                            .transcript
                            .run
                            .as_ref()
                            .is_some_and(|run| &run.run_id == run_id)
                }) {
                "实时转写存在缺口，自动补转正在等待其他任务或保存；退出后可从会议库手动重转。"
            } else {
                "实时转写存在缺口；可从会议库使用原录制另存重转，原稿会保留。"
            };
            toolbar = toolbar.child(
                div()
                    .w_full()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(SOFT))
                    .child(label(message)),
            );
        }
        if self
            .asr
            .as_ref()
            .is_some_and(|asr| asr.finished() && asr.events.is_empty())
            && self.transcript.run.is_none()
            && self.transcript.segments.is_empty()
            && let Some(source) = self.transcript.source.clone()
        {
            let base = self.transcript_output.clone();
            #[cfg(feature = "online-speaker")]
            let speaker_waiting = self.config.online_speakers
                && self
                    .online_speakers
                    .as_ref()
                    .is_some_and(|controller| !controller.can_replace());
            #[cfg(not(feature = "online-speaker"))]
            let speaker_waiting = false;
            toolbar = toolbar.child(
                button("retry-unstarted-file-asr", "用当前设置重试文件转写", false)
                    .disabled(
                        busy || self.active()
                            || self.transcript.primary_save_pending
                            || speaker_waiting,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.retry_unstarted_file(base.clone(), source.clone(), cx);
                    })),
            );
            if speaker_waiting {
                toolbar = toolbar.child(label("等待上一轮在线说话人收尾或保存后重试。"));
            }
        }
        if self.transcript.primary_save_pending || !self.transcript.output_warnings.is_empty() {
            toolbar = toolbar.child(
                div()
                    .w_full()
                    .p_3()
                    .bg(rgb(SOFT))
                    .rounded_lg()
                    .child(label(if self.transcript.output_warnings.is_empty() {
                        if self.transcript_output.is_none() {
                            "逐字稿未绑定保存位置，请先另存恢复副本。".into()
                        } else {
                            "逐字稿主文件尚未保存，请重试同步。".into()
                        }
                    } else {
                        self.transcript.output_warnings.join("\n")
                    }))
                    .child(
                        button("retry-transcript-sync", "重试同步文件", false)
                            .disabled(busy || self.transcript_output.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retry_transcript_sync();
                                cx.notify();
                            })),
                    )
                    .when(self.transcript.primary_save_pending, |d| {
                        d.child(
                            button(
                                "save-unbound-transcript-recovery",
                                "另存转写恢复副本…",
                                false,
                            )
                            .disabled(busy || self.active())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_unbound_transcript_recovery(cx);
                            })),
                        )
                    }),
            );
        }
        if self
            .transcript
            .output_warnings
            .iter()
            .any(|warning| warning.starts_with("识别历史版本未归档："))
            && let Some(base) = self.transcript_output.clone()
        {
            if self
                .transcript
                .run
                .as_ref()
                .is_some_and(|run| run.extra.contains_key("pending_archive_sha256"))
            {
                let repair_base = base.clone();
                toolbar = toolbar.child(
                    button("repair-transcript-archive", "修复原始识别版本", false)
                        .ghost()
                        .disabled(busy || self.active())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.active()
                                || this.transcription_busy()
                                || this.transcript_output.as_ref() != Some(&repair_base)
                            {
                                return;
                            }
                            let snapshot = this.transcript.clone();
                            let base = repair_base.clone();
                            this.historical_cancelled =
                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let cancelled = this.historical_cancelled.clone();
                            this.historical_checking = true;
                            this.transcription_status = "正在核对并修复原始识别版本…".into();
                            cx.notify();
                            cx.spawn(async move |entity, cx| {
                                let task_base = base.clone();
                                let task_snapshot = snapshot.clone();
                                let result = cx
                                    .background_executor()
                                    .spawn(async move {
                                        anyhow::ensure!(
                                            !cancelled.load(std::sync::atomic::Ordering::Relaxed),
                                            "已取消登记"
                                        );
                                        opencap_asr::repair_archive_registration(
                                            &task_base,
                                            &task_snapshot,
                                        )
                                    })
                                    .await;
                                let _ = entity.update(cx, |this, cx| {
                                    this.historical_checking = false;
                                    if this.transcript_output.as_ref() == Some(&base) {
                                        if result.is_ok() {
                                            this.invalidate_transcript_history();
                                        }
                                        match result {
                                            Ok(_)
                                                if this
                                                    .transcript
                                                    .run
                                                    .as_ref()
                                                    .map(|run| &run.run_id)
                                                    == snapshot
                                                        .run
                                                        .as_ref()
                                                        .map(|run| &run.run_id) =>
                                            {
                                                if let Some(run) = &mut this.transcript.run {
                                                    run.extra.remove("pending_archive_sha256");
                                                }
                                                this.transcript.output_warnings.retain(|warning| {
                                                    !warning.starts_with("识别历史版本未归档：")
                                                });
                                                this.transcript.primary_save_pending = true;
                                                this.retry_transcript_sync();
                                            }
                                            Ok(_) => {
                                                this.transcription_status =
                                                    "历史版本已登记；当前稿已变化，请核对保存状态。"
                                                        .into()
                                            }
                                            Err(error) => {
                                                this.transcription_status =
                                                    format!("未修复历史版本登记：{error:#}")
                                            }
                                        }
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        })),
                );
            }
            toolbar = toolbar.child(
                button(
                    "save-transcript-recovery-snapshot",
                    "另存当前稿恢复副本",
                    false,
                )
                .ghost()
                .disabled(busy || self.active())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.active()
                        || this.transcription_busy()
                        || this.transcript_output.as_ref() != Some(&base)
                    {
                        return;
                    }
                    let snapshot = this.transcript.clone();
                    let base = base.clone();
                    this.historical_cancelled =
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let cancelled = this.historical_cancelled.clone();
                    this.historical_checking = true;
                    this.transcription_status = "正在另存当前稿恢复副本…".into();
                    cx.notify();
                    cx.spawn(async move |entity, cx| {
                        let task_base = base.clone();
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                anyhow::ensure!(
                                    !cancelled.load(std::sync::atomic::Ordering::Relaxed),
                                    "已取消保存恢复副本"
                                );
                                opencap_asr::archive_current_recovery(&task_base, &snapshot)
                            })
                            .await;
                        let _ = entity.update(cx, |this, cx| {
                            this.historical_checking = false;
                            if this.transcript_output.as_ref() == Some(&base) {
                                if result.is_ok() {
                                    this.invalidate_transcript_history();
                                }
                                this.transcription_status = match result {
                                    Ok(path) => format!(
                                        "当前稿恢复副本已保存至 {}；原始识别归档仍未修复。",
                                        path.display()
                                    ),
                                    Err(error) => {
                                        format!("恢复副本未完成：{error:#}；已有文件保留。")
                                    }
                                };
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                })),
            );
        }
        if self.transcript_tools_open
            && let Some(base) = &self.transcript_output
        {
            let run_detail = self.transcript.run.as_ref().map(|run| {
                let integrity = match run.extra.get("input_integrity").and_then(|v| v.as_str()) {
                    Some("hash_matched_before_and_after") => "转录前后指纹一致",
                    Some("changed_during_transcription") => "转录期间媒体发生变化",
                    Some("verification_unavailable_after_transcription") => "转录后未能复核",
                    Some("not_checked_after_partial_input") => "任务未完整处理，未复核",
                    Some("fingerprint_unavailable") => "无法读取媒体指纹",
                    _ => "未记录",
                };
                let input_audio = run.extra.get("input_audio").map(|v| {
                    if let Some(index) = v.get("audio_stream_index").and_then(|n| n.as_u64()) {
                        format!("音轨 {index} · 原始 {} Hz / {} 声道 · 转录输入 {} Hz / {} 声道 · {}",
                            v.get("decoded_sample_rate").and_then(|n| n.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "未知".into()),
                            v.get("decoded_channels").and_then(|n| n.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "未知".into()),
                            v.get("output_sample_rate").and_then(|n| n.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "未知".into()),
                            v.get("output_channels").and_then(|n| n.as_u64()).map(|n| n.to_string()).unwrap_or_else(|| "未知".into()),
                            match v.get("container_origin_known").and_then(|value| value.as_bool()) {
                                Some(true) => "容器时间原点已记录",
                                Some(false) => "容器未提供时间原点，按零点处理；需核对定位",
                                None => "容器时间原点状态未记录",
                            })
                    } else if v.get("source").and_then(|s| s.as_str()) == Some("mixed_recording") {
                        "录制时的混合音频流 · 48 kHz 单声道".into()
                    } else { "未记录".into() }
                }).unwrap_or_else(|| "未记录".into());
                let revision = match run.extra.get("revision_effective").and_then(|v| v.as_str()) {
                    Some("ready") if run.extra.get("revision_fingerprint_policy").and_then(|v| v.as_str())
                        == Some("verify_before_after_load") => "模型已就绪，加载前后指纹已核对；逐句结果见复核状态",
                    Some("ready") => "模型已就绪，加载时指纹核对未记录；逐句结果见复核状态",
                    Some("loading") if run.state != "running" => "结束时模型仍未确认就绪",
                    Some("loading") => "模型加载中",
                    Some("failed") => "模型不可用，保留流式原文",
                    Some("not_scheduled") => "已请求但未安排",
                    Some("disabled") => "未请求",
                    _ if run.revision_scheduled => "已安排，实际状态未记录",
                    _ => "未记录",
                };
                let fingerprint_source = match run.extra.get("model_fingerprint")
                    .and_then(|value| value.get("cache_key"))
                    .and_then(|value| value.as_str())
                {
                    Some("fresh_file_reads") => "本次重新读取文件",
                    Some("process_cache_length_and_mtime") => "进程缓存（按文件大小和修改时间）",
                    Some("mixed_fresh_and_process_cache_length_and_mtime") => "部分重新读取，部分使用进程缓存",
                    _ => "未记录",
                };
                let known_weights = match run.extra.get("known_model_weights_match").and_then(|value| value.as_bool()) {
                    Some(true) => "与固定参考权重一致（不含 VAD）",
                    Some(false) => "与固定参考权重不同，可能是自定义或已更改文件",
                    None => "未记录或没有固定参考哈希",
                };
                let component_match = |key: &str| match run.extra.get(key).and_then(|value| value.as_bool()) {
                    Some(true) => "匹配固定参考",
                    Some(false) => "与固定参考不同",
                    None => "未记录或无参考",
                };
                let load_check = |key: &str| match run.extra.get(key).and_then(|value| value.as_str()) {
                    Some("before_after_load_matched") => "加载前后指纹一致",
                    Some("not_applicable") => "不适用",
                    _ => "未确认（包括旧记录）",
                };
                let timing = |key: &str| run.extra.get("block_processing_timing")
                    .and_then(|value| value.get(key))
                    .and_then(|value| Some((value.get("count")?.as_u64()?, value.get("mean_ms")?.as_f64()?, value.get("p95_upper_ms")?.as_f64()?)))
                    .map(|(count, mean, p95)| format!("{count} 块，平均 {mean:.2} ms，P95 上界 {p95:.2} ms"))
                    .unwrap_or_else(|| "未记录".into());
                let feed_ratio = run.extra.get("block_processing_timing")
                    .and_then(|value| value.get("feed_wall_time_over_audio"))
                    .and_then(|value| value.as_f64())
                    .map(|value| format!("{value:.3}"))
                    .unwrap_or_else(|| "未记录".into());
                let drain_errors = run.extra.get("drain_errors").and_then(|value| value.as_array())
                    .map(|errors| errors.iter().filter_map(|value| value.as_str()).collect::<Vec<_>>().join("；"))
                    .filter(|message| !message.is_empty()).unwrap_or_else(|| "无记录".into());
                format!(
                    "本次识别配置 · 运行 {} · 状态 {}\n结束信息：{}\n收尾错误：{}\n后端 {} {} · 模型 {}\n单文件权重哈希 {} · 文件组合指纹 {}\n指纹来源 {} · {}\n加载核对：主模型 {} · 标点模型 {}\n设备 {} · 线程 {} · 请求语言 {} · 后端语言模式 {} · 识别语言未核实\n请求标点 {} · 实际标点 {} · 句段复核 {}\n文本规范化：{}\n复核语言与规范化配置：{}\n复核权重：{} · 标点权重：{}\n输入媒体 SHA-256 {} · {}\n资产指纹登记：{}\n输入音频：{}\n逐块归一化：{}\n同步识别处理：{}\n同步识别耗时 / 输入音频时长：{}\n耗时不包括解码、异步复核或 GPU 完成时间。\n热词请求：{}\n实际接受热词：{}",
                    run.run_id,
                    run.state,
                    run.extra.get("termination_error").or_else(|| run.extra.get("input_error"))
                        .and_then(|value| value.as_str()).unwrap_or("无记录"),
                    drain_errors,
                    run.backend,
                    run.runtime_version,
                    run.model_id,
                    run.model_sha256.as_deref().unwrap_or("未记录"),
                    run.extra.get("model_fingerprint").and_then(|v| v.get("manifest_sha256")).and_then(|v| v.as_str()).unwrap_or("未记录"),
                    fingerprint_source,
                    known_weights,
                    load_check("model_fingerprint_verification"),
                    load_check("punctuation_fingerprint_verification"),
                    run.provider,
                    run.threads,
                    run.requested_language,
                    run.extra.get("language_mode_sent").and_then(|value| value.as_str())
                        .unwrap_or(if run.effective_language.is_some() { "旧字段语义未核实" } else { "未记录" }),
                    if run.punctuation_requested { "开启" } else { "关闭" },
                    run.punctuation_effective,
                    revision,
                    match run.extra.get("itn_requested").and_then(|value| value.as_bool()) {
                        Some(true) => "已向后端请求 ITN；效果未核实",
                        Some(false) => "未请求 ITN；效果未核实",
                        None => "未记录；效果未核实",
                    },
                    match (run.extra.get("revision_language_mode_configured").and_then(|value| value.as_str()),
                        run.extra.get("revision_itn_configured").and_then(|value| value.as_bool())) {
                        (Some(mode), Some(true)) => format!("{mode}，请求 ITN；是否就绪见复核状态，效果未核实"),
                        _ => "未记录或不适用".into(),
                    },
                    component_match("known_revision_weights_match"),
                    component_match("known_punctuation_weights_match"),
                    run.extra.get("input_media_fingerprint").and_then(|v| v.get("sha256")).and_then(|v| v.as_str()).unwrap_or("未记录"),
                    integrity,
                    run.extra.get("asset_fingerprint_warning").and_then(|v| v.as_str()).unwrap_or("无错误记录"),
                    input_audio,
                    timing("normalization"),
                    timing("recognizer_feed"),
                    feed_ratio,
                    run.requested_hotwords,
                    run.accepted_hotwords.as_ref().map(|words| words.join("、")).unwrap_or_else(|| "未记录".into()),
                )
            }).unwrap_or_else(|| "本次识别配置：历史版本未记录。".into());
            toolbar = toolbar.child(
                div()
                    .w_full()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(BG))
                    .child(label(run_detail)),
            );
            if let (Some(run), Some(source)) =
                (self.transcript.run.clone(), self.transcript.source.clone())
            {
                let base = base.clone();
                toolbar = toolbar.child(
                    button(
                        "rerun-historical-current",
                        "按已记录模型参数另存重转",
                        false,
                    )
                    .disabled(busy || self.active())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.start_historical_check(base.clone(), source.clone(), run.clone(), cx);
                    })),
                );
            }
            let histories: &[PathBuf] =
                if self.transcript_history_loaded_base.as_ref() == Some(base) {
                    &self.transcript_history_files
                } else {
                    &[]
                };
            if self.transcript_history_loaded_base.as_ref() != Some(base) {
                toolbar = toolbar.child(label("正在后台读取历史识别版本…"));
            } else if let Some(error) = self
                .transcript_history_error
                .as_ref()
                .filter(|_| self.transcript_history_loaded_base.as_ref() == Some(base))
            {
                toolbar = toolbar.child(label(format!("无法读取历史识别版本：{error}")));
            }
            if self.transcript_history_loaded_base.as_ref() == Some(base) {
                let refresh_base = base.clone();
                let refresh_epoch = self.transcript_history_epoch;
                toolbar = toolbar.child(
                    button("refresh-transcript-history", "刷新历史版本", false)
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.transcript_output.as_ref() == Some(&refresh_base)
                                && this.transcript_history_epoch == refresh_epoch
                            {
                                this.invalidate_transcript_history();
                            }
                            cx.notify();
                        })),
                );
            }
            if !histories.is_empty() {
                const HISTORY_PER_PAGE: usize = 8;
                let pages = histories.len().saturating_sub(1) / HISTORY_PER_PAGE + 1;
                let page = self.transcript_history_page.min(pages - 1);
                let mut history = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label(format!("历史识别版本 · {} 份", histories.len())));
                if !self.transcript_history_unavailable.is_empty() {
                    history =
                        history.child(label("不可访问的已登记版本仍保留；恢复文件后可刷新列表。"));
                }
                let mut entries = div().flex().flex_wrap().gap_2();
                for (index, path) in histories
                    .iter()
                    .enumerate()
                    .skip(page * HISTORY_PER_PAGE)
                    .take(HISTORY_PER_PAGE)
                {
                    let path = path.clone();
                    let unavailable = self.transcript_history_unavailable.contains(&path);
                    let recovery = path.file_name().is_some_and(|name| {
                        name.to_string_lossy().contains(".transcript.run-recovery-")
                    });
                    let rerun_path = path.clone();
                    let history_base = base.clone();
                    let view_base = base.clone();
                    let history_epoch = self.transcript_history_epoch;
                    entries = entries
                        .child(
                            button(
                                ("open-transcript-history", index),
                                format!(
                                    "{}{} {}",
                                    if unavailable {
                                        "不可访问 · "
                                    } else {
                                        "查看"
                                    },
                                    if recovery { "恢复副本" } else { "版本" },
                                    index + 1
                                ),
                                false,
                            )
                            .ghost()
                            .disabled(unavailable)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.transcript_output.as_ref() == Some(&view_base)
                                        && this.transcript_history_loaded_base.as_ref()
                                            == Some(&view_base)
                                        && this.transcript_history_epoch == history_epoch
                                        && this.transcript_history_files.contains(&path)
                                        && !this.transcript_history_unavailable.contains(&path)
                                    {
                                        this.open_history_text_document(
                                            view_base.clone(),
                                            path.clone(),
                                            window,
                                            cx,
                                        );
                                    } else {
                                        this.transcription_status =
                                            "会议或历史版本列表已变化，请重新选择。".into();
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            button(
                                ("rerun-transcript-history", index),
                                format!(
                                    "按{} {} 的参数重转",
                                    if recovery { "恢复副本" } else { "版本" },
                                    index + 1
                                ),
                                false,
                            )
                            .ghost()
                            .disabled(busy || self.active() || unavailable)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.transcript_output.as_ref() == Some(&history_base)
                                        && this.transcript_history_loaded_base.as_ref()
                                            == Some(&history_base)
                                        && this.transcript_history_epoch == history_epoch
                                        && this.transcript_history_files.contains(&rerun_path)
                                        && !this
                                            .transcript_history_unavailable
                                            .contains(&rerun_path)
                                    {
                                        this.start_historical_archive(
                                            history_base.clone(),
                                            rerun_path.clone(),
                                            cx,
                                        );
                                    } else {
                                        this.transcription_status =
                                            "会议或历史版本列表已变化，请重新选择。".into();
                                        cx.notify();
                                    }
                                },
                            )),
                        );
                }
                history = history.child(entries);
                if pages > 1 {
                    let previous_base = base.clone();
                    let next_base = previous_base.clone();
                    let history_epoch = self.transcript_history_epoch;
                    history = history.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                button("history-previous", "上一组", false)
                                    .disabled(page == 0)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.transcript_output.as_ref() == Some(&previous_base)
                                            && this.transcript_history_epoch == history_epoch
                                        {
                                            this.transcript_history_page = page.saturating_sub(1);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(label(format!("{} / {}", page + 1, pages)))
                            .child(
                                button("history-next", "下一组", false)
                                    .disabled(page + 1 >= pages)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.transcript_output.as_ref() == Some(&next_base)
                                            && this.transcript_history_epoch == history_epoch
                                        {
                                            this.transcript_history_page =
                                                (page + 1).min(pages - 1);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    );
                }
                toolbar = toolbar.child(history);
            }
            let word = base.with_extension("transcript.docx");
            let plain = base.with_extension("transcript.txt");
            let srt = base.with_extension("srt");
            toolbar = toolbar.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        primary_button("export-transcript-word", "导出逐字稿 Word…")
                            .disabled(busy || !ready)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(base) = &this.transcript_output {
                                    match this.transcript.export_word(base) {
                                        Ok(()) => {
                                            this.export_document(
                                                base.with_extension("transcript.docx"),
                                                cx,
                                            );
                                        }
                                        Err(e) => {
                                            this.transcription_status =
                                                format!("逐字稿导出失败：{e}")
                                        }
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        button("open-transcript-word", "打开逐字稿 Word", false)
                            .disabled(!word.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&word);
                            }),
                    )
                    .child(
                        button("open-transcript", "查看纯文本 TXT", false)
                            .disabled(!plain.exists())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_text_document(plain.clone(), window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("open-subtitles", "打开字幕 SRT", false)
                            .ghost()
                            .disabled(!srt.exists())
                            .on_click(move |_, _, _| {
                                let _ = opencap_platform::open_path(&srt);
                            }),
                    ),
            );
        }

        toolbar = toolbar.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    primary_button("to-minutes", "AI 对话与纪要")
                        .disabled(!ready || self.active() || busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.meeting_page = workspace::MeetingPage::Minutes;
                            this.page_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        })),
                )
                .child(
                    button("correct-transcript", "AI 校正全文", false)
                        .disabled(!ready || self.active() || busy)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.request_transcript_correction(cx)),
                        ),
                )
                .when(self.transcript.correction.is_some(), |d| {
                    d.child(
                        button("restore-transcript-original", "恢复识别原文", false)
                            .disabled(busy || self.active())
                            .on_click(
                                cx.listener(|this, _, _, cx| this.restore_transcript_original(cx)),
                            ),
                    )
                })
                .when(self.ai.correction_task.is_some(), |d| {
                    d.child(
                        button("cancel-transcript-correction", "停止校正", false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.ai
                                    .cancelled
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                                cx.notify();
                            }),
                        ),
                    )
                })
                .when(self.asr.as_ref().is_some_and(|a| !a.finished()), |d| {
                    d.child(
                        button(
                            "cancel-asr",
                            if self.asr.as_ref().is_some_and(|asr| asr.cancel_requested()) {
                                "正在取消…"
                            } else {
                                "取消转录"
                            },
                            false,
                        )
                        .danger()
                        .disabled(self.asr.as_ref().is_some_and(|asr| asr.cancel_requested()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(asr) = &mut this.asr {
                                asr.cancel();
                                this.transcription_status =
                                    "正在取消转录；等待当前处理结束，已识别内容将保留。".into();
                            }
                            cx.notify();
                        })),
                    )
                }),
        );
        if self.historical_checking {
            toolbar = toolbar.child(
                button(
                    "cancel-historical-check",
                    if self
                        .historical_cancelled
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        "正在取消…"
                    } else {
                        "取消当前检查"
                    },
                    false,
                )
                .disabled(
                    self.historical_cancelled
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.historical_cancelled
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    this.transcription_status = "正在取消，等待当前步骤结束…".into();
                    cx.notify();
                })),
            );
        }
        toolbar = toolbar
            .child(
                button("transcript-files", "导出与文件", self.transcript_tools_open)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transcript_tools_open = !this.transcript_tools_open;
                        cx.notify();
                    })),
            )
            .child(
                button(
                    "toggle-speaker-panel",
                    if self.speaker_panel_open {
                        "返回文字".to_owned()
                    } else {
                        format!("说话人 · {speakers} 组")
                    },
                    self.speaker_panel_open,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.speaker_panel_open = !this.speaker_panel_open;
                    this.transcript_media_tab = false;
                    cx.notify();
                })),
            );
        let document = ui::card()
            .flex_1()
            .h_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "逐字稿",
                        if self.transcript.correction.is_some() {
                            "AI 全文校正稿 · 时间与说话人保留，可恢复识别原文"
                        } else {
                            "语音识别原文，保留时间与说话人标记"
                        },
                    ))
                    .child(ui::badge(
                        format!("{} 句", self.transcript.segments.len()),
                        MUTED,
                        BG,
                    )),
            )
            .child(
                div()
                    .id("transcript-status-scroll")
                    .max_h(px(36.))
                    .overflow_y_scroll()
                    .flex_shrink_0()
                    .child(label(format!(
                        "{}{}",
                        self.transcription_status,
                        if self.transcription_progress > 0.0 && self.transcription_progress < 1.0 {
                            format!(" · {:.0}%", self.transcription_progress * 100.)
                        } else {
                            String::new()
                        }
                    ))),
            )
            .when(
                self.transcription_clock.has_run() || self.transcript.processing_seconds.is_some(),
                |d| {
                    let elapsed = if self.transcription_clock.has_run() {
                        self.transcription_clock.elapsed()
                    } else {
                        task::saved_duration(self.transcript.processing_seconds)
                    };
                    d.child(label(format!(
                        "转录已用时 {}",
                        task::duration_text(elapsed)
                    )))
                },
            )
            .when(
                self.asr.as_ref().is_some_and(|a| !a.finished()) || self.preparing_model.is_some(),
                |d| {
                    d.child(
                        gpui::component::progress::Progress::new("transcript-inline-progress")
                            .value((self.transcription_progress * 100.) as f32)
                            .loading(self.transcription_progress <= 0.),
                    )
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(self.transcript_view.clone()),
            );
        let speaker_panel = ui::card()
            .child(ui::section_title("说话人分析", "会后在本机完成，可选步骤"))
            .child(label("首次需下载约 41 MB 模型；下载完成后直接在本机使用。"))
            .child(ui::badge(
                if speakers == 0 {
                    "尚未分析".into()
                } else {
                    format!("已识别 {speakers} 组")
                },
                ACCENT,
                SOFT,
            ))
            .child(label(
                "已知人数时填写人数；0 表示自动判断。编号不代表姓名，结果需要核对。",
            ))
            .child(label("实际参会人数 · 已知时请填写"))
            .child(Input::new(&self.speaker_count).w_full().disabled(busy))
            .child(label("自动模式合并阈值 · 人数已知时无需调整"))
            .child(Input::new(&self.speaker_threshold).w_full().disabled(busy))
            .child(label("CPU 使用方式"))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [(1, "省资源"), (2, "双线程"), (4, "四线程")]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (threads, title))| {
                            button(
                                ("speaker-threads", i),
                                title,
                                self.speaker_threads == threads,
                            )
                            .disabled(busy)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.speaker_threads = threads;
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .when(
                self.speaker_clock.has_run() || self.transcript.speaker_analysis_seconds.is_some(),
                |d| {
                    let elapsed = if self.speaker_clock.has_run() {
                        self.speaker_clock.elapsed()
                    } else {
                        task::saved_duration(self.transcript.speaker_analysis_seconds)
                    };
                    d.child(label(format!(
                        "分析已用时 {}",
                        task::duration_text(elapsed)
                    )))
                },
            )
            .child(
                button("distinguish-speakers", "分析 / 更新说话人", false)
                    .w_full()
                    .disabled(self.active() || busy || !ready)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_speakers(cx);
                        cx.notify();
                    })),
            )
            .child(
                button("open-speaker-models", "模型文件夹", false)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let path = model_root().join("diarization");
                        if let Err(e) = std::fs::create_dir_all(&path)
                            .map_err(anyhow::Error::from)
                            .and_then(|_| opencap_platform::open_path(&path))
                        {
                            this.transcription_status = format!("无法打开模型文件夹：{e:#}");
                        }
                        cx.notify();
                    })),
            )
            .when(self.speaker_task.is_some(), |d| {
                d.child(
                    button("cancel-speakers", "取消分析", false)
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.speaker_cancelled
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            this.transcription_status = "正在取消；已完成模型和原稿保留…".into();
                            cx.notify();
                        })),
                )
            })
            .child(label(
                "分析完成后，生成的纪要可按说话人组织。重新分析可能改变分组，请核对姓名。",
            ))
            .child(label(self.transcript.speaker_memory_status.clone()))
            .child(self.speakers_view.clone())
            .child(self.speaker_history_comparison(cx));
        #[cfg(feature = "online-speaker")]
        let speaker_panel = speaker_panel.when(!self.online_speaker_status.is_empty(), |d| {
            d.child(label(self.online_speaker_status.clone()))
        });
        #[cfg(feature = "online-speaker")]
        let speaker_panel = speaker_panel.when(
            self.online_speakers
                .as_ref()
                .is_some_and(|controller| controller.can_cancel_file()),
            |d| {
                d.child(
                    button("cancel-file-online-speakers", "停止文件说话人分析", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(controller) = &mut this.online_speakers {
                                controller.cancel(
                                    "已停止文件说话人分析；已有结果保留，未处理部分待核对。",
                                );
                                this.online_speaker_status = controller.status.clone();
                            }
                            cx.notify();
                        })),
                )
            },
        );
        #[cfg(feature = "online-speaker")]
        let speaker_panel = {
            let recovery_dir = self
                .online_speakers
                .as_ref()
                .and_then(|controller| controller.recovery_dir.clone())
                .unwrap_or_else(|| self.config.output_dir.join(".opencap-recovery"));
            speaker_panel.when(recovery_dir.is_dir(), |d| {
                d.child(
                    button("open-speaker-recovery", "打开说话人恢复文件夹", false)
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(error) = opencap_platform::open_path(&recovery_dir) {
                                this.online_speaker_status =
                                    format!("无法打开恢复文件夹：{error:#}");
                            }
                            cx.notify();
                        })),
                )
            })
        };
        let wide = ui::two_columns(width);
        if wide {
            toolbar = toolbar.child(
                button("swap-meeting-panes", "交换左右区域", false)
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.meeting_media_on_right = !this.config.meeting_media_on_right;
                        this.save();
                        cx.notify();
                    })),
            );
        } else {
            toolbar = toolbar
                .child(
                    button(
                        "show-meeting-media",
                        "媒体与纪要",
                        self.transcript_media_tab,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transcript_media_tab = true;
                        cx.notify();
                    })),
                )
                .child(
                    button(
                        "show-meeting-transcript",
                        "逐字稿",
                        !self.transcript_media_tab,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transcript_media_tab = false;
                        cx.notify();
                    })),
                );
        }

        let mut media_content =
            ui::card().child(ui::section_title("媒体与纪要", "回听原媒体，查看本场来源"));
        #[cfg(feature = "internal-player")]
        {
            if self.player.is_some() && self.player_base == self.transcript_output {
                media_content = media_content.child(self.player_controls(cx));
            } else if let (Some(base), Some(media)) = (
                self.transcript_output.clone(),
                self.transcript.source.clone(),
            ) {
                media_content = media_content.child(
                    button("open-transcript-media", "打开本场媒体", false)
                        .disabled(self.active())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_internal_player(base.clone(), media.clone());
                            cx.notify();
                        })),
                );
                if self.active() {
                    media_content =
                        media_content.child(label("录制期间暂停回放，避免声音被录入。"));
                }
            } else {
                media_content = media_content.child(label("本场没有可用的原媒体。"));
            }
        }
        #[cfg(not(feature = "internal-player"))]
        {
            media_content = media_content.child(label("此构建未启用内置回放。"));
        }
        if self.asr.as_ref().is_some_and(|asr| !asr.finished()) || self.live_notes.is_some() {
            media_content = media_content.child(self.live_notes_panel(cx));
        }
        let media_panel = div()
            .id("meeting-media-panel-scroll")
            .size_full()
            .min_h(px(0.))
            .overflow_y_scroll()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(media_content);
        let text_panel = if self.speaker_panel_open {
            div()
                .id("speaker-panel-scroll")
                .size_full()
                .min_h(px(0.))
                .overflow_y_scroll()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(speaker_panel)
        } else {
            div()
                .id("transcript-document-panel")
                .size_full()
                .min_h(px(0.))
                .child(document)
        };
        let body = if wide {
            use gpui::component::resizable::{h_resizable, resizable_panel};
            let media = resizable_panel()
                .size(px(width * 0.44))
                .size_range(px(280.)..px(width - 320.))
                .child(media_panel);
            let text = resizable_panel()
                .size(px(width * 0.56))
                .size_range(px(300.)..px(width - 300.))
                .child(text_panel);
            let split = h_resizable("transcript-workspace-split");
            let split = if self.config.meeting_media_on_right {
                split.child(text).child(media)
            } else {
                split.child(media).child(text)
            };
            div().flex_1().min_h(px(0.)).child(split)
        } else if self.transcript_media_tab {
            div().flex_1().min_h(px(0.)).child(media_panel)
        } else {
            div().flex_1().min_h(px(0.)).child(text_panel)
        };
        div()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .overflow_hidden()
            .child(
                div()
                    .id("transcript-toolbar-scroll")
                    .max_h(px(150.))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(toolbar),
            )
            .child(body)
    }
    pub(super) fn transcription_settings(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let live_model = ModelKind::from_live(self.config.live_asr_model);
        let offline_model = ModelKind::from_offline(self.config.offline_asr_model);
        let plan = self.asr_models.live_plan_for(
            self.config.live_asr_model,
            self.config.offline_asr_model,
            self.config.transcription_context_revision,
        );
        let view = ui::card()
            .child(div().text_lg().child("本地语音转录"))
            .child(label("实时转录会自动保存逐字稿；可主动开启增量纪要。音频与视频使用同一套转录和导出流程。"))
            .child(label("工作方式"))
            .child(div().flex().flex_wrap().gap_2().children(
                ["轻量实时", "实时与句段复核", "逐句模式", "仅文件转录"]
                    .into_iter().enumerate().map(|(index, title)| {
                        let selected = match index {
                            0 => self.config.realtime_transcription && self.config.live_asr_model == LiveAsrModel::Compact && !self.config.transcription_context_revision,
                            1 => self.config.realtime_transcription && self.config.live_asr_model == LiveAsrModel::Auto && self.config.transcription_context_revision,
                            2 => self.config.realtime_transcription && self.config.live_asr_model == LiveAsrModel::Sentence,
                            _ => !self.config.realtime_transcription,
                        };
                        button(("asr-work-mode", index), title, selected).disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                match index {
                                    0 => { this.config.realtime_transcription = true; this.config.live_asr_model = LiveAsrModel::Compact; this.config.transcription_context_revision = false; }
                                    1 => { this.config.realtime_transcription = true; this.config.live_asr_model = LiveAsrModel::Auto; this.config.transcription_context_revision = true; }
                                    2 => { this.config.realtime_transcription = true; this.config.live_asr_model = LiveAsrModel::Sentence; this.config.transcription_context_revision = false; }
                                    _ => this.config.realtime_transcription = false,
                                }
                                this.save();
                                this.transcription_status = "工作方式将在下次任务生效；模型需单独安装。".into();
                                cx.notify();
                            }))
                    }),
            ))
            .child(label("轻量实时需要流式模型；实时与句段复核需要流式模型及所选文件模型；逐句模式需要所选文件模型。选择方式不会自动下载，也不保证某种模型识别更准。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("live-asr-toggle", if self.config.realtime_transcription { "实时转录：开启" } else { "实时转录：关闭" }, self.config.realtime_transcription).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.realtime_transcription = !this.config.realtime_transcription; this.save(); cx.notify(); })))
                .child(button("context-revision", if self.config.transcription_context_revision { "断句校正：开启" } else { "断句校正：关闭" }, self.config.transcription_context_revision).disabled(busy).on_click(cx.listener(|this,_,_,cx| { this.config.transcription_context_revision = !this.config.transcription_context_revision; this.save(); cx.notify(); }))))
            .child(label("实时识别模型"))
            .child(div().flex().flex_wrap().gap_2().children(LiveAsrModel::ALL.into_iter().enumerate().map(|(i, model)| {
                button(("live-asr-model", i), model.label(), self.config.live_asr_model == model)
                    .disabled(busy)
                    .on_click(cx.listener(move |this,_,_,cx| { if !this.transcription_busy() && !this.active() { this.config.live_asr_model = model; this.save(); } cx.notify(); }))
            })))
            .child(label("自动优先使用轻量实时模型，未安装时尝试所选文件模型；标准约 76 MB、增强约 167 MB，会增加 CPU 和内存占用。实时模型以中文为主。"))
            .when_some(live_model, |d, model| d.child(button("download-selected-live", if self.asr_models.ready(model) { format!("{} · 已安装", model.label()) } else { format!("下载{}", model.label()) }, false)
                .disabled(busy || self.asr_models.ready(model))
                .on_click(cx.listener(move |this,_,_,cx| { this.prepare_asr_model(model); cx.notify(); }))))
            .child(label("文件转录与断句校正模型"))
            .child(div().flex().flex_wrap().gap_2().children(OfflineAsrModel::ALL.into_iter().enumerate().map(|(i, model)| {
                button(("offline-asr-model", i), model.label(), self.config.offline_asr_model == model)
                    .disabled(busy)
                    .on_click(cx.listener(move |this,_,_,cx| { if !this.transcription_busy() && !this.active() { this.config.offline_asr_model = model; this.save(); } cx.notify(); }))
            })))
            .child(label("SenseVoice 支持中英日韩粤；Paraformer 支持中英及部分方言，轻量约 82 MB、标准约 227 MB。识别效果取决于录音，没有一种模型始终最好。"))
            .child(button("download-selected-offline", if self.asr_models.ready(offline_model) { format!("{} · 已安装", offline_model.label()) } else { format!("下载{}", offline_model.label()) }, false)
                .disabled(busy || self.asr_models.ready(offline_model))
                .on_click(cx.listener(move |this,_,_,cx| { this.prepare_asr_model(offline_model); cx.notify(); })))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("punctuation-toggle", if self.config.transcription_punctuation { "自动标点：开启" } else { "自动标点：关闭" }, self.config.transcription_punctuation)
                    .disabled(busy)
                    .on_click(cx.listener(|this,_,_,cx| { this.config.transcription_punctuation = !this.config.transcription_punctuation; this.save(); cx.notify(); })))
                .child(button("download-punctuation", if self.asr_models.punctuation { "中英标点模型 · 已安装" } else { "下载中英标点模型 · 约 65–76 MB" }, false)
                    .disabled(busy || self.asr_models.punctuation)
                    .on_click(cx.listener(|this,_,_,cx| { this.prepare_punctuation_model(); cx.notify(); }))))
            .child(label("自动标点在本机运行：流式文字定期补标点，停顿和二遍结果再次处理；未安装模型时只补句末标点。模型可能判断错误，可关闭。"))
            .when(self.preparing_model.is_some() || self.transcription_status.starts_with("下载失败") || self.transcription_status.ends_with("已就绪"), |d| d.child(label(self.transcription_status.clone())))
            .when(self.preparing_model.is_some(), |d| {
                let estimate = self.transcription_clock.remaining().map_or_else(
                    || if self.transcription_clock.stalled() { " · 暂无进展，暂不估计剩余时间".into() } else { String::new() },
                    |remaining| format!(" · 预计剩余 {}", task::duration_text(remaining)),
                );
                d.child(label(format!("已用时 {}{}", task::duration_text(self.transcription_clock.elapsed()), estimate)))
            })
            .when(self.preparing_model.is_some(), |d| d.child(button("cancel-model-download", "取消模型下载", false)
                .disabled(self.prepare_cancelled.load(std::sync::atomic::Ordering::Relaxed))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.prepare_cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    this.transcription_status = "正在取消；等待当前网络读取结束后清理临时文件…".into();
                    cx.notify();
                }))))
            .child(label(plan.map(|p| p.description()).unwrap_or("所选实时模型尚不可用，请下载或切换模型")))
            .when_some(plan, |d, plan| d.child(label(plan.kind.capabilities().description())))
            .child(label("断句校正使用所选文件模型，运行时占用更多资源；未安装时实时识别继续，长句或积压时保留流式原文。文件模型不使用实时热词，字幕时间为整句范围；Paraformer 不使用语言提示。"))
            .child(label("实时热词只使用当前模型词表支持的词；不支持的词会跳过，不再阻止识别。切换模型在下次任务生效。"))
            .child(button("speech-vocabulary-settings", "配置全局词汇与语言", false).ghost().on_click(cx.listener(|this, _, _, cx| this.open_ai_settings(crate::settings::SettingsPage::Vocabulary, cx))))
            .child(button("stream-file-demo", "按原速试播所选模型", false).ghost().disabled(busy).on_click(cx.listener(|this,_,_,cx| this.choose_transcription_file(true,cx))));
        #[cfg(feature = "online-speaker")]
        let view = view.child(self.online_speaker_settings(cx));
        view
    }

    #[cfg(feature = "online-speaker")]
    fn choose_online_speaker_file(&mut self, library: bool, cx: &mut Context<Self>) {
        if self.active() || self.transcription_busy() {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                if library {
                    "选择可信的 NeMo 原生库"
                } else {
                    "选择固定版本的说话人模型"
                }
                .into(),
            ),
        });
        cx.spawn(async move |entity, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = entity.update(cx, |this, cx| {
                if this.active() || this.transcription_busy() {
                    return;
                }
                if library {
                    this.config.online_speaker_library = Some(path);
                } else {
                    this.config.online_speaker_model = Some(path);
                }
                this.save();
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(feature = "online-speaker")]
    fn online_speaker_settings(&self, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.transcription_busy();
        let library = self
            .config
            .online_speaker_library
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "尚未选择".into());
        let model = self
            .config
            .online_speaker_model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "尚未选择".into());
        div().flex().flex_col().gap_2().pt_3()
            .child(ui::section_title("在线说话人 · 实验", "独立本机流；默认关闭"))
            .child(label("仅加载你选择并信任的原生库；模型文件会校验固定 SHA-256。最多 8 个匿名槽，标签可能修订或合并，姓名需自行核对。"))
            .child(label(format!("原生库：{library}")))
            .child(label(format!("模型：{model}")))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("online-speaker-library", "选择可信原生库", false).disabled(busy).on_click(cx.listener(|this, _, _, cx| this.choose_online_speaker_file(true, cx))))
                .child(button("online-speaker-model", "选择说话人模型", false).disabled(busy).on_click(cx.listener(|this, _, _, cx| this.choose_online_speaker_file(false, cx))))
                .child(button("online-speaker-toggle", if self.config.online_speakers { "在线说话人：开启" } else { "在线说话人：关闭" }, self.config.online_speakers)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| { this.config.online_speakers = !this.config.online_speakers; this.save(); cx.notify(); }))))
            .child(label("此路线尚未完成同样本准确率和性能评测，不会改变默认转写模型。"))
    }
}

#[cfg(test)]
mod model_output_tests {
    use super::transcription_output;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn retranscription_of_recording_gets_a_separate_output_but_backfill_keeps_identity() {
        let root = std::env::temp_dir().join(format!(
            "opencap-model-output-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("meeting.mp4");
        std::fs::write(&source, b"media").unwrap();
        std::fs::write(source.with_extension("recording.json"), b"{}").unwrap();
        let original = opencap_core::meeting_base(&source);
        assert_eq!(
            transcription_output(&root, &source, false).unwrap(),
            original
        );
        std::fs::write(original.with_extension("transcript.json"), b"original").unwrap();
        let separate = transcription_output(&root, &source, false).unwrap();
        assert_ne!(separate, original);
        assert_eq!(
            std::fs::read(original.with_extension("transcript.json")).unwrap(),
            b"original"
        );
        assert_eq!(
            transcription_output(&root, &source, true).unwrap(),
            original
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
