use super::*;
use anyhow::Context as _;
use gpui::component::input::Textarea;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

const MEETINGS_PER_PAGE: usize = 25;

/// Export only a uniquely registered managed recording. The meeting activity
/// guard prevents a cooperating rename from moving files during the export.
fn retry_recording_audio_export(
    base: &Path,
    expected_meeting_id: &str,
    source: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
    stage: &std::sync::atomic::AtomicU8,
) -> anyhow::Result<(PathBuf, bool)> {
    let _activity = opencap_core::managed_meeting_activity(base)?;
    let manifest = opencap_core::meeting_manifest(base)?.context("会议清单不可用")?;
    anyhow::ensure!(manifest.meeting_id == expected_meeting_id, "会议身份已变化");
    let candidates: Vec<_> = manifest
        .assets
        .iter()
        .filter(|asset| {
            asset.role == "recording_video"
                && asset.ownership == opencap_core::AssetOwnership::Managed
        })
        .collect();
    anyhow::ensure!(candidates.len() == 1, "会议录屏资产缺失或不唯一");
    let actual = opencap_core::resolve_asset_in_manifest(base, &manifest, &candidates[0].asset_id)?
        .context("会议录屏资产不可用")?;
    anyhow::ensure!(actual == source, "会议录屏路径已变化");
    let output = base.with_extension("m4a");
    match std::fs::symlink_metadata(&output) {
        Ok(_) => anyhow::bail!("目标音频已存在，未覆盖：{}", output.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("无法检查音频目标路径"),
    }
    let expected_hash = candidates[0]
        .extra
        .get("sha256")
        .map(|value| value.as_str().context("录屏资产指纹格式无效"))
        .transpose()?;
    let source_identity_verified = expected_hash.is_some();
    let expected_size = candidates[0]
        .extra
        .get("size_bytes")
        .map(|value| value.as_u64().context("录屏资产大小格式无效"))
        .transpose()?;
    stage.store(1, std::sync::atomic::Ordering::Release);
    let source_fingerprint = opencap_core::fingerprint_media(source, Some(cancelled))?;
    anyhow::ensure!(
        expected_hash.is_none_or(|hash| hash == source_fingerprint.sha256)
            && expected_size.is_none_or(|size| size == source_fingerprint.size_bytes),
        "录屏内容与资产记录不一致，未导出"
    );
    let name = output.file_name().context("音频文件名无效")?;
    let existing: Vec<_> = manifest
        .assets
        .iter()
        .filter(|asset| {
            asset.ownership == opencap_core::AssetOwnership::Managed
                && asset.path == Path::new(name)
        })
        .collect();
    anyhow::ensure!(
        existing.len() <= 1
            && existing
                .first()
                .is_none_or(|asset| asset.role == "recording_audio"),
        "目标路径已登记为其他资产，未导出"
    );
    stage.store(2, std::sync::atomic::Ordering::Release);
    opencap_media::export_audio_cancellable(source, &output, Some(cancelled))?;
    stage.store(3, std::sync::atomic::Ordering::Release);
    let after = opencap_core::fingerprint_media(source, Some(cancelled))
        .with_context(|| format!("音频已导出至 {}，但无法复核录屏指纹", output.display()))?;
    anyhow::ensure!(
        source_fingerprint == after,
        "音频已导出至 {}，但录屏内容在导出期间变化；请核对",
        output.display()
    );
    (|| -> anyhow::Result<()> {
        let latest = opencap_core::meeting_manifest(base)?.context("会议清单不可用")?;
        let mut matching = latest
            .assets
            .iter()
            .filter(|asset| asset.asset_id == candidates[0].asset_id);
        let current = matching.next().context("录屏资产已消失")?;
        anyhow::ensure!(
            latest.meeting_id == expected_meeting_id
                && matching.next().is_none()
                && current.role == "recording_video"
                && current.ownership == opencap_core::AssetOwnership::Managed
                && current.extra.get("sha256") == candidates[0].extra.get("sha256")
                && current.extra.get("size_bytes") == candidates[0].extra.get("size_bytes")
                && opencap_core::resolve_asset_in_manifest(base, &latest, &current.asset_id)?
                    == Some(source.to_path_buf()),
            "会议或录屏资产已变化"
        );
        Ok(())
    })()
    .with_context(|| {
        format!(
            "音频已导出至 {}，但无法核对录屏资产；文件保留",
            output.display()
        )
    })?;
    stage.store(4, std::sync::atomic::Ordering::Release);
    let asset_id = opencap_core::register_meeting_asset_checked(
        base,
        "recording_audio",
        opencap_core::AssetOwnership::Managed,
        Path::new(name),
        Some(expected_meeting_id),
    )
    .with_context(|| {
        format!(
            "音频已导出至 {}，但资产登记失败；文件保留",
            output.display()
        )
    })?
    .context("音频已导出，但没有登记资产编号")?;
    let registered =
        opencap_core::meeting_manifest(base)?.context("音频已导出，但登记后的会议清单不可用")?;
    anyhow::ensure!(
        registered.meeting_id == expected_meeting_id
            && registered
                .assets
                .iter()
                .filter(|asset| asset.asset_id == asset_id
                    && asset.role == "recording_audio"
                    && asset.ownership == opencap_core::AssetOwnership::Managed
                    && asset.path == Path::new(name))
                .count()
                == 1
            && opencap_core::resolve_asset_in_manifest(base, &registered, &asset_id)?
                == Some(output.clone()),
        "音频已导出至 {}，但登记结果已变化；文件保留",
        output.display()
    );
    let fingerprint = opencap_core::fingerprint_media(&output, Some(cancelled))
        .with_context(|| format!("音频已导出至 {}，但无法核对文件指纹", output.display()))?;
    opencap_core::record_asset_fingerprint(
        base,
        expected_meeting_id,
        &asset_id,
        &output,
        &fingerprint,
    )
    .with_context(|| format!("音频已导出至 {}，但资产指纹登记失败", output.display()))?;
    Ok((output, source_identity_verified))
}

/// Verify the stored chunk against the current run and every source slice.
fn verified_semantic_source<'a>(
    transcript: &'a opencap_asr::Transcript,
    source: &search::SemanticSource,
) -> Option<&'a opencap_asr::Segment> {
    let run = transcript.run.as_ref();
    if source.run_id.as_deref() != run.map(|run| run.run_id.as_str())
        || source.meeting_id.as_deref() != run.and_then(|run| run.meeting_id.as_deref())
        || source.asset_id.as_deref() != run.and_then(|run| run.input_asset_id.as_deref())
        || source.segments.is_empty()
        || source.segments.len() > 20_000
    {
        return None;
    }
    let mut segments = std::collections::HashMap::new();
    for (index, segment) in transcript.segments.iter().enumerate() {
        if segments.insert(segment.id, (index, segment)).is_some() {
            return None;
        }
    }
    let mut text = String::new();
    let mut first = None;
    let mut previous: Option<(usize, usize)> = None;
    for reference in &source.segments {
        let &(index, segment) = segments.get(&reference.id)?;
        if previous.is_some_and(|(prior_index, prior_end)| {
            index < prior_index || (index == prior_index && reference.start_byte < prior_end)
        }) {
            return None;
        }
        if !segment.is_confirmed()
            || segment.revision != reference.revision
            || segment.start_ms != reference.start_ms
            || segment.end_ms != reference.end_ms
            || format!("{:x}", Sha256::digest(segment.text.as_bytes())) != reference.text_sha256
        {
            return None;
        }
        let part = segment.text.get(reference.start_byte..reference.end_byte)?;
        if part.is_empty() {
            return None;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
        if text.len() > 16_000_000 {
            return None;
        }
        first.get_or_insert(segment);
        previous = Some((index, reference.end_byte));
    }
    (format!("{:x}", Sha256::digest(text.as_bytes())) == source.chunk_sha256).then_some(first?)
}

pub(super) struct NotesReload {
    base: PathBuf,
    editor_text: String,
    saved_text: String,
    saved_exists: bool,
    result: anyhow::Result<Option<String>>,
}

fn read_meeting_notes(base: &Path) -> anyhow::Result<Option<String>> {
    use std::io::Read;
    const LIMIT: u64 = 16_000_000;
    let path = base.with_extension("notes.txt");
    let mut file = match opencap_core::open_regular_file_for_read(&path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let before = file.metadata()?;
    anyhow::ensure!(before.len() <= LIMIT, "笔记超过 16 MB");
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "笔记超过 16 MB");
    let after = file.metadata()?;
    let current = opencap_core::open_regular_file_for_read(&path)?;
    anyhow::ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && same_file::Handle::from_file(current)? == identity,
        "读取期间笔记发生变化，请重试"
    );
    Ok(Some(String::from_utf8(bytes)?))
}

fn filter_date(text: &str, exclusive_end: bool) -> anyhow::Result<Option<i64>> {
    use chrono::TimeZone;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut date =
        chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").context("日期请使用 YYYY-MM-DD")?;
    if exclusive_end {
        date = date
            .checked_add_signed(chrono::Duration::days(1))
            .context("结束日期超出范围")?;
    }
    let local = date.and_hms_opt(0, 0, 0).context("日期无效")?;
    let instant = chrono::Local
        .from_local_datetime(&local)
        .earliest()
        .context("本地时区无法表示此日期")?;
    Ok(Some(instant.timestamp_millis()))
}
fn filter_minutes(text: &str) -> anyhow::Result<Option<i64>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value: u64 = text.parse().context("时长请输入非负整数分钟")?;
    anyhow::ensure!(value <= 5_256_000, "时长筛选超过范围");
    Ok(Some((value * 60_000) as i64))
}

pub(super) struct MeetingScan {
    root: PathBuf,
    files: Vec<MeetingFile>,
    error: Option<String>,
    notice: Option<String>,
    pending_imports: Option<Vec<PendingImport>>,
    relocated: Vec<opencap_core::RenamedMeeting>,
    persisted_extra: BTreeSet<PathBuf>,
    cancelled: bool,
}

#[derive(Clone)]
pub(super) struct PendingImport {
    stage: PathBuf,
    request: Option<opencap_core::PendingLegacyImport>,
    detail: Option<String>,
}

pub(super) struct EntityRename {
    base: PathBuf,
    title: String,
    mode: RenameMode,
    state: RenameState,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    migration_progress: std::sync::Arc<opencap_core::LegacyMigrationProgress>,
    shown_progress: (u8, u64),
    #[cfg(feature = "internal-player")]
    player: Option<PlayerResume>,
}
#[derive(Clone)]
enum RenameMode {
    Managed,
    Legacy {
        root: PathBuf,
        media: Option<PathBuf>,
        archive_media: bool,
    },
    ResumeLegacy {
        root: PathBuf,
        pending: opencap_core::PendingLegacyImport,
    },
}
enum RenameOutcome {
    Moved(opencap_core::RenamedMeeting, Option<String>),
    Migrated(opencap_core::LegacyMigration, Option<String>),
}
enum RenameState {
    WaitingForPlayer,
    Working(crossbeam_channel::Receiver<anyhow::Result<RenameOutcome>>),
}
#[cfg(feature = "internal-player")]
struct PlayerResume {
    media: PathBuf,
    snapshot: opencap_media::PlaybackSnapshot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MeetingPage {
    Library,
    ReadOnly,
    Transcript,
    Minutes,
    Notes,
}
pub(super) struct ReadOnlyMeeting {
    base: PathBuf,
    title: String,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
}
#[derive(Clone)]
pub(super) struct MeetingFile {
    pub base: PathBuf,
    pub media: Option<PathBuf>,
    pub(super) media_items: Vec<MeetingMedia>,
    media_is_candidate: bool,
    relink_asset_id: Option<String>,
    relink_is_candidate: bool,
    manifest_meeting_id: Option<String>,
    manifest_fingerprint: Option<opencap_core::MediaFingerprint>,
    pub modified: std::time::SystemTime,
    /// Recording start from the report when its basis is trustworthy; else None.
    pub recording_started_ms: Option<u64>,
    available: bool,
    summary_available: bool,
    media_available: Option<bool>,
    media_size: Option<u64>,
    manifest_available: bool,
    manifest_present: bool,
    manifest_read_error: bool,
    transcript_available: bool,
    transcript_read_error: bool,
    complete: Option<bool>,
    model: Option<&'static str>,
    title: String,
    preview: String,
    details: String,
    display_stamp: String,
}
#[derive(Clone)]
pub(super) struct MeetingMedia {
    pub path: PathBuf,
    pub role: String,
    pub available: bool,
}

fn list_meeting_media(
    base: &Path,
    manifest: Option<&opencap_core::MeetingManifest>,
    fallback: Option<&Path>,
) -> Vec<MeetingMedia> {
    let mut items = Vec::new();
    if let Some(manifest) = manifest {
        for asset in &manifest.assets {
            if !opencap_core::is_input_media_asset(asset) && asset.role != "candidate_media" {
                continue;
            }
            if let Ok(Some(path)) =
                opencap_core::resolve_asset_in_manifest(base, manifest, &asset.asset_id)
                && !items.iter().any(|item: &MeetingMedia| item.path == path)
            {
                let available = path.is_file();
                items.push(MeetingMedia {
                    path,
                    role: asset.role.clone(),
                    available,
                });
            }
        }
    } else {
        if let Some(path) = fallback {
            items.push(MeetingMedia {
                path: path.to_path_buf(),
                role: "source_media".into(),
                available: path.is_file(),
            });
        }
        // Older meetings have no manifest. Show same-name files as candidates,
        // without claiming that their timelines match the transcript.
        for extension in ["mp4", "m4a", "mov", "wav", "mp3"] {
            let path = base.with_extension(extension);
            if path.is_file() && !items.iter().any(|item| item.path == path) {
                items.push(MeetingMedia {
                    path,
                    role: "same_name_media".into(),
                    available: true,
                });
            }
        }
    }
    items
}
impl MeetingFile {
    /// True when the list can open this meeting's transcript and internal player.
    #[cfg(feature = "ui-smoke")]
    pub(super) fn reviewable(&self) -> bool {
        self.transcript_available && self.media_available == Some(true) && !self.media_is_candidate
    }
    fn age(&self) -> String {
        let hours = self.modified.elapsed().unwrap_or_default().as_secs() / 3_600;
        if hours == 0 {
            return "最近更新".into();
        }
        if hours < 24 {
            return format!("{hours} 小时前");
        }
        match hours / 24 {
            1 => "1 天前".into(),
            n => format!("{n} 天前"),
        }
    }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn status(&self) -> String {
        if !self.available {
            return "文件缺失 · 可重新定位".into();
        }
        let parts = [
            match self.complete {
                Some(true) => "逐字稿已完成",
                Some(false) => "逐字稿部分保存 · 可重新转录",
                // A new recording has no transcript yet; a present but
                // unreadable file is a different condition and must not be
                // reported as "waiting for transcription".
                None if self.transcript_read_error => "转写数据未能读取",
                None if self.transcript_available => "逐字稿状态未记录",
                None => "待转录",
            },
            if self.summary_available {
                "AI 纪要已生成"
            } else {
                "AI 纪要未生成"
            },
        ]
        .join("  ·  ");
        let parts = if self.media_available == Some(false) {
            format!("{parts}  ·  原媒体缺失 · 可重新定位")
        } else {
            parts
        };
        self.model
            .map(|model| format!("{parts}  ·  主模型：{model}"))
            .unwrap_or(parts)
    }
}
/// Local calendar month used by the library accordion. A trusted recording
/// start is preferred; modified time is only a visible fallback.
pub(super) fn meeting_month(meeting: &MeetingFile) -> String {
    let ms = meeting.recording_started_ms.unwrap_or_else(|| {
        meeting
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    });
    month_from_ms(ms)
}

fn meeting_group_key(meeting: &MeetingFile, weekly: bool) -> String {
    if !weekly {
        return meeting_month(meeting);
    }
    let time = meeting
        .recording_started_ms
        .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
        .unwrap_or(meeting.modified);
    week_key_for_time(time)
}

fn week_key_for_time(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%G-W%V")
        .to_string()
}

fn meeting_group_label(key: &str, weekly: bool) -> String {
    if weekly {
        key.split_once("-W")
            .and_then(|(year, week)| {
                week.parse::<u32>()
                    .ok()
                    .map(|week| format!("{year} 年第 {week} 周"))
            })
            .unwrap_or_else(|| key.to_owned())
    } else {
        month_label(key)
    }
}

fn month_from_ms(ms: u64) -> String {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%Y-%m")
        .to_string()
}

pub(super) fn month_label(key: &str) -> String {
    key.split_once('-')
        .and_then(|(year, month)| {
            month
                .parse::<u32>()
                .ok()
                .map(|month| format!("{year}年{month}月"))
        })
        .unwrap_or_else(|| key.to_owned())
}

/// Read a recording start only when the report states the media-origin basis.
/// This is intentionally stricter than the UI timestamp used for display.
fn meeting_recording_started_ms(base: &Path) -> Option<u64> {
    let path = base.with_extension("recording.json");
    let bytes = search::source_bytes_cancellable(&path, None)
        .ok()
        .flatten()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("recording_start_basis").and_then(|v| v.as_str())
        != Some("wall clock observed at media timeline origin")
    {
        return None;
    }
    value
        .get("recording_started_unix_ms")
        .and_then(|v| v.as_u64())
        .filter(|ms| *ms > 0)
}

fn index_path() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(path) = smoke::isolated_dir() {
        return path.join("meetings.json");
    }
    config_path().with_file_name("meetings.json")
}
fn search_index_path() -> PathBuf {
    index_path().with_file_name("meeting-search-v1.sqlite3")
}

fn index_guard() -> anyhow::Result<(std::sync::MutexGuard<'static, ()>, std::fs::File)> {
    static INDEX_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = INDEX_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = index_path().with_file_name("meetings.lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok((guard, file))
}

/// Merge rather than replacing remembered paths with only the files that are
/// currently reachable. An unreadable or malformed index is never overwritten.
fn persist_meeting_index(paths: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<()> {
    let _guard = index_guard()?;
    let path = index_path();
    let snapshot = search::source_bytes_cancellable(&path, None)?;
    let existing = snapshot
        .as_ref()
        .map(|bytes| serde_json::from_slice::<Vec<PathBuf>>(bytes))
        .transpose()?
        .unwrap_or_default();
    let mut merged: BTreeSet<PathBuf> = existing.into_iter().collect();
    merged.extend(paths);
    let bytes = serde_json::to_vec(&merged)?;
    if snapshot.as_deref() != Some(bytes.as_slice()) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if snapshot.is_some() {
            anyhow::ensure!(
                search::source_bytes_cancellable(&path, None)? == snapshot,
                "会议路径列表已被外部修改，未覆盖"
            );
            opencap_asr::atomic_write(&path, &bytes)?;
        } else {
            opencap_core::atomic_create_bytes(&path, &bytes)?;
        }
    }
    Ok(())
}

fn relocate_meeting_index(moved: &opencap_core::RenamedMeeting) -> anyhow::Result<()> {
    let _guard = index_guard()?;
    let path = index_path();
    let snapshot = search::source_bytes_cancellable(&path, None)?;
    let existing = snapshot
        .as_ref()
        .map(|bytes| serde_json::from_slice::<Vec<PathBuf>>(bytes))
        .transpose()?
        .unwrap_or_default();
    let mut paths: BTreeSet<_> = existing.into_iter().collect();
    paths.remove(&moved.old_base);
    paths.insert(moved.new_base.clone());
    let bytes = serde_json::to_vec(&paths)?;
    if snapshot.as_deref() != Some(bytes.as_slice()) {
        if snapshot.is_some() {
            anyhow::ensure!(
                search::source_bytes_cancellable(&path, None)? == snapshot,
                "会议路径列表已被外部修改，未覆盖"
            );
            opencap_asr::atomic_write(&path, &bytes)?;
        } else {
            opencap_core::atomic_create_bytes(&path, &bytes)?;
        }
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?
        .sync_all()?;
    opencap_core::sync_parent_directory(&path)?;
    Ok(())
}

fn finish_index_move(moved: &opencap_core::RenamedMeeting) -> anyhow::Result<()> {
    let path = moved.new_base.with_extension("manifest.json");
    anyhow::ensure!(
        std::fs::symlink_metadata(&path)?.file_type().is_file(),
        "目标会议清单不是普通文件"
    );
    let bytes = search::source_bytes_cancellable(&path, None)?.context("目标会议清单不存在")?;
    let manifest: opencap_core::MeetingManifest = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        manifest.meeting_id == moved.meeting_id
            && Some(manifest.managed_directory.as_os_str()) == moved.new_base.file_name(),
        "目标会议身份或目录已变化，未迁移索引"
    );
    relocate_meeting_index(moved)?;
    search::relocate_index(&search_index_path(), moved)?;
    opencap_core::acknowledge_meeting_rename(moved)
}
/// Scan only application artifacts, plus explicitly remembered imports. No media decoding.
fn display_stamp(base: &Path) -> (String, std::time::SystemTime) {
    let mut stamp = String::new();
    let mut latest = std::time::UNIX_EPOCH;
    for extension in [
        "transcript.json",
        "recording.json",
        "meeting.json",
        "manifest.json",
        "summary.history.json",
        "summary.live.json",
        "summary.md",
        "notes.txt",
    ] {
        let path = base.with_extension(extension);
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
            latest = latest.max(modified);
            let nanos = modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            stamp.push_str(&format!(
                "{extension}:{}:{nanos}:{}:{};",
                metadata.len(),
                metadata.file_type().is_file(),
                metadata.file_type().is_symlink()
            ));
        } else {
            stamp.push_str(&format!("{extension}:-;"));
        }
    }
    (stamp, latest)
}
#[cfg(test)]
fn discover_paths(root: &Path, remembered: Vec<PathBuf>) -> Vec<MeetingFile> {
    discover_paths_cached(
        root,
        remembered,
        Vec::new(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap()
    .0
}
fn discover_paths_cached(
    root: &Path,
    remembered: Vec<PathBuf>,
    previous: Vec<MeetingFile>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Option<(Vec<MeetingFile>, Vec<String>)> {
    fn scan(
        dir: &Path,
        depth: usize,
        bases: &mut BTreeSet<PathBuf>,
        cancelled: &std::sync::atomic::AtomicBool,
        warnings: &mut Vec<String>,
    ) {
        if depth == 0 || cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                if warnings.len() < 20 {
                    warnings.push(format!("无法读取目录 {}：{error}", dir.display()));
                }
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if warnings.len() < 20 {
                        warnings.push(format!("读取目录 {} 的条目失败：{error}", dir.display()));
                    }
                    continue;
                }
            };
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    if warnings.len() < 20 {
                        warnings.push(format!("无法读取 {} 的类型：{error}", path.display()));
                    }
                    continue;
                }
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".opencap-rename-") || name.starts_with(".opencap-import-") {
                    continue;
                }
                scan(&path, depth - 1, bases, cancelled, warnings);
            } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                for suffix in [".transcript.json", ".recording.json"] {
                    if let Some(stem) = name.strip_suffix(suffix) {
                        bases.insert(path.with_file_name(stem));
                    }
                }
            }
        }
    }
    let mut bases = remembered.into_iter().collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();
    scan(root, 3, &mut bases, cancelled, &mut warnings);
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if !warnings.is_empty() {
        bases.extend(previous.iter().map(|meeting| meeting.base.clone()));
    }
    let mut normalized = BTreeSet::new();
    for path in &bases {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        normalized.insert(opencap_core::meeting_base(path));
    }
    let bases = normalized;
    let mut files = Vec::new();
    let previous: std::collections::HashMap<_, _> = previous
        .into_iter()
        .map(|meeting| (meeting.base.clone(), meeting))
        .collect();
    for base in bases {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let transcript = base.with_extension("transcript.json");
        let report = base.with_extension("recording.json");
        let artifact = if transcript.exists() {
            &transcript
        } else {
            &report
        };
        let Ok(_metadata) = std::fs::metadata(artifact) else {
            if let Some(cached) = previous.get(&base) {
                let mut cached = cached.clone();
                cached.available = false;
                files.push(cached);
                continue;
            }
            files.push(MeetingFile {
                title: base
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                preview: "连接原文件所在磁盘后刷新，会议入口会继续保留。".into(),
                details: base.display().to_string(),
                display_stamp: String::new(),
                base,
                media: None,
                media_items: Vec::new(),
                media_is_candidate: false,
                relink_asset_id: None,
                relink_is_candidate: false,
                manifest_meeting_id: None,
                manifest_fingerprint: None,
                modified: std::time::UNIX_EPOCH,
                recording_started_ms: None,
                available: false,
                summary_available: false,
                media_available: None,
                media_size: None,
                manifest_available: false,
                manifest_present: false,
                manifest_read_error: false,
                transcript_available: false,
                transcript_read_error: false,
                complete: None,
                model: None,
            });
            continue;
        };
        let (stamp, modified) = display_stamp(&base);
        let manifest_result = opencap_core::read_manifest_snapshot(&base);
        let manifest_fingerprint = manifest_result.as_ref().ok().and_then(|snapshot| {
            snapshot
                .as_ref()
                .map(|(_, fingerprint)| fingerprint.clone())
        });
        let cached_media_metadata = previous
            .get(&base)
            .and_then(|cached| cached.media.as_ref())
            .and_then(|path| std::fs::metadata(path).ok());
        let cached_managed_path_valid = previous
            .get(&base)
            .and_then(|cached| cached.media.as_ref())
            .is_none_or(|media| {
                manifest_result
                    .as_ref()
                    .ok()
                    .and_then(|snapshot| snapshot.as_ref())
                    .is_none_or(|(manifest, _)| {
                        let root = base.parent().unwrap_or(Path::new("."));
                        let mut matches = manifest.assets.iter().filter(|asset| {
                            asset.ownership == opencap_core::AssetOwnership::Managed
                                && root.join(&asset.path) == *media
                        });
                        match matches.next() {
                            Some(asset) => {
                                matches.next().is_none()
                                    && opencap_core::checked_managed_asset_path(&base, &asset.path)
                                        .is_ok()
                            }
                            None => true,
                        }
                    })
            });
        if let Some(cached) = previous.get(&base).filter(|cached| {
            cached.available
                && manifest_result.is_ok()
                && !cached.manifest_read_error
                && !cached.transcript_read_error
                && cached.display_stamp == stamp
                && cached.manifest_fingerprint == manifest_fingerprint
                && cached_managed_path_valid
                && cached.media_available
                    == cached.media.as_ref().map(|_| {
                        cached_media_metadata
                            .as_ref()
                            .is_some_and(|metadata| metadata.is_file())
                    })
                && cached.media_size
                    == cached_media_metadata
                        .as_ref()
                        .map(|metadata| metadata.len())
                && (cached.media.is_some()
                    || (!cached.manifest_present
                        && ![base.with_extension("mp4"), base.with_extension("m4a")]
                            .iter()
                            .any(|path| path.is_file())))
        }) {
            let mut cached = cached.clone();
            cached.available = true;
            cached.summary_available = base.with_extension("summary.md").exists();
            cached.transcript_available = transcript.exists();
            if cached.manifest_present {
                for item in &mut cached.media_items {
                    item.available = item.path.is_file();
                }
            } else {
                cached.media_items = list_meeting_media(&base, None, cached.media.as_deref());
            }
            files.push(cached);
            continue;
        }
        let saved = if std::fs::metadata(&transcript).is_ok_and(|m| m.len() < 16_000_000) {
            search::source_bytes_cancellable(&transcript, Some(cancelled))
                .ok()
                .flatten()
                .and_then(|b| serde_json::from_slice::<opencap_asr::Transcript>(&b).ok())
        } else {
            None
        };
        let complete = saved.as_ref().map(|t| t.is_complete());
        let transcript_read_error =
            std::fs::symlink_metadata(&transcript).is_ok() && saved.is_none();
        let model = saved.as_ref().and_then(|transcript| {
            opencap_asr::ModelKind::ALL
                .into_iter()
                .find(|kind| kind.directory() == transcript.model)
                .map(|kind| kind.label())
        });
        let manifest_path = base.with_extension("manifest.json");
        let manifest_present = match std::fs::symlink_metadata(&manifest_path) {
            Ok(_) => true,
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        };
        let manifest_error = manifest_result
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"));
        let manifest_read_error = manifest_error.is_some();
        let manifest = manifest_result.ok().flatten().map(|(manifest, _)| manifest);
        let meeting_identity_mismatch = manifest
            .as_ref()
            .zip(saved.as_ref())
            .and_then(|(manifest, transcript)| {
                transcript
                    .run
                    .as_ref()
                    .and_then(|run| run.meeting_id.as_deref())
                    .map(|id| id != manifest.meeting_id)
            })
            .unwrap_or(false);
        let complete = if meeting_identity_mismatch {
            None
        } else {
            complete
        };
        let model = if meeting_identity_mismatch {
            None
        } else {
            model
        };
        let source_asset = manifest.as_ref().and_then(|manifest| {
            if meeting_identity_mismatch {
                return None;
            }
            let recorded_id = saved
                .as_ref()
                .and_then(|transcript| transcript.run.as_ref())
                .and_then(|run| run.input_asset_id.as_ref());
            if let Some(id) = recorded_id {
                let mut matches = manifest.assets.iter().filter(|asset| &asset.asset_id == id);
                let first = matches.next()?;
                (opencap_core::is_input_media_asset(first) && matches.next().is_none())
                    .then_some(first)
            } else {
                let recorded_report = search::source_bytes_cancellable(&report, Some(cancelled))
                    .ok()
                    .flatten()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
                if let Some(asset) = recorded_report
                    .as_ref()
                    .and_then(|report| search::recording_output_asset(&base, manifest, report))
                {
                    return Some(asset);
                }
                // A single registered input is unambiguous even before a
                // recording has a transcription run; multiple inputs
                // require an explicit run identity.
                let mut sources = manifest
                    .assets
                    .iter()
                    .filter(|asset| opencap_core::is_input_media_asset(asset));
                let first = sources.next()?;
                sources.next().is_none().then_some(first)
            }
        });
        let candidate_media = manifest.as_ref().and_then(|manifest| {
            if meeting_identity_mismatch {
                return None;
            }
            let mut candidates = manifest
                .assets
                .iter()
                .filter(|asset| asset.role == "candidate_media");
            let candidate = candidates.next()?;
            candidates.next().is_none().then_some(candidate)
        });
        let relink_asset_id = source_asset
            .or(candidate_media)
            .filter(|asset| asset.ownership == opencap_core::AssetOwnership::External)
            .map(|asset| asset.asset_id.clone());
        let relink_is_candidate = source_asset.is_none() && candidate_media.is_some();
        let source_resolution = source_asset.and_then(|asset| {
            manifest.as_ref().map(|manifest| {
                opencap_core::resolve_asset_in_manifest(&base, manifest, &asset.asset_id)
            })
        });
        let source_error = source_resolution
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .map(|error| format!("{error:#}"));
        let source_media = source_resolution.and_then(|result| result.ok().flatten());
        let candidate_path = candidate_media.and_then(|asset| {
            opencap_core::resolve_asset_in_manifest(&base, manifest.as_ref()?, &asset.asset_id)
                .ok()
                .flatten()
        });
        let media_is_candidate = source_media.is_none() && candidate_path.is_some();
        let registered_media = source_media.or(candidate_path);
        let media = if manifest_present {
            registered_media
        } else {
            registered_media
                .or_else(|| {
                    [base.with_extension("mp4"), base.with_extension("m4a")]
                        .into_iter()
                        .find(|p| p.exists())
                })
                .or_else(|| saved.as_ref().and_then(|t| t.source.clone()))
        };
        let (title, mut preview, mut details) = meeting_details(
            &base,
            media.as_deref(),
            saved.as_ref().filter(|_| !meeting_identity_mismatch),
            manifest.as_ref().map(|m| m.meeting_id.as_str()),
            !manifest_present,
        );
        if transcript_read_error {
            details.push_str("\n逐字稿暂时无法读取或格式异常；原文件保留，可刷新后重试。");
        }
        if meeting_identity_mismatch {
            if preview == "尚未转录，完成后可在这里预览会议内容" {
                preview = "逐字稿与当前会议身份不一致，请检查原文件。".into();
            }
            details.push_str("\n逐字稿与资产清单的会议身份不一致，未关联媒体，请检查会议文件。");
        } else if let Some(error) = manifest_error {
            details.push_str(&format!("\n会议资产清单无法读取，未自动选择媒体：{error}"));
        } else if let Some(error) = source_error {
            details.push_str(&format!("\n原媒体登记无法解析：{error}"));
        } else if manifest_present && media.is_none() {
            details.push_str("\n会议资产清单中的原媒体无法唯一解析，请检查资产登记。");
        }
        if media_is_candidate {
            details.push_str("\n媒体是迁移时选择的候选文件，尚未证实与原转写对应。");
        }
        if let Some(migration) = manifest
            .as_ref()
            .and_then(|manifest| manifest.extra.get("legacy_migration"))
            && migration
                .get("media_provenance")
                .and_then(|value| value.as_str())
                == Some("recorded_source_path")
        {
            if migration
                .get("historical_media_verified")
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                details
                    .push_str("\n迁移归档时媒体内容与历史运行指纹一致；后续播放仍需核对当前文件。");
            } else {
                details.push_str("\n媒体路径来自旧转写；迁移时未证实其内容与历史运行一致。");
            }
        }
        if let Some(pending) = manifest
            .as_ref()
            .and_then(|m| m.extra.get("document_titles_to_review"))
            .and_then(|value| value.as_array())
        {
            let names: Vec<_> = pending.iter().filter_map(|value| value.as_str()).collect();
            if !names.is_empty() {
                details.push_str(&format!("\n改名后请核对正文标题：{}", names.join("、")));
            }
        }
        let summary_available = base.with_extension("summary.md").exists();
        let media_metadata = media.as_ref().and_then(|path| std::fs::metadata(path).ok());
        let media_available = media.as_ref().map(|_| {
            media_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_file())
        });
        let media_size = media_metadata.as_ref().map(|metadata| metadata.len());
        let manifest_available = manifest.is_some();
        let manifest_meeting_id = manifest
            .as_ref()
            .map(|manifest| manifest.meeting_id.clone());
        let transcript_available = transcript.exists();
        let recording_started_ms = meeting_recording_started_ms(&base);
        let media_items = if meeting_identity_mismatch {
            Vec::new()
        } else {
            list_meeting_media(&base, manifest.as_ref(), media.as_deref())
        };
        files.push(MeetingFile {
            base,
            media,
            media_items,
            media_is_candidate,
            relink_asset_id,
            relink_is_candidate,
            manifest_meeting_id,
            manifest_fingerprint,
            modified,
            recording_started_ms,
            available: true,
            summary_available,
            media_available,
            media_size,
            manifest_available,
            manifest_present,
            manifest_read_error,
            transcript_available,
            transcript_read_error,
            complete,
            model,
            title,
            preview,
            details,
            display_stamp: stamp,
        });
    }
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    Some((files, warnings))
}
fn meeting_details(
    base: &Path,
    media: Option<&Path>,
    transcript: Option<&opencap_asr::Transcript>,
    report_meeting_id: Option<&str>,
    allow_legacy_report: bool,
) -> (String, String, String) {
    let summary = std::fs::metadata(base.with_extension("summary.md"))
        .ok()
        .filter(|m| m.len() <= 2_000_000)
        .and_then(|_| {
            search::source_bytes_cancellable(&base.with_extension("summary.md"), None)
                .ok()
                .flatten()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let fallback = media
        .unwrap_or(base)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let title = search::source_bytes_cancellable(&base.with_extension("meeting.json"), None)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("title")
                .and_then(|title| title.as_str())
                .filter(|title| !title.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| fallback.into_owned());
    let plain = summary
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .filter(|line| !line.is_empty() && !line.starts_with("| ---"))
        .collect::<Vec<_>>()
        .join(" ");
    let plain = if plain.is_empty() {
        transcript
            .map(|t| {
                t.segments
                    .iter()
                    .filter(|s| s.is_confirmed())
                    .map(|s| s.text.as_str())
                    .take(5)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    } else {
        plain
    };
    let preview: String = plain.chars().take(140).collect();
    let preview = if preview.is_empty() {
        "尚未转录，完成后可在这里预览会议内容".into()
    } else if plain.chars().count() > 140 {
        format!("{preview}…")
    } else {
        preview
    };
    let report: serde_json::Value = std::fs::metadata(base.with_extension("recording.json"))
        .ok()
        .filter(|m| m.len() < 4_000_000)
        .and_then(|_| {
            search::source_bytes_cancellable(&base.with_extension("recording.json"), None)
                .ok()
                .flatten()
        })
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let report_matches = match report_meeting_id {
        Some(id) => report.get("meeting_id").and_then(|value| value.as_str()) == Some(id),
        None => allow_legacy_report && report.get("meeting_id").is_none_or(|value| value.is_null()),
    };
    let seconds = report_matches
        .then(|| &report["stats"]["duration_seconds"])
        .and_then(|value| value.as_f64())
        .filter(|s| s.is_finite() && *s > 0.)
        .or_else(|| transcript.and_then(|t| t.source_duration_ms.map(|ms| ms as f64 / 1000.)));
    let duration = seconds
        .map(|s| {
            format!(
                "时长 {}",
                task::duration_text(Duration::from_millis((s * 1000.) as u64))
            )
        })
        .unwrap_or_else(|| {
            transcript
                .and_then(|t| t.segments.last())
                .map(|s| {
                    format!(
                        "内容约 {}",
                        task::duration_text(Duration::from_millis(s.end_ms))
                    )
                })
                .unwrap_or_else(|| "时长待获取".into())
        });
    let size = media
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| {
            let bytes = m.len() as f64;
            if bytes >= 1_000_000_000. {
                format!("{:.2} GB", bytes / 1_000_000_000.)
            } else if bytes >= 1_000_000. {
                format!("{:.1} MB", bytes / 1_000_000.)
            } else {
                format!("{:.1} KB", bytes / 1000.)
            }
        })
        .unwrap_or_else(|| "原文件不在当前位置".into());
    (title, preview, format!("{duration}  ·  {size}"))
}
fn card() -> Div {
    ui::card()
}
fn copy_export(
    source: &Path,
    destination: &Path,
    protected_media: Option<&Path>,
) -> anyhow::Result<()> {
    if let Some(media) = protected_media
        && let (Ok(media), Ok(target)) = (media.canonicalize(), destination.canonicalize())
    {
        anyhow::ensure!(
            media != target,
            "请为文档选择新位置，原录音或视频不会被覆盖"
        );
    }
    let bytes = std::fs::read(source)?;
    opencap_asr::atomic_write(destination, &bytes)
}

impl Recorder {
    fn start_recording_audio_export(
        &mut self,
        base: PathBuf,
        meeting_id: String,
        source: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.quit_pending || self.active() || self.audio_export_job.is_some() {
            return;
        }
        let job_base = base.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let stage = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let worker_stage = stage.clone();
        match std::thread::Builder::new()
            .name("opencap-audio-export".into())
            .spawn(move || {
                let _ = tx.send(retry_recording_audio_export(
                    &job_base,
                    &meeting_id,
                    &source,
                    &worker_cancelled,
                    &worker_stage,
                ));
            }) {
            Ok(_) => {
                self.audio_export_stage_seen = 0;
                self.audio_export_job = Some((base, rx, cancelled, stage));
                self.message = "正在后台导出独立音频…".into();
            }
            Err(error) => self.message = format!("无法启动音频导出，可重试：{error}"),
        }
        cx.notify();
    }
    pub(super) fn cancel_migration_for_quit(&mut self) {
        if let Some(job) = &self.rename_job
            && matches!(
                &job.mode,
                RenameMode::Legacy { .. } | RenameMode::ResumeLegacy { .. }
            )
        {
            job.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    /// Re-point remembered meeting paths after a storage root move. Old paths
    /// that cannot be found under the new root remain listed as missing.
    fn apply_library_relink(
        &mut self,
        old_root: &Path,
        new_root: &Path,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<(usize, usize)> {
        let mut mappings = Vec::new();
        for meeting in &self.meetings {
            let Ok(relative) = meeting.base.strip_prefix(old_root) else {
                continue;
            };
            let candidate = new_root.join(relative);
            if candidate.with_extension("transcript.json").exists()
                || candidate.with_extension("recording.json").exists()
            {
                mappings.push((meeting.base.clone(), candidate));
            }
        }
        if mappings.is_empty() {
            anyhow::bail!("新目录下没有找到可重新关联的会议");
        }
        let path = index_path();
        if let Some(bytes) = search::source_bytes_cancellable(&path, None)? {
            let mut paths: Vec<PathBuf> = serde_json::from_slice(&bytes)?;
            for old in paths.iter_mut() {
                if let Some((_, new)) = mappings.iter().find(|(from, _)| from == old) {
                    *old = new.clone();
                }
            }
            opencap_asr::atomic_write(&path, &serde_json::to_vec(&paths)?)?;
        }
        search::relink_root_index(&search_index_path(), &mappings)?;
        for (old, new) in &mappings {
            if self.transcript_output.as_ref() == Some(old) {
                self.transcript_output = Some(new.clone());
            }
            if let Some(source) = &self.transcript.source
                && source.starts_with(old_root)
                && let Ok(relative) = source.strip_prefix(old_root)
            {
                self.transcript.source = Some(new_root.join(relative));
            }
        }
        self.config.output_dir = new_root.to_path_buf();
        self.save();
        self.refresh_meetings();
        cx.notify();
        Ok((
            mappings.len(),
            self.meetings.len().saturating_sub(mappings.len()),
        ))
    }

    pub(super) fn choose_relink_library_root(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.meeting_task_busy() || self.quit_pending {
            self.message = "请先结束录制和其他会议任务，再重新定位存储根目录。".into();
            cx.notify();
            return;
        }
        let old_root = self.config.output_dir.clone();
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择会议存储库的新根目录；原文件不会被移动".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(new_root) = paths.first().cloned()
            {
                let _ = entity.update(cx, |this, cx| {
                    match this.apply_library_relink(&old_root, &new_root, cx) {
                        Ok((mapped, missing)) => {
                            this.message = format!(
                                "已重新关联 {mapped} 场会议；{missing} 场仍未找到，保留为缺失条目。"
                            );
                        }
                        Err(error) => {
                            this.message = format!("重新定位存储根目录失败：{error:#}");
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn choose_relink_meeting(&mut self, old_base: PathBuf, cx: &mut Context<Self>) {
        if self.active() || self.meeting_task_busy() || self.quit_pending {
            self.message = "请先结束录制和其他会议任务，再重新定位会议。".into();
            cx.notify();
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择这场会议的新文件夹（包含同名转写或录制报告）".into()),
        });
        cx.spawn(async move |entity, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(folder) = paths.first().cloned()
            {
                let _ = entity.update(cx, |this, cx| {
                    let Some(name) = folder
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                    else {
                        this.message = "所选目录名称无效。".into();
                        cx.notify();
                        return;
                    };
                    let new_base = folder.join(&name);
                    if !new_base.with_extension("transcript.json").exists()
                        && !new_base.with_extension("recording.json").exists()
                    {
                        this.message = "所选目录中没有找到同名会议转写或录制报告。".into();
                        cx.notify();
                        return;
                    }
                    let old_id = opencap_core::meeting_manifest(&old_base)
                        .ok()
                        .flatten()
                        .map(|manifest| manifest.meeting_id);
                    if let Ok(Some(new_manifest)) = opencap_core::meeting_manifest(&new_base)
                        && old_id.as_deref() != Some(new_manifest.meeting_id.as_str())
                    {
                        this.message = "所选会议身份与要重新定位的会议不一致。".into();
                        cx.notify();
                        return;
                    }
                    let path = index_path();
                    if let Some(bytes) =
                        search::source_bytes_cancellable(&path, None).ok().flatten()
                        && let Ok(mut paths) = serde_json::from_slice::<Vec<PathBuf>>(&bytes)
                    {
                        for path in &mut paths {
                            if *path == old_base {
                                *path = new_base.clone();
                            }
                        }
                        let _ = opencap_asr::atomic_write(
                            &path,
                            &serde_json::to_vec(&paths).unwrap_or_default(),
                        );
                    }
                    let _ = search::relink_root_index(
                        &search_index_path(),
                        &[(old_base.clone(), new_base.clone())],
                    );
                    if this.transcript_output.as_ref() == Some(&old_base) {
                        this.transcript_output = Some(new_base.clone());
                    }
                    if this.meeting_selected_base.as_ref() == Some(&old_base) {
                        this.meeting_selected_base = Some(new_base.clone());
                    }
                    this.message = "已重新定位会议；原文件未被移动。".into();
                    this.refresh_meetings();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn choose_relink_media(
        &mut self,
        base: PathBuf,
        asset_id: String,
        is_candidate: bool,
        cx: &mut Context<Self>,
    ) {
        if self.quit_pending
            || self.active()
            || self.meeting_task_busy()
            || self.meeting_selected_base.as_ref() != Some(&base)
            || !self.meetings.iter().any(|meeting| {
                meeting.base == base
                    && meeting.available
                    && meeting.relink_asset_id.as_ref() == Some(&asset_id)
                    && meeting.relink_is_candidate == is_candidate
            })
        {
            return;
        }
        let Some(meeting_id) = self
            .meetings
            .iter()
            .find(|meeting| meeting.base == base)
            .and_then(|meeting| meeting.manifest_meeting_id.clone())
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        self.relink_prompt_generation = self.relink_prompt_generation.wrapping_add(1);
        let generation = self.relink_prompt_generation;
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                if is_candidate {
                    "选择候选音视频；它与转写的关系仍需核对"
                } else {
                    "选择原音视频；请确认内容与原文件相同"
                }
                .into(),
            ),
        });
        cx.spawn(async move |entity, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.first().cloned() else {
                return;
            };
            let allowed = entity
                .update(cx, |this, cx| {
                    if this.quit_pending
                        || this.active()
                        || this.meeting_task_busy()
                        || this.relink_prompt_generation != generation
                        || this.meeting_selected_base.as_ref() != Some(&base)
                        || !this.meetings.iter().any(|meeting| {
                            meeting.base == base
                                && meeting.available
                                && meeting.relink_asset_id.as_ref() == Some(&asset_id)
                                && meeting.manifest_meeting_id.as_ref() == Some(&meeting_id)
                                && meeting.relink_is_candidate == is_candidate
                        })
                    {
                        this.message = "会议或媒体状态已变化，请重新选择后定位。".into();
                        cx.notify();
                        return false;
                    }
                    this.relink_busy = true;
                    this.relink_cancelled =
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    this.message = "正在后台核对所选媒体；已有原引用保留到核对完成。".into();
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !allowed {
                return;
            }
            let task_base = base.clone();
            let task_asset_id = asset_id.clone();
            let cancelled = match entity.update(cx, |this, _| this.relink_cancelled.clone()) {
                Ok(cancelled) => cancelled,
                Err(_) => return,
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    opencap_core::relink_external_meeting_asset_checked(
                        &task_base,
                        &task_asset_id,
                        &path,
                        Some(&cancelled),
                        Some(if is_candidate {
                            "candidate_media"
                        } else {
                            "source_media"
                        }),
                        Some(&meeting_id),
                    )
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.relink_busy = false;
                match result {
                    Ok(linked) => {
                        let mut transcript_changed = false;
                        #[cfg(feature = "internal-player")]
                        let playback_needs_close = (this.player_base.as_ref() == Some(&base)
                            && this
                                .player_media
                                .as_ref()
                                .is_some_and(|media| media != &linked.path))
                            || this.pending_player.as_ref().is_some_and(
                                |(pending_base, media, _)| {
                                    pending_base == &base && media != &linked.path
                                },
                            );
                        if this.transcript_output.as_ref() == Some(&base)
                            && this
                                .transcript
                                .run
                                .as_ref()
                                .and_then(|run| run.meeting_id.as_deref())
                                == Some(linked.meeting_id.as_str())
                            && this
                                .transcript
                                .run
                                .as_ref()
                                .and_then(|run| run.input_asset_id.as_ref())
                                == Some(&asset_id)
                            && this.transcript.source.as_ref() == Some(&linked.previous_path)
                            && this.transcript.source.as_ref() != Some(&linked.path)
                        {
                            this.transcript.source = Some(linked.path.clone());
                            this.transcript.primary_save_pending = true;
                            this.transcript_dirty = true;
                            transcript_changed = true;
                        }
                        this.message = if is_candidate && linked.content_verified {
                            "已核对候选媒体文件指纹并更新位置；它与转写的关系仍需核对。"
                        } else if is_candidate {
                            "已更新候选媒体位置；文件身份及其与转写的关系仍需核对。"
                        } else if linked.content_verified {
                            "已核对 SHA-256 并更新外部媒体位置。"
                        } else {
                            "已更新外部媒体位置；文件身份尚未确认，请重新核对。"
                        }
                        .into();
                        if this.transcript_output.as_ref() == Some(&base)
                            && this
                                .transcript
                                .run
                                .as_ref()
                                .and_then(|run| run.input_asset_id.as_deref())
                                == Some(asset_id.as_str())
                            && this.transcript.source.as_ref() != Some(&linked.path)
                        {
                            this.message
                                .push_str(" 当前逐字稿的来源未自动更改，请重新打开会议并核对。");
                        }
                        #[cfg(feature = "internal-player")]
                        if transcript_changed || playback_needs_close {
                            this.close_player_for_media_change(&base);
                        }
                        this.refresh_meetings();
                        if transcript_changed && !this.active() && !this.transcription_busy() {
                            this.retry_transcript_sync();
                        }
                    }
                    Err(error) => {
                        let cancelled = this
                            .relink_cancelled
                            .load(std::sync::atomic::Ordering::Relaxed)
                            && error.chain().any(|cause| {
                                matches!(
                                    cause.to_string().as_str(),
                                    "媒体重定位已取消"
                                        | "媒体重定位已取消，原引用保留"
                                        | "媒体校验已取消"
                                )
                            });
                        this.message = if cancelled {
                            "媒体核对已取消，未提交位置更改。".into()
                        } else {
                            format!("重定位未确认完成，请刷新并检查媒体位置：{error:#}")
                        };
                        #[cfg(feature = "internal-player")]
                        if !cancelled {
                            this.close_player_for_media_change(&base);
                        }
                        this.refresh_meetings();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn begin_rename(
        &mut self,
        base: PathBuf,
        files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename_base.is_some() {
            self.message = "已有名称编辑，请先保存或取消。".into();
            cx.notify();
            return;
        }
        if self.rename_job.is_some() {
            self.message = "上一场会议的文件移动尚未结束。".into();
            return;
        }
        let loaded = (|| -> anyhow::Result<_> {
            let before = opencap_core::read_manifest_snapshot(&base)?;
            let title = opencap_asr::meeting_name_checked(&base)?;
            let after = opencap_core::read_manifest_snapshot(&base)?;
            anyhow::ensure!(
                before.as_ref().map(|(_, fingerprint)| fingerprint)
                    == after.as_ref().map(|(_, fingerprint)| fingerprint),
                "会议清单在读取期间变化"
            );
            Ok((title, before.map(|(manifest, _)| manifest.meeting_id)))
        })();
        let (baseline, identity) = match loaded {
            Ok(value) => value,
            Err(error) => {
                self.message = format!("无法读取会议名称：{error:#}");
                cx.notify();
                return;
            }
        };
        let title = baseline
            .clone()
            .or_else(|| {
                self.meetings
                    .iter()
                    .find(|m| m.base == base)
                    .map(|m| m.title())
            })
            .unwrap_or_else(|| {
                if self.transcript_output.as_ref() == Some(&base) {
                    self.transcript.title()
                } else {
                    base.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }
            });
        self.rename_input
            .update(cx, |s, cx| s.set_value(title, window, cx));
        self.rename_base = Some(base);
        self.rename_title_baseline = Some(baseline);
        self.rename_identity = Some(identity);
        self.rename_files = files;
        self.rename_migration = None;
        cx.notify();
    }

    fn begin_legacy_migration(
        &mut self,
        base: PathBuf,
        archive_media: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename_base.is_some() {
            self.message = "已有名称编辑，请先保存或取消。".into();
            cx.notify();
            return;
        }
        if self.rename_job.is_some() {
            self.message = "上一场会议的文件操作尚未结束。".into();
            return;
        }
        self.begin_rename(base, false, window, cx);
        if self.rename_base.is_some() {
            self.rename_migration = Some(archive_media);
        }
    }

    fn start_legacy_migration(
        &mut self,
        base: PathBuf,
        title: String,
        archive_media: bool,
        cx: &mut Context<Self>,
    ) {
        if self.transcript_output.as_ref() == Some(&base) && self.transcript.primary_save_pending {
            self.message = "当前逐字稿尚未保存，请先重试同步再迁移。".into();
            return;
        }
        if self.meeting_tags_base.as_ref() == Some(&base) {
            self.message = "请先保存或取消标签编辑。".into();
            return;
        }
        if title.trim().is_empty()
            || title.chars().count() > 100
            || title.chars().any(char::is_control)
        {
            self.message = "名称须为 1–100 个非控制字符。".into();
            return;
        }
        if self.rename_job.is_some() || self.meeting_scan.is_some() {
            self.message = "会议列表正在更新，请稍后再迁移。".into();
            return;
        }
        if self.transcript_output.as_ref() == Some(&base)
            && (self.active()
                || self.meeting_task_busy()
                || self
                    .live_notes
                    .as_ref()
                    .is_some_and(|notes| notes.base == base && notes.enabled))
        {
            self.message = "请先结束这场会议正在运行的录制、识别或自动纪要任务。".into();
            return;
        }
        self.persist_notes(cx);
        if self.notes_base.as_ref() == Some(&base) && self.notes_have_unsaved_work(cx) {
            self.message = "笔记尚未保存，旧会议未迁移。".into();
            return;
        }
        if self.transcript_output.as_ref() == Some(&base) && self.persist_minutes_draft(cx) {
            self.message = "纪要草稿尚未保存，旧会议未迁移。".into();
            return;
        }
        let media = self
            .meetings
            .iter()
            .find(|meeting| meeting.base == base)
            .and_then(|meeting| meeting.media.clone())
            .or_else(|| {
                (self.transcript_output.as_ref() == Some(&base))
                    .then(|| self.transcript.source.clone())
                    .flatten()
            });
        self.rename_job = Some(EntityRename {
            base,
            title,
            mode: RenameMode::Legacy {
                root: self.config.output_dir.clone(),
                media,
                archive_media,
            },
            state: RenameState::WaitingForPlayer,
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            migration_progress: Default::default(),
            shown_progress: (0, 0),
            #[cfg(feature = "internal-player")]
            player: None,
        });
        self.rename_base = None;
        self.rename_title_baseline = None;
        self.rename_identity = None;
        self.message = if archive_media {
            "正在复制并校验旧会议和媒体，原文件保留…"
        } else {
            "正在复制并校验旧会议文稿，原文件保留…"
        }
        .into();
        cx.notify();
    }

    fn start_pending_import_recovery(
        &mut self,
        pending: opencap_core::PendingLegacyImport,
        cx: &mut Context<Self>,
    ) {
        if self.quit_pending
            || self.rename_job.is_some()
            || self.rename_base.is_some()
            || !self.pending_legacy_imports.iter().any(|item| {
                item.request.as_ref().is_some_and(|current| {
                    current.stage == pending.stage
                        && current.request_fingerprint == pending.request_fingerprint
                })
            })
        {
            self.message = "迁移状态已变化，请刷新后重新检查。".into();
            cx.notify();
            return;
        }
        let root = self.config.output_dir.clone();
        self.start_legacy_migration(
            pending.original_base.clone(),
            pending.title.clone(),
            pending.archive_media,
            cx,
        );
        if let Some(job) = &mut self.rename_job
            && job.base == pending.original_base
            && matches!(&job.mode, RenameMode::Legacy { .. })
        {
            job.mode = RenameMode::ResumeLegacy { root, pending };
            self.message = "正在从原文件重新创建迁移副本；旧暂存和原会议均保留。".into();
            cx.notify();
        }
    }

    fn start_entity_rename(&mut self, base: PathBuf, title: String, cx: &mut Context<Self>) {
        let identity_matches = self.rename_identity.as_ref().is_some_and(|expected| {
            opencap_core::meeting_manifest(&base).is_ok_and(|current| {
                current
                    .as_ref()
                    .map(|manifest| manifest.meeting_id.as_str())
                    == expected.as_deref()
            })
        });
        if !identity_matches {
            self.message = "会议身份已变化，请取消并重新打开。".into();
            return;
        }
        match (
            self.rename_title_baseline.as_ref(),
            opencap_asr::meeting_name_checked(&base),
        ) {
            (Some(expected), Ok(current)) if expected == &current => (),
            (_, Ok(_)) => {
                self.message = "会议名称在编辑期间已变化，请取消并重新打开。".into();
                return;
            }
            (_, Err(error)) => {
                self.message = format!("无法核对会议名称：{error:#}");
                return;
            }
        }
        if self.transcript_output.as_ref() == Some(&base) && self.transcript.primary_save_pending {
            self.message = "当前逐字稿尚未保存，请先重试同步再移动会议。".into();
            return;
        }
        if self.meeting_tags_base.as_ref() == Some(&base) {
            self.message = "请先保存或取消标签编辑。".into();
            return;
        }
        if title.trim().is_empty()
            || title.chars().count() > 100
            || title.chars().any(char::is_control)
        {
            self.message = "名称须为 1–100 个非控制字符。".into();
            return;
        }
        if self.rename_job.is_some() || self.meeting_scan.is_some() {
            self.message = "会议列表正在更新，请稍后再移动文件。".into();
            return;
        }
        #[cfg(feature = "online-speaker")]
        if self
            .online_speakers
            .as_ref()
            .is_some_and(|speaker| speaker.base.as_ref() == Some(&base) && !speaker.can_replace())
        {
            self.message = "请等待这场会议的在线说话人结果收尾并保存后，再移动文件。".into();
            return;
        }
        if self
            .live_notes
            .as_ref()
            .is_some_and(|notes| notes.base == base && notes.enabled)
        {
            self.message = "请先停止这场会议的自动纪要更新，再移动文件。".into();
            return;
        }
        self.persist_notes(cx);
        if self.notes_base.as_ref() == Some(&base) && self.notes_have_unsaved_work(cx) {
            self.message = "笔记尚未保存，会议文件未移动。".into();
            return;
        }
        if self.transcript_output.as_ref() == Some(&base) && self.persist_minutes_draft(cx) {
            self.message = "纪要草稿尚未保存，会议文件未移动。".into();
            return;
        }
        #[cfg(feature = "internal-player")]
        self.cancel_waveforms_for(&base);
        #[cfg(feature = "internal-player")]
        let player = {
            if self.pending_player.is_some() {
                self.message = "播放切换尚未结束，请稍后重试。".into();
                return;
            }
            if self.player_base.as_ref() == Some(&base) {
                self.player.as_ref().and_then(|session| {
                    self.player_media.as_ref().map(|media| {
                        let resume = PlayerResume {
                            media: media.clone(),
                            snapshot: session.snapshot(),
                        };
                        let _ = session.command(opencap_media::PlaybackCommand::Close);
                        resume
                    })
                })
            } else {
                None
            }
        };
        self.rename_job = Some(EntityRename {
            base,
            title,
            mode: RenameMode::Managed,
            state: RenameState::WaitingForPlayer,
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            migration_progress: Default::default(),
            shown_progress: (0, 0),
            #[cfg(feature = "internal-player")]
            player,
        });
        self.rename_base = None;
        self.rename_title_baseline = None;
        self.rename_identity = None;
        self.message = "正在关闭此会议的播放并准备移动文件…".into();
        cx.notify();
    }

    pub(super) fn poll_entity_rename(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(mut job) = self.rename_job.take() else {
            return false;
        };
        match &job.state {
            RenameState::WaitingForPlayer => {
                #[cfg(feature = "internal-player")]
                if self
                    .player_waveform_jobs
                    .iter()
                    .any(|waveform| waveform.base == job.base)
                {
                    self.rename_job = Some(job);
                    return false;
                }
                #[cfg(feature = "internal-player")]
                if job.player.is_some()
                    && self
                        .player
                        .as_ref()
                        .is_some_and(|player| !player.finished())
                {
                    self.rename_job = Some(job);
                    return false;
                }
                #[cfg(feature = "internal-player")]
                if job.player.is_some() && self.player_base.as_ref() == Some(&job.base) {
                    self.player = None;
                    self.player_snapshot = None;
                    self.player_base = None;
                    self.player_media = None;
                    if let Some(image) = self.player_image.take() {
                        cx.drop_image(image, None);
                    }
                }
                let (tx, rx) = crossbeam_channel::bounded(1);
                let base = job.base.clone();
                let title = job.title.clone();
                let mode = job.mode.clone();
                let cancelled = job.cancelled.clone();
                let progress = job.migration_progress.clone();
                std::thread::spawn(move || {
                    let result = match mode {
                        RenameMode::Managed => opencap_core::rename_managed_meeting(&base, &title)
                            .map(|moved| {
                                let warning = finish_index_move(&moved).err().map(|error| {
                                    format!("文件已移动；索引将在下次刷新时恢复：{error:#}")
                                });
                                RenameOutcome::Moved(moved, warning)
                            }),
                        RenameMode::Legacy {
                            root,
                            media,
                            archive_media,
                        } => opencap_core::migrate_legacy_copy_cancellable(
                            &base,
                            &root,
                            &title,
                            media.as_deref(),
                            archive_media,
                            Some(&cancelled),
                            Some(&progress),
                        )
                        .map(|migration| RenameOutcome::Migrated(migration, None)),
                        RenameMode::ResumeLegacy { root, pending } => {
                            opencap_core::retry_pending_legacy_import(
                                &root,
                                &pending,
                                Some(&cancelled),
                                Some(&progress),
                            )
                            .map(|(migration, warning)| RenameOutcome::Migrated(migration, warning))
                        }
                    };
                    let _ = tx.send(result);
                });
                job.state = RenameState::Working(rx);
                if matches!(&job.mode, RenameMode::Managed) {
                    self.message = "正在移动会议文件；原媒体不会重新编码。".into();
                }
                self.rename_job = Some(job);
                true
            }
            RenameState::Working(rx) => {
                let result = match rx.try_recv() {
                    Ok(result) => result,
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        let (phase, bytes) = job.migration_progress.snapshot();
                        let displayed = (phase, bytes / (1024 * 1024));
                        let progress_changed = matches!(
                            &job.mode,
                            RenameMode::Legacy { .. } | RenameMode::ResumeLegacy { .. }
                        ) && displayed != job.shown_progress;
                        if progress_changed {
                            job.shown_progress = displayed;
                            if !self.quit_pending
                                && !job.cancelled.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                let action = match phase {
                                    1 => "正在复制",
                                    2 => "正在校验",
                                    3 => "正在发布",
                                    _ => "正在准备",
                                };
                                self.message = format!(
                                    "{action}旧会议副本；已复制 {} MiB，原文件保留。",
                                    displayed.1
                                );
                            }
                        }
                        self.rename_job = Some(job);
                        return progress_changed;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        Err(anyhow::anyhow!("移动线程意外退出；恢复日志将保留"))
                    }
                };
                match result {
                    Ok(RenameOutcome::Moved(moved, warning)) => {
                        if self.meeting_selected_base.as_ref() == Some(&moved.old_base) {
                            self.meeting_selected_base = Some(moved.new_base.clone());
                        }
                        if self.transcript_output.as_ref() == Some(&moved.old_base) {
                            self.transcript_output = Some(moved.new_base.clone());
                            self.transcript.source = self
                                .transcript
                                .source
                                .as_ref()
                                .map(|path| moved.relocate_path(path));
                            self.transcript
                                .extra
                                .insert("document_title".into(), job.title.clone().into());
                            self.meeting_title = job.title.clone();
                            self.transcript_dirty = true;
                        }
                        if self.notes_base.as_ref() == Some(&moved.old_base) {
                            self.notes_base = Some(moved.new_base.clone());
                            if self.notes_reload_busy
                                || self
                                    .notes_reload_result
                                    .as_ref()
                                    .is_some_and(|load| load.base == moved.old_base)
                            {
                                self.notes_initial_load_pending = true;
                            }
                        }
                        if let Some(notes) = &mut self.live_notes
                            && notes.base == moved.old_base
                        {
                            notes.base = moved.new_base.clone();
                        }
                        #[cfg(feature = "online-speaker")]
                        if let Some(speaker) = &mut self.online_speakers
                            && speaker.base.as_ref() == Some(&moved.old_base)
                        {
                            speaker.base = Some(moved.new_base.clone());
                        }
                        self.ai.relocate_meeting(&moved.old_base, &moved.new_base);
                        if let Some(path) = &mut self.stats.output {
                            *path = moved.relocate_path(path);
                        }
                        if let Some(path) = &mut self.stats.audio_output {
                            *path = moved.relocate_path(path);
                        }
                        self.meeting_scan_extra.remove(&moved.old_base);
                        self.meeting_scan_extra.insert(moved.new_base.clone());
                        self.message = warning.unwrap_or_else(|| {
                            format!(
                                "会议文件已移动到 {}",
                                moved.new_base.parent().unwrap_or(&moved.new_base).display()
                            )
                        });
                        if !moved.document_titles_to_review.is_empty() {
                            self.message.push_str(&format!(
                                "；以下正文标题保留原内容，请核对：{}",
                                moved.document_titles_to_review.join("、")
                            ));
                        }
                        #[cfg(feature = "internal-player")]
                        if let Some(player) = job.player.take() {
                            let media = moved.relocate_path(&player.media);
                            if let Err(error) = self.restore_player_after_move(
                                moved.new_base.clone(),
                                media,
                                player.snapshot,
                            ) {
                                self.message = format!("{}；播放未恢复：{error:#}", self.message);
                            }
                        }
                    }
                    Ok(RenameOutcome::Migrated(migrated, warning)) => {
                        if self.meeting_selected_base.as_ref() == Some(&migrated.original_base) {
                            self.meeting_selected_base = Some(migrated.new_base.clone());
                        }
                        // Migration is a copy, not a move. Keep any open
                        // transcript, notes and AI work bound to the original:
                        // an in-memory draft may have changed during the copy
                        // and lacks identity fields added to the new file.
                        let original_open = self.transcript_output.as_ref()
                            == Some(&migrated.original_base)
                            || self.notes_base.as_ref() == Some(&migrated.original_base);
                        self.meeting_scan_extra.insert(migrated.new_base.clone());
                        self.message = if migrated.media_archived {
                            "已创建含媒体的受管理副本，旧会议和原媒体仍保留在原位置。"
                        } else {
                            "已创建受管理文稿副本；旧会议和引用的原媒体仍保留在原位置。"
                        }
                        .into();
                        if original_open {
                            self.message.push_str(
                                " 当前打开的稿件和笔记仍属于旧会议；请从会议库打开新副本。",
                            );
                        }
                        if let Some(warning) = warning {
                            self.message.push_str(&format!(" {warning}。"));
                        }
                        if opencap_core::meeting_manifest(&migrated.new_base)
                            .ok()
                            .flatten()
                            .is_some_and(|manifest| {
                                manifest
                                    .assets
                                    .iter()
                                    .any(|asset| asset.role == "candidate_media")
                            })
                        {
                            self.message
                                .push_str(" 媒体是未核实候选文件，与旧转写的关系仍需确认。");
                        }
                    }
                    Err(error) => {
                        self.message = if matches!(&job.mode, RenameMode::Managed) {
                            format!(
                                "会议文件未能完成移动：{error:#}。如日志存在，刷新会议列表会尝试恢复。"
                            )
                        } else {
                            format!("旧会议副本未能完成：{error:#}。原文件保留。")
                        };
                        #[cfg(feature = "internal-player")]
                        if let Some(player) = job.player.take()
                            && player.media.is_file()
                        {
                            let _ = self.restore_player_after_move(
                                job.base.clone(),
                                player.media,
                                player.snapshot,
                            );
                        }
                    }
                }
                self.refresh_meetings();
                cx.notify();
                true
            }
        }
    }
    fn rename_editor(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(label(match self.rename_migration {
                Some(true) => "复制并校验会议文稿和原媒体；旧文件全部保留。",
                Some(false) => "复制会议文稿并引用原媒体；旧文件全部保留。",
                None if self.rename_files => "受管理文件会一起移动；外部原件保留原位置。",
                None => "仅修改显示名称，文件路径不变。",
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(gpui::component::input::Input::new(&self.rename_input).flex_1())
                    .child(
                        primary_button(
                            "save-meeting-name",
                            if self.rename_migration.is_some() {
                                "创建副本"
                            } else if self.rename_files {
                                "移动文件并保存"
                            } else {
                                "保存名称"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(base) = this.rename_base.clone() {
                                if let Some(archive_media) = this.rename_migration {
                                    let title = this.rename_input.read(cx).value().to_string();
                                    this.start_legacy_migration(base, title, archive_media, cx);
                                    return;
                                }
                                if this.rename_files {
                                    let title = this.rename_input.read(cx).value().to_string();
                                    this.start_entity_rename(base, title, cx);
                                    return;
                                }
                                let Some(baseline) = this.rename_title_baseline.as_ref() else {
                                    this.message = "缺少名称编辑基线，请重新打开编辑器。".into();
                                    cx.notify();
                                    return;
                                };
                                let Some(identity) = this.rename_identity.as_ref() else {
                                    this.message = "缺少会议身份基线，请重新打开编辑器。".into();
                                    cx.notify();
                                    return;
                                };
                                match opencap_asr::rename_meeting_for_identity(
                                    &base,
                                    &this.rename_input.read(cx).value(),
                                    Some(baseline.as_deref()),
                                    Some(identity.as_deref()),
                                ) {
                                    Ok(()) => {
                                        if this.transcript_output.as_ref() == Some(&base) {
                                            this.meeting_title = opencap_asr::meeting_name(&base)
                                                .unwrap_or_default();
                                        }
                                        this.rename_base = None;
                                        this.rename_title_baseline = None;
                                        this.rename_identity = None;
                                        this.refresh_meetings();
                                    }
                                    Err(e) => this.message = format!("保存显示名称失败：{e:#}"),
                                }
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        button("copy-meeting-name-draft", "复制输入", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let text = this.rename_input.read(cx).value().to_string();
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                this.message =
                                    "名称输入已复制；可取消并重新打开以读取最新名称。".into();
                                cx.notify();
                            })),
                    )
                    .child(
                        button("cancel-meeting-name", "取消", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename_base = None;
                                this.rename_title_baseline = None;
                                this.rename_identity = None;
                                this.rename_files = false;
                                this.rename_migration = None;
                                cx.notify();
                            })),
                    ),
            )
    }
    /// Save an independent document copy. Opening an existing document is a separate action.
    pub(super) fn export_document(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        let folder = source.parent().unwrap_or(Path::new("."));
        let kind = if source.to_string_lossy().ends_with("summary.docx") {
            "纪要"
        } else if source.to_string_lossy().ends_with("notes.docx") {
            "笔记"
        } else {
            "逐字稿"
        };
        let title = if self.meeting_title.is_empty() {
            self.transcript.title()
        } else {
            self.meeting_title.clone()
        };
        let suggested = opencap_asr::document_filename(&title, kind);
        let prompt = cx.prompt_for_new_path(folder, Some(&suggested));
        let protected_media = self.transcript.source.clone();
        cx.spawn(async move |entity, cx| {
            let selected = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                error => {
                    let _ = entity.update(cx, |this, cx| {
                        this.message = format!("无法选择导出位置：{error:?}");
                        cx.notify();
                    });
                    return;
                }
            };
            let mut destination = selected;
            if destination.extension().is_none() {
                destination.set_extension("docx");
            }
            let saved = destination.clone();
            let result = cx
                .background_executor()
                .spawn(
                    async move { copy_export(&source, &destination, protected_media.as_deref()) },
                )
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.message = match result {
                    Ok(()) => format!("已导出副本：{}", saved.display()),
                    Err(error) => format!("导出失败：{error:#}"),
                };
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn refresh_meetings(&mut self) {
        if self.quit_pending {
            return;
        }
        if self.rename_job.is_some() {
            self.meeting_scan_again = true;
            return;
        }
        if self.meeting_scan.is_some() {
            self.meeting_scan_again = true;
            return;
        }
        let root = self.config.output_dir.clone();
        let previous = self.meetings.clone();
        // Retain pending paths until a worker has durably merged them into the
        // index. A lock failure or a crashed worker must not lose a meeting.
        let requested_extra = self.meeting_scan_extra.clone();
        let mut extra = requested_extra.clone();
        self.meeting_scan_cancelled =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = self.meeting_scan_cancelled.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.meeting_scan = Some(rx);
        std::thread::spawn(move || {
            let mut relocated = Vec::new();
            let mut errors = Vec::new();
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = tx.send(MeetingScan {
                    root,
                    files: Vec::new(),
                    error: None,
                    notice: None,
                    pending_imports: None,
                    relocated,
                    persisted_extra: BTreeSet::new(),
                    cancelled: true,
                });
                return;
            }
            if root.is_dir() {
                match opencap_core::recover_meeting_renames(&root) {
                    Ok(moves) => {
                        for moved in moves {
                            if extra.remove(&moved.old_base) {
                                extra.insert(moved.new_base.clone());
                            }
                            relocated.push(moved.clone());
                            if let Err(error) = finish_index_move(&moved) {
                                errors.push(format!("会议改名索引恢复待处理：{error:#}"));
                            }
                        }
                    }
                    Err(error) => errors.push(format!("会议改名恢复待处理：{error:#}")),
                }
            }
            // Do not persist queued old paths while any rename is unresolved.
            if !errors.is_empty() {
                let _ = tx.send(MeetingScan {
                    root,
                    files: previous,
                    error: Some(errors.join("；")),
                    notice: None,
                    pending_imports: None,
                    relocated,
                    persisted_extra: BTreeSet::new(),
                    cancelled: false,
                });
                return;
            }
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = tx.send(MeetingScan {
                    root,
                    files: Vec::new(),
                    error: None,
                    notice: None,
                    pending_imports: None,
                    relocated,
                    persisted_extra: BTreeSet::new(),
                    cancelled: true,
                });
                return;
            }
            let _scan_activity = if root.is_dir() {
                match opencap_core::meeting_scan_activity(&root) {
                    Ok(activity) => Some(activity),
                    Err(error) => {
                        errors.push(format!("{error:#}"));
                        let _ = tx.send(MeetingScan {
                            root,
                            files: previous,
                            error: Some(errors.join("；")),
                            notice: None,
                            pending_imports: None,
                            relocated,
                            persisted_extra: BTreeSet::new(),
                            cancelled: false,
                        });
                        return;
                    }
                }
            } else {
                None
            };
            let mut persisted_extra = match persist_meeting_index(extra.iter().cloned()) {
                Ok(()) => requested_extra,
                Err(err) => {
                    errors.push(format!("会议路径列表未保存：{err:#}"));
                    BTreeSet::new()
                }
            };
            let mut remembered = match std::fs::read(index_path()) {
                Ok(bytes) => match serde_json::from_slice::<Vec<PathBuf>>(&bytes) {
                    Ok(paths) => paths,
                    Err(error) => {
                        errors.push(format!("会议路径列表无法解析，保留当前列表：{error}"));
                        persisted_extra.clear();
                        previous
                            .iter()
                            .map(|meeting| meeting.base.clone())
                            .collect()
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    errors.push(format!("会议路径列表无法读取，保留当前列表：{error}"));
                    persisted_extra.clear();
                    previous
                        .iter()
                        .map(|meeting| meeting.base.clone())
                        .collect()
                }
            };
            remembered.extend(extra);
            let mut previous = previous;
            for moved in &relocated {
                for path in &mut remembered {
                    if *path == moved.old_base {
                        *path = moved.new_base.clone();
                    }
                }
                for meeting in &mut previous {
                    if meeting.base == moved.old_base {
                        meeting.base = moved.new_base.clone();
                        meeting.media =
                            meeting.media.as_ref().map(|path| moved.relocate_path(path));
                        meeting.available = false;
                        meeting.title = moved
                            .new_base
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        meeting.details = format!(
                            "已恢复到 {}；连接目录后刷新以读取最新详情。",
                            moved.new_base.display()
                        );
                    }
                }
            }
            let Some((files, scan_warnings)) =
                discover_paths_cached(&root, remembered, previous, &cancelled)
            else {
                let _ = tx.send(MeetingScan {
                    root,
                    files: Vec::new(),
                    error: None,
                    notice: None,
                    pending_imports: None,
                    relocated,
                    persisted_extra: BTreeSet::new(),
                    cancelled: true,
                });
                return;
            };
            if !scan_warnings.is_empty() {
                persisted_extra.clear();
                errors.push(format!("会议目录扫描不完整：{}", scan_warnings.join("；")));
            }
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = tx.send(MeetingScan {
                    root,
                    files: Vec::new(),
                    error: None,
                    notice: None,
                    relocated,
                    pending_imports: None,
                    persisted_extra: BTreeSet::new(),
                    cancelled: true,
                });
                return;
            }
            if let Err(err) = persist_meeting_index(files.iter().map(|m| m.base.clone())) {
                persisted_extra.clear();
                errors.push(format!("会议路径列表未更新：{err:#}"));
            }
            let entries: Vec<_> = files
                .iter()
                .map(|meeting| search::IndexEntry {
                    base: meeting.base.clone(),
                    title: meeting.title.clone(),
                    available: meeting.available,
                    media_available: if meeting.media_is_candidate || !meeting.available {
                        None
                    } else {
                        meeting.media_available
                    },
                })
                .collect();
            match search::update_index(&search_index_path(), &entries, &cancelled) {
                Ok(warnings) if !warnings.is_empty() => {
                    persisted_extra.clear();
                    errors.push(format!(
                        "{} 场会议的搜索索引未更新：{}",
                        warnings.len(),
                        warnings[0]
                    ));
                }
                Err(err) => {
                    persisted_extra.clear();
                    errors.push(format!("搜索索引不可用：{err:#}"));
                }
                _ => (),
            }
            let was_cancelled = cancelled.load(std::sync::atomic::Ordering::Relaxed);
            if was_cancelled {
                let _ = tx.send(MeetingScan {
                    root,
                    files: Vec::new(),
                    error: None,
                    notice: None,
                    relocated,
                    pending_imports: None,
                    persisted_extra: BTreeSet::new(),
                    cancelled: true,
                });
                return;
            }
            let (pending_imports, notice) = if root.is_dir() {
                match opencap_core::pending_legacy_imports(&root) {
                    Ok(paths) if !paths.is_empty() => {
                        let notice = format!(
                            "发现 {} 个迁移暂存目录；可能仍在复制或上次中断，原会议保留。",
                            paths.len()
                        );
                        let pending = paths
                            .into_iter()
                            .map(|stage| {
                                match opencap_core::inspect_pending_legacy_import(&root, &stage) {
                                    Ok(request) => PendingImport {
                                        stage,
                                        request: Some(request),
                                        detail: None,
                                    },
                                    Err(error) => PendingImport {
                                        stage,
                                        request: None,
                                        detail: Some(format!("{error:#}")),
                                    },
                                }
                            })
                            .collect();
                        (Some(pending), Some(notice))
                    }
                    Err(error) => (None, Some(format!("无法检查迁移暂存目录：{error:#}"))),
                    _ => (Some(Vec::new()), None),
                }
            } else {
                (None, None)
            };
            let _ = tx.send(MeetingScan {
                root,
                files,
                error: (!errors.is_empty()).then(|| errors.join("；")),
                notice,
                pending_imports,
                relocated,
                persisted_extra,
                cancelled: was_cancelled,
            });
        });
    }
    pub(super) fn start_meeting_search(&mut self, query: String) {
        let query = query.trim().to_owned();
        self.meeting_rows_scroll.set_offset(point(px(0.), px(0.)));
        self.meeting_detail_scroll.set_offset(point(px(0.), px(0.)));
        if query.is_empty() && self.meeting_search_filters.is_empty() {
            self.semantic_search_cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.meeting_search_query = None;
            self.meeting_search_result = None;
            self.meeting_search_task = None;
            self.meeting_list_page = 0;
            return;
        }
        self.meeting_search_query = Some(query);
        self.meeting_list_page = 0;
        self.search_meeting_page();
    }
    fn apply_meeting_filters(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<search::SearchFilters> {
            let from = filter_date(&self.meeting_filter_from.read(cx).value(), false)?;
            let before = filter_date(&self.meeting_filter_to.read(cx).value(), true)?;
            anyhow::ensure!(
                from.zip(before).is_none_or(|(from, before)| from < before),
                "结束日期不能早于起始日期"
            );
            let min = filter_minutes(&self.meeting_filter_min.read(cx).value())?;
            let max = filter_minutes(&self.meeting_filter_max.read(cx).value())?;
            anyhow::ensure!(
                min.zip(max).is_none_or(|(min, max)| min <= max),
                "最长时长不能小于最短时长"
            );
            let tag = self.meeting_filter_tag.read(cx).value().trim().to_owned();
            let speaker = self
                .meeting_filter_speaker
                .read(cx)
                .value()
                .trim()
                .to_owned();
            anyhow::ensure!(
                tag.chars().count() <= 32 && speaker.chars().count() <= 80,
                "标签或说话人筛选过长"
            );
            Ok(search::SearchFilters {
                date_by_recording: self.meeting_filter_recording_date
                    && (from.is_some() || before.is_some()),
                updated_from_ms: from,
                updated_before_ms: before,
                min_duration_ms: min,
                max_duration_ms: max,
                source: self.meeting_filter_source.clone(),
                status: self.meeting_filter_status.clone(),
                tag: (!tag.is_empty()).then_some(tag),
                speaker: (!speaker.is_empty()).then_some(speaker),
            })
        })();
        match result {
            Ok(filters) => {
                self.meeting_search_filters = filters;
                let query = self.meeting_search_input.read(cx).value().to_string();
                self.start_meeting_search(query);
            }
            Err(error) => self.message = format!("筛选条件无效：{error:#}"),
        }
        cx.notify();
    }
    fn clear_meeting_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.meeting_filter_source = None;
        self.meeting_filter_status = None;
        self.meeting_filter_recording_date = false;
        self.meeting_search_filters = Default::default();
        for input in [
            &self.meeting_filter_from,
            &self.meeting_filter_to,
            &self.meeting_filter_min,
            &self.meeting_filter_max,
            &self.meeting_filter_tag,
            &self.meeting_filter_speaker,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        let query = self.meeting_search_input.read(cx).value().to_string();
        self.start_meeting_search(query);
    }

    fn meeting_filter_panel(&self, cx: &Context<Self>) -> Div {
        let source = [
            (None, "全部来源"),
            (Some("managed"), "受管理"),
            (Some("external"), "外部引用"),
            (Some("legacy"), "旧记录"),
            (Some("unknown"), "来源待核对"),
        ];
        let status = [
            (None, "全部状态"),
            (Some("complete"), "已完成"),
            (Some("incomplete"), "未完成或未知"),
            (Some("offline"), "暂不可访问"),
        ];
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .child(label(
                "日期、时长、来源、状态、标签和说话人可组合；录制日期未知的会议不匹配录制日期范围。",
            ))
            .child(label("“来源待核对”包括候选媒体、来源不唯一或会议身份不一致；打开详情可进一步核对。"))
            .child(div().flex().gap_2()
                .child(button("filter-date-updated", "更新日期", !self.meeting_filter_recording_date,)
                    .ghost().on_click(cx.listener(|this, _, _, cx| { this.meeting_filter_recording_date = false; cx.notify(); })))
                .child(button("filter-date-recorded", "录制日期", self.meeting_filter_recording_date,)
                    .ghost().on_click(cx.listener(|this, _, _, cx| { this.meeting_filter_recording_date = true; cx.notify(); }))))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        source
                            .into_iter()
                            .enumerate()
                            .map(|(index, (value, title))| {
                                button(
                                    ("filter-source", index),
                                    title,
                                    self.meeting_filter_source.as_deref() == value,
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.meeting_filter_source = value.map(str::to_owned);
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        status
                            .into_iter()
                            .enumerate()
                            .map(|(index, (value, title))| {
                                button(
                                    ("filter-status", index),
                                    title,
                                    self.meeting_filter_status.as_deref() == value,
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.meeting_filter_status = value.map(str::to_owned);
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        gpui::component::input::Input::new(&self.meeting_filter_from).w(px(200.)),
                    )
                    .child(gpui::component::input::Input::new(&self.meeting_filter_to).w(px(200.)))
                    .child(gpui::component::input::Input::new(&self.meeting_filter_min).w(px(150.)))
                    .child(gpui::component::input::Input::new(&self.meeting_filter_max).w(px(150.)))
                    .child(gpui::component::input::Input::new(&self.meeting_filter_tag).w(px(160.)))
                    .child(
                        gpui::component::input::Input::new(&self.meeting_filter_speaker)
                            .w(px(160.)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        primary_button("apply-meeting-filters", "应用筛选")
                            .on_click(cx.listener(|this, _, _, cx| this.apply_meeting_filters(cx))),
                    )
                    .child(
                        button("reset-meeting-filters", "清除筛选", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_meeting_filters(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex().flex_col().gap_2().border_t_1().border_color(rgb(BORDER)).pt_3()
                    .child(label("本地语义检索（可选）：仅连接本机 Ollama，且必须先关闭其云功能；不会自动下载模型。"))
                    .child(div().flex().flex_wrap().items_center().gap_2()
                        .child(gpui::component::input::Input::new(&self.semantic_model_input).w(px(240.)).disabled(self.semantic_task.is_some()))
                        .child(button("build-semantic-index", "创建 / 更新语义索引", false).disabled(self.active() || self.semantic_task.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.start_semantic_index(cx))))
                        .child(button("toggle-semantic-search", if self.semantic_enabled { "混合搜索：开" } else { "混合搜索：关" }, self.semantic_enabled)
                            .disabled(self.active() && !self.semantic_enabled).on_click(cx.listener(|this, _, _, cx| this.toggle_semantic_search(cx))))
                        .when(self.semantic_task.is_some(), |d| d.child(button("cancel-semantic-index", if self.semantic_cancel.load(std::sync::atomic::Ordering::Relaxed) { "正在停止…" } else { "停止索引" }, false).ghost()
                            .disabled(self.semantic_cancel.load(std::sync::atomic::Ordering::Relaxed)).on_click(cx.listener(|this, _, _, cx| {
                            this.semantic_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            this.semantic_status = "已请求停止；当前本地模型请求返回后结束。".into();
                            cx.notify();
                        })))))
                    .when(!self.semantic_status.is_empty(), |d| d.child(label(self.semantic_status.clone())))
            )
    }
    fn search_meeting_page(&mut self) {
        let Some(query) = self.meeting_search_query.clone() else {
            return;
        };
        let page = self.meeting_list_page;
        let filters = self.meeting_search_filters.clone();
        self.semantic_search_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.semantic_search_cancel = cancelled.clone();
        let model = if self.semantic_enabled && !query.is_empty() {
            self.config.semantic_embedding_model.clone()
        } else {
            None
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.meeting_search_result = None;
        self.meeting_search_task = Some(rx);
        std::thread::spawn(move || {
            let result = if let Some(model) = model {
                let hybrid = semantic::search_hybrid(
                    &search_index_path(),
                    &model,
                    &query,
                    &filters,
                    page,
                    MEETINGS_PER_PAGE,
                    &cancelled,
                );
                match hybrid {
                    Ok(result) => Ok(result),
                    Err(error) if !cancelled.load(std::sync::atomic::Ordering::Relaxed) => {
                        search::search(
                            &search_index_path(),
                            &query,
                            &filters,
                            page,
                            MEETINGS_PER_PAGE,
                        )
                        .map(|mut result| {
                            result.semantic_model = Some(model.trim().to_owned());
                            result.semantic_fallback = true;
                            result.notice =
                                Some(format!("语义搜索未完成：{error:#}；以下为关键词结果。"));
                            result
                        })
                    }
                    Err(error) => Err(error),
                }
            } else {
                search::search(
                    &search_index_path(),
                    &query,
                    &filters,
                    page,
                    MEETINGS_PER_PAGE,
                )
            };
            let _ = tx.send(result);
        });
    }
    pub(super) fn poll_meeting_search(&mut self) -> bool {
        let Some(result) = self
            .meeting_search_task
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("搜索线程已退出")))
                }
            })
        else {
            return false;
        };
        self.meeting_search_task = None;
        match result {
            Ok(result)
                if self.meeting_search_query.as_deref() == Some(result.query.as_str())
                    && self.meeting_list_page == result.page
                    && self.meeting_search_filters == result.filters
                    && result.semantic_model.as_deref()
                        == if self.semantic_enabled && !result.query.is_empty() {
                            self.config.semantic_embedding_model.as_deref()
                        } else {
                            None
                        } =>
            {
                self.meeting_search_result = Some(result)
            }
            Ok(_) => (),
            Err(error) => self.message = format!("会议搜索失败：{error:#}"),
        }
        true
    }

    fn selected_semantic_model(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let model = self.semantic_model_input.read(cx).value().trim().to_owned();
        if model.is_empty() {
            self.semantic_status = "请先输入已安装的本地嵌入模型名称。".into();
            return None;
        }
        self.config.semantic_embedding_model = Some(model.clone());
        self.save();
        Some(model)
    }
    fn start_semantic_index(&mut self, cx: &mut Context<Self>) {
        if self.active() || self.semantic_task.is_some() || self.meeting_scan.is_some() {
            self.semantic_status = "请在录制和会议索引更新结束后创建语义索引。".into();
            cx.notify();
            return;
        }
        let Some(model) = self.selected_semantic_model(cx) else {
            cx.notify();
            return;
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.semantic_cancel = cancelled.clone();
        self.semantic_task_model = Some(model.clone());
        self.semantic_task = Some(rx);
        self.semantic_status = "正在检查本机服务和模型…".into();
        std::thread::spawn(move || {
            let result =
                semantic::build_index(&search_index_path(), &model, &cancelled, |status| {
                    let _ = tx.send(semantic::SemanticEvent::Progress(status));
                });
            let _ = tx.send(semantic::SemanticEvent::Finished(result));
        });
        cx.notify();
    }
    fn toggle_semantic_search(&mut self, cx: &mut Context<Self>) {
        if self.active() && !self.semantic_enabled {
            self.semantic_status = "录制期间请使用关键词搜索；结束后可启用混合搜索。".into();
            cx.notify();
            return;
        }
        if !self.semantic_enabled && self.selected_semantic_model(cx).is_none() {
            cx.notify();
            return;
        }
        self.semantic_enabled = !self.semantic_enabled;
        self.semantic_status = if self.semantic_enabled {
            "混合搜索已开启；语义候选最多 500 场。"
        } else {
            "已切换为关键词搜索。"
        }
        .into();
        if self.meeting_search_query.is_some() {
            self.search_meeting_page();
        }
        cx.notify();
    }
    pub(super) fn poll_semantic_index(&mut self) -> bool {
        let Some(receiver) = &self.semantic_task else {
            return false;
        };
        let mut changed = false;
        let mut finished = false;
        loop {
            match receiver.try_recv() {
                Ok(semantic::SemanticEvent::Progress(status)) => {
                    self.semantic_status = status;
                    changed = true;
                }
                Ok(semantic::SemanticEvent::Finished(result)) => {
                    let model = self.semantic_task_model.take().unwrap_or_default();
                    self.semantic_status = match result {
                        Ok(report) => format!(
                            "{} · 模型 {model} · 已更新 {} / {} 场，新增 {} 段，复用 {} 段；{} 场离线，{} 场来源待刷新{}",
                            if report.cancelled {
                                "索引已停止"
                            } else if !report.skipped.is_empty()
                                || report.source_unavailable > 0
                                || report.source_pending > 0
                            {
                                "索引部分更新"
                            } else {
                                "索引更新完成"
                            },
                            report.meetings,
                            report.eligible,
                            report.chunks_created,
                            report.chunks_reused,
                            report.source_unavailable,
                            report.source_pending,
                            if report.skipped.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    "；{} 场未更新：{}",
                                    report.skipped.len(),
                                    report.skipped[0]
                                )
                            }
                        ),
                        Err(error) => format!("语义索引未完成：{error:#}"),
                    };
                    finished = true;
                    changed = true;
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.semantic_status = "语义索引线程退出，已有索引保留。".into();
                    self.semantic_task_model = None;
                    finished = true;
                    changed = true;
                    break;
                }
            }
        }
        if finished {
            self.semantic_task = None;
            if self.semantic_enabled && self.meeting_search_query.is_some() {
                self.search_meeting_page();
            }
        }
        changed
    }
    pub(super) fn poll_meeting_scan(&mut self) -> bool {
        let Some(result) = self
            .meeting_scan
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(())),
            })
        else {
            return false;
        };
        self.meeting_scan = None;
        match result {
            Ok(scan) => {
                // A path may have been queued again while this worker was
                // indexing it. Keep the set for the already scheduled next
                // scan instead of acknowledging that newer request early.
                if !self.meeting_scan_again && self.notes_index_pending.is_none() {
                    for path in scan.persisted_extra {
                        self.meeting_scan_extra.remove(&path);
                    }
                }
                let mut recovered_open = false;
                for moved in scan.relocated {
                    if self.meeting_scan_extra.remove(&moved.old_base) {
                        self.meeting_scan_extra.insert(moved.new_base.clone());
                    }
                    if self.meeting_selected_base.as_ref() == Some(&moved.old_base) {
                        self.meeting_selected_base = Some(moved.new_base.clone());
                    }
                    if self.rename_base.as_ref() == Some(&moved.old_base) {
                        self.rename_base = Some(moved.new_base.clone());
                        recovered_open = true;
                    }
                    if self.meeting_tags_base.as_ref() == Some(&moved.old_base) {
                        self.meeting_tags_base = Some(moved.new_base.clone());
                        recovered_open = true;
                    }
                    if self.transcript_output.as_ref() == Some(&moved.old_base) {
                        self.transcript_output = Some(moved.new_base.clone());
                        self.transcript.source = self
                            .transcript
                            .source
                            .as_ref()
                            .map(|path| moved.relocate_path(path));
                        recovered_open = true;
                    }
                    if self.notes_base.as_ref() == Some(&moved.old_base) {
                        self.notes_base = Some(moved.new_base.clone());
                        if self.notes_reload_busy
                            || self
                                .notes_reload_result
                                .as_ref()
                                .is_some_and(|load| load.base == moved.old_base)
                        {
                            self.notes_initial_load_pending = true;
                        }
                        recovered_open = true;
                    }
                    if let Some(notes) = &mut self.live_notes
                        && notes.base == moved.old_base
                    {
                        notes.base = moved.new_base.clone();
                    }
                    #[cfg(feature = "online-speaker")]
                    if let Some(speaker) = &mut self.online_speakers
                        && speaker.base.as_ref() == Some(&moved.old_base)
                    {
                        speaker.base = Some(moved.new_base.clone());
                    }
                    self.ai.relocate_meeting(&moved.old_base, &moved.new_base);
                    if let Some(path) = &mut self.stats.output {
                        *path = moved.relocate_path(path);
                    }
                    if let Some(path) = &mut self.stats.audio_output {
                        *path = moved.relocate_path(path);
                    }
                    #[cfg(feature = "internal-player")]
                    if self
                        .pending_player
                        .as_ref()
                        .is_some_and(|(base, _, _)| base == &moved.old_base)
                    {
                        self.pending_player = None;
                        recovered_open = true;
                    }
                    #[cfg(feature = "internal-player")]
                    if self.player_base.as_ref() == Some(&moved.old_base) {
                        self.pending_player = None;
                        self.cancel_waveforms_for(&moved.old_base);
                        if let Some(player) = &self.player {
                            let _ = player.command(opencap_media::PlaybackCommand::Close);
                        }
                        self.player_base = None;
                        recovered_open = true;
                    }
                }
                if scan.cancelled {
                    self.message = "会议扫描已取消；原列表与已保存的索引保留。".into();
                } else {
                    if let Some(error) = scan.error {
                        self.message = format!("会议扫描或索引存在未完成项，原文件已保留：{error}");
                        if let Some(notice) = scan.notice {
                            self.message.push_str(&format!("；{notice}"));
                        }
                    } else if let Some(notice) = scan.notice {
                        self.message = notice;
                    }
                    if scan.root == self.config.output_dir {
                        if let Some(pending) = scan.pending_imports {
                            self.pending_legacy_imports = pending;
                            self.pending_legacy_import_page = self
                                .pending_legacy_import_page
                                .min(self.pending_legacy_imports.len().saturating_sub(1) / 5);
                        }
                        self.meetings = scan.files;
                        self.meeting_list_page = self
                            .meeting_list_page
                            .min(self.meetings.len().saturating_sub(1) / MEETINGS_PER_PAGE);
                        if self.meeting_search_query.is_some() {
                            self.search_meeting_page();
                        }
                    } else {
                        self.meeting_scan_again = true;
                    }
                }
                if recovered_open {
                    self.message.push_str(
                        " 已打开的会议路径已更新；如有回放请重新打开，未保存编辑保留在界面中。",
                    );
                }
            }
            Err(()) => self.message = "会议索引线程意外退出，原文件已保留；可点击刷新重试。".into(),
        }
        // Failed pending paths remain for the next explicit refresh. Retrying
        // continuously while the index is locked would spin up worker threads.
        if std::mem::take(&mut self.meeting_scan_again) {
            self.refresh_meetings();
        }
        true
    }
    pub(super) fn remember_meeting(&mut self) {
        if let Some(base) = &self.transcript_output {
            self.meeting_scan_extra.insert(base.clone());
        }
        self.refresh_meetings();
    }
    pub(super) fn sync_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(load) = self.notes_reload_result.take() {
            if self.notes_base.as_ref() != Some(&load.base)
                || self.transcript_output.as_ref() != Some(&load.base)
                || self.notes.read(cx).value().as_str() != load.editor_text
                || self.notes_saved != load.saved_text
                || self.notes_exists != load.saved_exists
            {
                self.notes_status = "笔记或会议在读取期间发生变化，未覆盖当前编辑。".into();
            } else {
                match load.result {
                    Ok(value) => {
                        self.notes_exists = value.is_some();
                        self.notes_saved = value.unwrap_or_default();
                        self.notes_load_failed = false;
                        self.notes_recovery = None;
                        let text = self.notes_saved.clone();
                        self.notes.update(cx, |s, cx| s.set_value(text, window, cx));
                        self.notes_status = "已载入磁盘笔记；原恢复副本仍保留。".into();
                    }
                    Err(error) => {
                        self.notes_status = format!("无法重新载入笔记；当前编辑保留：{error:#}")
                    }
                }
            }
        }
        if self.notes_base != self.transcript_output {
            self.persist_notes(cx);
            if self.notes_have_unsaved_work(cx) {
                self.notes_status = "原会议笔记尚未保存，编辑内容保留。".into();
                return;
            }
            self.notes_base = self.transcript_output.clone();
            self.notes_recovery = None;
            self.meeting_title = self
                .transcript_output
                .as_deref()
                .and_then(opencap_asr::meeting_name)
                .unwrap_or_else(|| self.transcript.title());
            self.notes_exists = false;
            self.notes_saved.clear();
            self.notes_load_failed = self.notes_base.is_some();
            self.notes_initial_load_pending = self.notes_base.is_some();
            self.notes
                .update(cx, |s, cx| s.set_value(String::new(), window, cx));
            self.notes_status = if self.notes_initial_load_pending {
                "正在后台读取会议笔记…".into()
            } else {
                "选择会议后可编辑笔记。".into()
            };
        }
        if self.notes_initial_load_pending
            && !self.notes_reload_busy
            && !self.notes_recovery_busy
            && !self.quit_pending
        {
            self.notes_initial_load_pending = false;
            self.reload_notes(cx);
        }
    }
    pub(super) fn persist_notes(&mut self, cx: &App) {
        if self.notes_initial_load_pending || self.notes_reload_busy {
            return;
        }
        let Some(base) = &self.notes_base else {
            return;
        };
        let text = self.notes.read(cx).value().to_string();
        if self.notes_recovery.as_deref() == Some(text.as_str()) {
            return;
        }
        if text == self.notes_saved {
            return;
        }
        if self.notes_load_failed {
            self.notes_status =
                "笔记尚未成功读取，当前编辑未覆盖磁盘；请先另存副本并重新载入。".into();
            return;
        }
        let result = opencap_core::managed_meeting_activity(base).and_then(|_activity| {
            let _guard = opencap_core::meeting_metadata_guard(base)?;
            let current = read_meeting_notes(base)?;
            let expected = self.notes_exists.then_some(self.notes_saved.as_str());
            anyhow::ensure!(
                current.as_deref() == expected,
                "笔记已被其他操作修改，当前编辑保留在窗口中"
            );
            anyhow::ensure!(text.len() <= 16_000_000, "笔记超过 16 MB，未覆盖原文件");
            opencap_asr::atomic_write(&base.with_extension("notes.txt"), text.as_bytes())
        });
        match result {
            Ok(()) => {
                let changed_base = base.clone();
                self.notes_saved = text;
                self.notes_exists = true;
                self.notes_load_failed = false;
                self.notes_recovery = None;
                self.notes_status = "笔记已自动保存到本机。Word 需点击导出以更新。".into();
                self.meeting_scan_extra.insert(changed_base);
                self.notes_index_pending = Some(std::time::Instant::now());
            }
            Err(e) => self.notes_status = format!("笔记保存失败：{e}"),
        }
    }
    pub(super) fn notes_have_unsaved_work(&self, cx: &App) -> bool {
        if self.notes_base.is_none() {
            return false;
        }
        let text = self.notes.read(cx).value().to_string();
        text != self.notes_saved && self.notes_recovery.as_deref() != Some(text.as_str())
    }
    fn reload_notes(&mut self, cx: &mut Context<Self>) {
        if self.notes_reload_busy || self.notes_recovery_busy || self.quit_pending {
            return;
        }
        if self.notes_have_unsaved_work(cx) {
            self.notes_status = "当前编辑尚未保存；请先另存笔记副本再重新载入。".into();
            cx.notify();
            return;
        }
        let Some(base) = self.notes_base.clone() else {
            return;
        };
        let editor_text = self.notes.read(cx).value().to_string();
        let saved_text = self.notes_saved.clone();
        let saved_exists = self.notes_exists;
        self.notes_reload_busy = true;
        self.notes_status = "正在后台读取已保存笔记…".into();
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _activity = opencap_core::managed_meeting_activity(&task_base)?;
                    let _guard = opencap_core::meeting_metadata_guard(&task_base)?;
                    read_meeting_notes(&task_base)
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.notes_reload_busy = false;
                if !this.quit_pending {
                    this.notes_reload_result = Some(NotesReload {
                        base,
                        editor_text,
                        saved_text,
                        saved_exists,
                        result,
                    });
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save_notes_recovery(&mut self, cx: &mut Context<Self>) {
        if self.notes_recovery_busy || self.quit_pending {
            return;
        }
        let Some(base) = self.notes_base.clone() else {
            return;
        };
        let text = self.notes.read(cx).value().to_string();
        if text.len() > 16_000_000 {
            self.notes_status = "笔记超过 16 MB，无法保存恢复副本。".into();
            cx.notify();
            return;
        }
        self.notes_recovery_busy = true;
        self.notes_status = "正在另存笔记恢复副本…".into();
        cx.spawn(async move |entity, cx| {
            let task_base = base.clone();
            let task_text = text.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _activity = opencap_core::managed_meeting_activity(&task_base)?;
                    let path = task_base.with_extension(format!(
                        "notes.recovery-{}.txt",
                        opencap_core::new_stable_id()
                    ));
                    opencap_core::atomic_create_bytes(&path, task_text.as_bytes())?;
                    let warning = path.file_name().and_then(|name| {
                        opencap_core::register_meeting_asset(
                            &task_base,
                            "notes_recovery",
                            opencap_core::AssetOwnership::Managed,
                            Path::new(name),
                        )
                        .err()
                        .map(|error| format!("资产登记失败：{error:#}"))
                    });
                    Ok::<_, anyhow::Error>((path, warning))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.notes_recovery_busy = false;
                match result {
                    Ok((path, warning)) => {
                        if this.notes_base.as_ref() == Some(&base)
                            && this.notes.read(cx).value().as_str() == text
                        {
                            this.notes_recovery = Some(text);
                        }
                        let warning = warning
                            .map(|value| format!("；{value}"))
                            .unwrap_or_default();
                        this.notes_status =
                            format!("笔记已另存恢复副本：{}{warning}", path.display());
                    }
                    Err(error) => {
                        this.notes_status = format!("恢复副本保存失败，编辑保留：{error:#}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn begin_edit_tags(&mut self, base: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.meeting_tags_base.is_some() {
            self.message = "已有标签编辑，请返回原会议保存或取消。".into();
            cx.notify();
            return;
        }
        let loaded = (|| -> anyhow::Result<_> {
            let before = opencap_core::read_manifest_snapshot(&base)?;
            let tags = opencap_asr::meeting_tags(&base)?;
            let after = opencap_core::read_manifest_snapshot(&base)?;
            anyhow::ensure!(
                before.as_ref().map(|(_, fingerprint)| fingerprint)
                    == after.as_ref().map(|(_, fingerprint)| fingerprint),
                "读取标签时会议清单发生变化，请重试"
            );
            Ok((tags, before.map(|(manifest, _)| manifest.meeting_id)))
        })();
        match loaded {
            Ok((tags, identity)) => {
                self.meeting_tags_input
                    .update(cx, |input, cx| input.set_value(tags.join("，"), window, cx));
                self.meeting_tags_baseline = Some(tags);
                self.meeting_tags_identity = Some(identity);
                self.meeting_tags_base = Some(base);
            }
            Err(error) => self.message = format!("无法读取标签：{error:#}"),
        }
        cx.notify();
    }
    fn save_tags(&mut self, cx: &mut Context<Self>) {
        let Some(base) = self.meeting_tags_base.clone() else {
            return;
        };
        let text = self.meeting_tags_input.read(cx).value().to_string();
        let Some(baseline) = self.meeting_tags_baseline.as_deref() else {
            self.message = "缺少标签编辑基线，请重新打开编辑器。".into();
            cx.notify();
            return;
        };
        let Some(identity) = self.meeting_tags_identity.as_ref() else {
            self.message = "缺少会议身份基线，请重新打开编辑器。".into();
            cx.notify();
            return;
        };
        match opencap_asr::save_meeting_tags_for_identity(
            &base,
            &text,
            Some(baseline),
            Some(identity.as_deref()),
        ) {
            Ok((_, warning)) => {
                self.meeting_tags_base = None;
                self.meeting_tags_baseline = None;
                self.meeting_tags_identity = None;
                self.meeting_scan_extra.insert(base);
                self.message = warning
                    .map(|warning| format!("标签已保存，但资产登记失败，请检查会议清单：{warning}"))
                    .unwrap_or_else(|| "标签已保存。".into());
                self.refresh_meetings();
            }
            Err(error) => self.message = format!("标签未保存：{error:#}"),
        }
        cx.notify();
    }
    fn meeting_locations(&self, cx: &Context<Self>) -> gpui::component::popover::Popover {
        let folder = self
            .transcript_output
            .as_ref()
            .and_then(|p| p.parent())
            .unwrap_or(&self.config.output_dir)
            .to_path_buf();
        let open = folder.clone();
        let copy = folder.clone();
        let mut content = div()
            .w(px(340.))
            .flex()
            .flex_col()
            .gap_2()
            .child(label("会议文件夹"))
            .child(label(folder.display().to_string()))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("copy-meeting-location", "复制路径", false).on_click(
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copy.display().to_string(),
                            ))
                        },
                    ))
                    .child(button("meeting-location", "打开文件夹", false).on_click(
                        move |_, _, _| {
                            let _ = opencap_platform::open_path(&open);
                        },
                    )),
            );
        if let Some(source) = &self.transcript.source
            && source.parent() != Some(folder.as_path())
            && let Some(parent) = source.parent()
        {
            let path = parent.to_path_buf();
            content = content.child(label("原音视频保留在原位置")).child(
                button("original-media-location", "打开原文件位置", false).on_click(
                    move |_, _, _| {
                        let _ = opencap_platform::open_path(&path);
                    },
                ),
            );
        }
        content = content.child(
            button("rename-current-meeting", "修改显示名称", false)
                .disabled(self.active())
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(base) = this.transcript_output.clone() {
                        this.begin_rename(base, false, window, cx);
                    }
                })),
        );
        content = content.child(
            button("edit-meeting-tags", "编辑标签", false)
                .disabled(self.transcript_output.is_none() || self.rename_job.is_some())
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(base) = this.transcript_output.clone() {
                        this.begin_edit_tags(base, window, cx);
                    }
                })),
        );
        let current_meeting = self
            .transcript_output
            .as_ref()
            .and_then(|base| self.meetings.iter().find(|meeting| &meeting.base == base));
        let managed =
            current_meeting.is_some_and(|meeting| meeting.available && meeting.manifest_available);
        let legacy = current_meeting.is_some_and(|meeting| {
            meeting.available && !meeting.manifest_present && !meeting.manifest_read_error
        });
        if legacy {
            content = content
                .child(
                    button("migrate-current-reference", "迁移副本 · 引用媒体", false)
                        .disabled(self.rename_job.is_some())
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(base) = this.transcript_output.clone() {
                                this.begin_legacy_migration(base, false, window, cx);
                            }
                        })),
                )
                .child(
                    button("migrate-current-archive", "迁移副本 · 复制媒体", false)
                        .disabled(self.rename_job.is_some())
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(base) = this.transcript_output.clone() {
                                this.begin_legacy_migration(base, true, window, cx);
                            }
                        })),
                );
        }
        if self.transcript_output.is_some()
            && current_meeting
                .is_none_or(|meeting| !meeting.available || meeting.manifest_read_error)
        {
            content = content.child(label("会议文件状态尚未确认；请刷新会议库后再改名或迁移。"));
        }
        content = content.child(
            button("rename-current-files", "修改名称与文件", false)
                .disabled(!managed || self.rename_job.is_some())
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(base) = this.transcript_output.clone() {
                        this.begin_rename(base, true, window, cx);
                    }
                })),
        );
        gpui::component::popover::Popover::new("meeting-locations-popover")
            .anchor(Anchor::TopRight)
            .trigger(button("toggle-meeting-locations", "文件位置", false).ghost())
            .child(content)
    }
    pub(super) fn meeting_header(&self, compact: bool, cx: &Context<Self>) -> Div {
        let back = button(
            "back-meetings",
            if compact { "列表" } else { "返回列表" },
            false,
        )
        .icon(gpui::component::IconName::ArrowLeft)
        .ghost()
        .on_click(cx.listener(|this, _, _, cx| {
            this.meeting_page = MeetingPage::Library;
            this.refresh_meetings();
            this.page_scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        }));
        let title = div()
            .flex_1()
            .min_w(px(0.))
            .font_weight(FontWeight::SEMIBOLD)
            .truncate()
            .child(if self.meeting_title.is_empty() {
                self.transcript.title()
            } else {
                self.meeting_title.clone()
            });
        let tabs = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .children(
                [
                    (MeetingPage::Transcript, "逐字稿"),
                    (MeetingPage::Minutes, "AI 纪要"),
                    (MeetingPage::Notes, "笔记"),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (page, text))| {
                    button(("meeting-section", i), text, self.meeting_page == page).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.meeting_page = page;
                            this.page_scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        }),
                    )
                }),
            );
        let mut header = div()
            .flex_shrink_0()
            .px_4()
            .py_2()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_2();
        if compact {
            header = header.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(back)
                    .child(title)
                    .child(tabs)
                    .child(self.meeting_locations(cx)),
            );
        } else {
            header = header
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(back)
                        .child(title)
                        .child(self.meeting_locations(cx)),
                )
                .child(tabs);
        }
        if self.rename_base.is_some() && self.rename_base == self.transcript_output {
            header = header.child(self.rename_editor(cx));
        }
        if self.meeting_tags_base.is_some() && self.meeting_tags_base == self.transcript_output {
            header = header.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(gpui::component::input::Input::new(&self.meeting_tags_input).flex_1())
                    .child(
                        primary_button("save-meeting-tags", "保存标签")
                            .on_click(cx.listener(|this, _, _, cx| this.save_tags(cx))),
                    )
                    .child(
                        button("copy-meeting-tags-draft", "复制编辑内容", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let text = this.meeting_tags_input.read(cx).value().to_string();
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                this.message =
                                    "标签编辑内容已复制；可取消并重新打开以读取最新标签。".into();
                                cx.notify();
                            })),
                    )
                    .child(
                        button("cancel-meeting-tags", "取消", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.meeting_tags_base = None;
                                this.meeting_tags_baseline = None;
                                this.meeting_tags_identity = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        header
    }
    pub(super) fn meeting_workspace(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        match self.meeting_page {
            MeetingPage::Library => self.library_view(width, height, cx),
            MeetingPage::ReadOnly => self.readonly_meeting_view(cx),
            MeetingPage::Transcript => self.transcription_view(width, cx),
            MeetingPage::Minutes => self.minutes_view(width, height, cx),
            MeetingPage::Notes => self.notes_view(cx),
        }
    }

    fn readonly_meeting_view(&self, cx: &Context<Self>) -> Div {
        let Some(meeting) = self.readonly_meeting.as_ref() else {
            return div().child(label("未选择会议。"));
        };
        let mut content = div()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button("readonly-back-library", "返回会议文件", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.readonly_load_generation =
                                    this.readonly_load_generation.wrapping_add(1);
                                this.readonly_meeting = None;
                                this.meeting_page = MeetingPage::Library;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(meeting.title.clone()),
                    )
                    .child(ui::badge("只读", ACCENT, SOFT))
                    .child(
                        button("readonly-return-recording", "返回录制", false)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.transcription_tab = false;
                                this.settings = false;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    ),
            );
        if meeting.loading {
            content = content.child(label("正在读取逐字稿…"));
        } else if let Some(error) = meeting.error.as_ref() {
            content = content.child(label(format!("无法读取逐字稿：{error}")));
        } else {
            content = content.child(
                ui::card()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(self.readonly_view.clone()),
            );
        }
        content
    }

    pub(super) fn open_readonly_library_meeting(
        &mut self,
        meeting: MeetingFile,
        cx: &mut Context<Self>,
    ) {
        if !meeting.transcript_available {
            self.message = "此会议没有可查看的逐字稿。".into();
            cx.notify();
            return;
        }
        let base = meeting.base.clone();
        let path = base.with_extension("transcript.json");
        let expected_id = meeting.manifest_meeting_id.clone();
        self.readonly_load_generation = self.readonly_load_generation.wrapping_add(1);
        let generation = self.readonly_load_generation;
        self.readonly_meeting = Some(ReadOnlyMeeting {
            base: base.clone(),
            title: meeting.title(),
            loading: true,
            error: None,
        });
        self.settings = false;
        self.transcription_tab = true;
        self.meeting_page = MeetingPage::ReadOnly;
        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    transcription::read_transcript_snapshot(&path, expected_id.as_deref())
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.readonly_load_generation != generation
                    || this.meeting_page != MeetingPage::ReadOnly
                    || this
                        .readonly_meeting
                        .as_ref()
                        .is_none_or(|meeting| meeting.base != base)
                {
                    return;
                }
                if let Some(meeting) = this.readonly_meeting.as_mut() {
                    meeting.loading = false;
                    match result {
                        Ok(transcript) => this
                            .readonly_view
                            .update(cx, |view, cx| view.replace(&transcript, true, false, cx)),
                        Err(error) => meeting.error = Some(format!("{error:#}")),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn select_library_meeting(&mut self, base: PathBuf) {
        if self.meeting_selected_base.as_ref() != Some(&base)
            && let Some(cancel) = self.transcript_load_cancel.take()
        {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.transcript_load_generation = self.transcript_load_generation.wrapping_add(1);
            self.transcription_status = "已取消上一次逐字稿加载。".into();
        }
        #[cfg(feature = "internal-player")]
        if self
            .pending_player
            .as_ref()
            .is_some_and(|(pending_base, _, _)| pending_base != &base)
        {
            self.pending_player = None;
            self.message = "已取消切换到上一场会议的播放。".into();
        }
        self.meeting_selected_base = Some(base);
        self.meeting_detail_scroll.set_offset(point(px(0.), px(0.)));
    }

    #[cfg(feature = "internal-player")]
    pub(super) fn can_retry_library_player(&self, base: &Path, media: &Path) -> bool {
        self.transcription_tab
            && self.meeting_page == MeetingPage::Library
            && self.meeting_selected_base.as_deref() == Some(base)
            && self.meetings.iter().any(|meeting| {
                meeting.base.as_path() == base
                    && meeting.available
                    && !meeting.transcript_available
                    && !meeting.media_is_candidate
                    && meeting.media_available == Some(true)
                    && meeting.media.as_deref() == Some(media)
            })
    }

    pub(super) fn open_library_meeting_for_review(&mut self, base: &Path, cx: &mut Context<Self>) {
        if self.quit_pending {
            self.message = "退出期间不能打开会议。".into();
            return;
        }
        let Some(meeting) = self
            .meetings
            .iter()
            .find(|meeting| meeting.base.as_path() == base)
            .cloned()
        else {
            self.message = "会议列表已变化，请重新选择。".into();
            return;
        };
        if self.active() {
            self.open_readonly_library_meeting(meeting, cx);
            return;
        }
        let transcript = meeting
            .transcript_available
            .then(|| meeting.base.with_extension("transcript.json"));
        #[cfg(feature = "internal-player")]
        {
            let media = meeting.media.clone().filter(|_| {
                meeting.available
                    && meeting.media_available == Some(true)
                    && !meeting.media_is_candidate
            });
            if let Some(transcript) = transcript {
                let expected_base = meeting.base;
                let expected_media = media;
                let expected_meeting_id = meeting.manifest_meeting_id;
                self.load_transcript_async_then(transcript, cx, move |this, cx| {
                    let identity_matches = expected_meeting_id.as_deref().is_none_or(|id| {
                        this.transcript
                            .run
                            .as_ref()
                            .and_then(|run| run.meeting_id.as_deref())
                            .is_none_or(|recorded| recorded == id)
                    });
                    let meeting_matches = this.meetings.iter().any(|meeting| {
                        meeting.base == expected_base
                            && meeting.available
                            && !meeting.media_is_candidate
                            && meeting.media_available == Some(true)
                            && meeting.media == expected_media
                            && meeting.manifest_meeting_id == expected_meeting_id
                    });
                    if identity_matches
                        && meeting_matches
                        && this.transcript_output.as_ref() == Some(&expected_base)
                        && expected_media
                            .as_ref()
                            .is_some_and(|media| this.transcript.source.as_ref() == Some(media))
                    {
                        this.open_internal_player(
                            expected_base,
                            expected_media.expect("已核对媒体"),
                        );
                    } else {
                        this.transcription_status =
                            "已打开逐字稿；媒体不可用或身份待核对，请在会议库检查。".into();
                    }
                    cx.notify();
                });
            } else if let Some(media) = media {
                self.open_internal_player(meeting.base, media);
            } else {
                self.message = "会议没有可用的已确认媒体，请检查文件或重新定位。".into();
            }
        }
        #[cfg(not(feature = "internal-player"))]
        {
            if let Some(transcript) = transcript {
                self.load_transcript_async_then(transcript, cx, |_, _| {});
            } else {
                self.message = "当前构建未启用内置播放器，且此会议没有逐字稿。".into();
            }
        }
    }

    fn library_view(&self, width: f32, height: f32, cx: &Context<Self>) -> Div {
        let busy = self.active() || self.meeting_task_busy();
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().min_w(px(200.)).child(
                            gpui::component::input::Input::new(&self.meeting_search_input)
                                .prefix(
                                    ui::icon(gpui::component::IconName::Search)
                                        .size(px(16.))
                                        .text_color(rgb(MUTED)),
                                )
                                .w_full(),
                        ),
                    )
                    .child(
                        primary_button("import-meeting", "导入音频 / 视频")
                            .icon(gpui::component::IconName::Plus)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_transcription_file(false, cx)
                            })),
                    )
                    .child(
                        ui::icon_button(
                            "search-meetings",
                            gpui::component::IconName::Search,
                            "搜索会议",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let query = this.meeting_search_input.read(cx).value().to_string();
                            this.start_meeting_search(query);
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::icon_button(
                            "toggle-meeting-filters",
                            gpui::assets::IconName::ListFilter,
                            "筛选会议",
                        )
                        .selected(
                            self.meeting_filter_open || !self.meeting_search_filters.is_empty(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.meeting_filter_open = !this.meeting_filter_open;
                            cx.notify();
                        })),
                    )
                    .when(
                        self.meeting_search_query
                            .as_ref()
                            .is_some_and(|query| !query.is_empty()),
                        |d| {
                            d.child(
                                ui::icon_button(
                                    "clear-meeting-search",
                                    gpui::assets::IconName::X,
                                    "清除搜索词",
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.meeting_search_input.update(cx, |input, cx| {
                                            input.set_value("", window, cx)
                                        });
                                        this.start_meeting_search(String::new());
                                        cx.notify();
                                    },
                                )),
                            )
                        },
                    ),
            )
            .when(
                self.meetings
                    .iter()
                    .any(|meeting| !meeting.available || meeting.media_available == Some(false)),
                |d| {
                    let count = self
                        .meetings
                        .iter()
                        .filter(|meeting| {
                            !meeting.available || meeting.media_available == Some(false)
                        })
                        .count();
                    let first_missing = self
                        .meetings
                        .iter()
                        .find(|meeting| !meeting.available)
                        .map(|meeting| meeting.base.clone());
                    d.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(ui::badge(
                                format!("{count} 场文件或媒体缺失"),
                                0x9a5a10,
                                0xfff4e7,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .when_some(first_missing, |row, base| {
                                        row.child(
                                            button("relink-first-missing", "逐场重新定位…", false)
                                                .ghost()
                                                .disabled(self.meeting_task_busy() || self.active())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.choose_relink_meeting(base.clone(), cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        button("relink-library-root", "重新定位目录…", false)
                                            .ghost()
                                            .disabled(self.meeting_task_busy() || self.active())
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.choose_relink_library_root(cx);
                                            })),
                                    ),
                            ),
                    )
                },
            )
            .when(self.meeting_filter_open, |d| {
                d.child(self.meeting_filter_panel(cx))
            })
            .when_some(
                self.meeting_search_result
                    .as_ref()
                    .and_then(|result| result.notice.clone()),
                |d, notice| d.child(label(notice)),
            )
            .when(self.meeting_scan.is_some(), |d| {
                d.child(
                    button("cancel-meeting-scan", "取消会议扫描", false)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.meeting_scan_again = false;
                            // Cancel the refresh already queued before this
                            // click; later edits schedule a fresh request.
                            this.notes_index_pending = None;
                            this.meeting_scan_cancelled
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            this.message = "正在停止会议扫描…".into();
                            cx.notify();
                        })),
                )
            })
            .when(self.transcription_busy(), |d| {
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(ui::badge("处理中", ACCENT, SOFT))
                        .child(
                            label(self.transcription_status.clone())
                                .flex_1()
                                .min_w(px(0.))
                                .truncate(),
                        )
                        .child(button("current-task", "查看", false).ghost().on_click(
                            cx.listener(|this, _, _, cx| {
                                this.meeting_page = MeetingPage::Transcript;
                                cx.notify();
                            }),
                        )),
                )
            })
            .when(self.transcript_load_cancel.is_some(), |d| {
                d.child(label("正在后台读取逐字稿…"))
            })
            .when(self.active(), |d| {
                d.child(label("录制中可查看其他逐字稿；播放需先停止录制。"))
            })
            .when_some(self.audio_export_job.as_ref(), |d, (base, _, cancel, _)| {
                let expected = cancel.clone();
                let cancelling = cancel.load(std::sync::atomic::Ordering::Acquire);
                let stage = match self.audio_export_stage_seen {
                    1 => "正在核对源录屏",
                    2 => "正在导出音频",
                    3 => "正在复核源录屏",
                    4 => "正在登记并核对音频",
                    _ => "正在准备音频导出",
                };
                d.child(
                    card()
                        .child(label(format!(
                            "{} · {}",
                            stage,
                            base.file_name().unwrap_or_default().to_string_lossy()
                        )))
                        .child(
                            button(
                                "cancel-background-audio-export",
                                if cancelling {
                                    "正在取消…"
                                } else {
                                    "取消音频导出"
                                },
                                false,
                            )
                            .ghost()
                            .disabled(cancelling)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some((_, _, flag, _)) = &this.audio_export_job
                                        && std::sync::Arc::ptr_eq(&expected, flag)
                                    {
                                        flag.store(true, std::sync::atomic::Ordering::Release);
                                        this.message = "正在取消音频导出，等待当前读取结束…".into();
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                )
            })
            .when(self.relink_busy, |d| {
                d.child(
                    button(
                        "cancel-media-relink",
                        if self
                            .relink_cancelled
                            .load(std::sync::atomic::Ordering::Relaxed)
                        {
                            "正在取消媒体核对…"
                        } else {
                            "取消媒体核对"
                        },
                        false,
                    )
                    .disabled(
                        self.relink_cancelled
                            .load(std::sync::atomic::Ordering::Relaxed),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.relink_cancelled
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        this.message = "正在取消媒体核对；等待当前读取结束。".into();
                        cx.notify();
                    })),
                )
            })
            .when(
                self.rename_job.as_ref().is_some_and(|job| {
                    matches!(
                        &job.mode,
                        RenameMode::Legacy { .. } | RenameMode::ResumeLegacy { .. }
                    )
                }),
                |d| {
                    let cancelled = self
                        .rename_job
                        .as_ref()
                        .unwrap()
                        .cancelled
                        .load(std::sync::atomic::Ordering::Relaxed);
                    d.child(
                        button(
                            "cancel-legacy-migration",
                            if cancelled {
                                "正在取消迁移…"
                            } else {
                                "取消迁移"
                            },
                            false,
                        )
                        .disabled(cancelled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(job) = &this.rename_job {
                                job.cancelled
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                                this.message =
                                    "正在取消迁移；等待当前文件操作结束，原会议保留。".into();
                            }
                            cx.notify();
                        })),
                    )
                },
            );
        #[cfg(feature = "online-speaker")]
        if let Some(cancel) = &self.word_timing_cancel {
            let expected = cancel.clone();
            let cancelling = cancel.load(std::sync::atomic::Ordering::Relaxed);
            view = view.child(
                card().child(label(self.word_timing_status.clone())).child(
                    button(
                        "cancel-background-word-timing",
                        if cancelling {
                            "正在停止…"
                        } else {
                            "停止词时间任务"
                        },
                        false,
                    )
                    .ghost()
                    .disabled(cancelling)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(flag) = &this.word_timing_cancel
                            && std::sync::Arc::ptr_eq(flag, &expected)
                        {
                            flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            this.word_timing_status = "正在停止，需等待当前模型调用结束。".into();
                        }
                        cx.notify();
                    })),
                ),
            );
        }
        if !self.pending_legacy_imports.is_empty() {
            const PAGE: usize = 5;
            let pages = self.pending_legacy_imports.len().saturating_sub(1) / PAGE + 1;
            let page = self.pending_legacy_import_page.min(pages - 1);
            let mut recovery = card().child(label(format!(
                "迁移暂存 · {} 项。可从原文件重新创建副本，已有暂存保留；重试前请检查会议库是否已有完整副本。",
                self.pending_legacy_imports.len()
            )));
            for (index, pending) in self
                .pending_legacy_imports
                .iter()
                .enumerate()
                .skip(page * PAGE)
                .take(PAGE)
            {
                let stage = pending.stage.clone();
                let mut row = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_2()
                    .child(label(format!("暂存：{}", stage.display())));
                if let Some(request) = &pending.request {
                    let request = request.clone();
                    row = row
                        .child(label(format!(
                            "原会议：{} · 名称：{} · {}",
                            request.original_base.display(),
                            request.title,
                            if request.archive_media {
                                "复制媒体"
                            } else {
                                "引用原媒体"
                            }
                        )))
                        .child(
                            button(("retry-legacy-import", index), "从原文件重试迁移", false)
                                .ghost()
                                .disabled(
                                    self.rename_job.is_some()
                                        || self.rename_base.is_some()
                                        || self.meeting_scan.is_some(),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_pending_import_recovery(request.clone(), cx);
                                })),
                        );
                } else {
                    row = row.child(label(format!(
                        "暂不能自动重试：{}。如仍在复制，请稍后刷新。",
                        pending.detail.as_deref().unwrap_or("缺少恢复记录")
                    )));
                }
                row = row.child(
                    button(("open-legacy-import-stage", index), "打开暂存目录", false)
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(error) = opencap_platform::open_path(&stage) {
                                this.message = format!("无法打开迁移暂存目录：{error:#}");
                            }
                            cx.notify();
                        })),
                );
                recovery = recovery.child(row);
            }
            if pages > 1 {
                recovery = recovery.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            button("pending-import-prev", "上一页", false)
                                .ghost()
                                .disabled(page == 0)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pending_legacy_import_page = page.saturating_sub(1);
                                    cx.notify();
                                })),
                        )
                        .child(label(format!("{} / {}", page + 1, pages)))
                        .child(
                            button("pending-import-next", "下一页", false)
                                .ghost()
                                .disabled(page + 1 >= pages)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pending_legacy_import_page = (page + 1).min(pages - 1);
                                    cx.notify();
                                })),
                        ),
                );
            }
            view = view.child(recovery);
        }
        if self.meetings.is_empty() && self.meeting_scan.is_some() {
            view = view.child(label("正在后台读取会议索引…"));
        } else if self.meetings.is_empty() {
            view = view.child(
                card()
                    .py_8()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .size(px(64.))
                            .rounded_xl()
                            .bg(rgb(SOFT))
                            .text_color(rgb(ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon(gpui::component::IconName::FolderOpen).size(px(28.))),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("让下一场会议留下记录"),
                    )
                    .child(label("录制一场会议，或导入已有音频和视频。"))
                    .child(
                        primary_button("empty-record", "开始一次录制").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.transcription_tab = false;
                                this.settings = false;
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        let page_start = self.meeting_list_page * MEETINGS_PER_PAGE;
        let weekly = self.config.meeting_group_by_week;
        let grouped = (self.config.meeting_group_by_month || weekly)
            && self.meeting_search_result.is_none()
            && self.meeting_search_query.is_none();
        let rows: Vec<_> = if let Some(result) = &self.meeting_search_result {
            result
                .hits
                .iter()
                .filter_map(|hit| {
                    self.meetings
                        .iter()
                        .enumerate()
                        .find(|(_, meeting)| meeting.base == hit.base)
                        .map(|(index, meeting)| (index, meeting, Some(hit)))
                })
                .collect()
        } else if self.meeting_search_query.is_some() {
            Vec::new()
        } else if grouped {
            let mut all: Vec<_> = self.meetings.iter().enumerate().collect();
            all.sort_by(|(_, left), (_, right)| {
                meeting_group_key(right, weekly)
                    .cmp(&meeting_group_key(left, weekly))
                    .then_with(|| {
                        let l = left.recording_started_ms.unwrap_or_else(|| {
                            left.modified
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64
                        });
                        let r = right.recording_started_ms.unwrap_or_else(|| {
                            right
                                .modified
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64
                        });
                        r.cmp(&l)
                    })
            });
            all.into_iter()
                .map(|(index, meeting)| (index, meeting, None))
                .collect()
        } else {
            self.meetings
                .iter()
                .enumerate()
                .skip(page_start)
                .take(MEETINGS_PER_PAGE)
                .map(|(index, meeting)| (index, meeting, None))
                .collect()
        };
        if self
            .meeting_search_result
            .as_ref()
            .is_some_and(|result| result.total == 0)
        {
            view = view.child(label("没有找到匹配的会议。"));
        }
        let is_visible = |meeting: &MeetingFile| {
            !grouped
                || !self
                    .meeting_collapsed_months
                    .contains(&meeting_group_key(meeting, weekly))
        };
        let selected_base = self
            .meeting_selected_base
            .as_ref()
            .filter(|base| rows.iter().any(|(_, meeting, _)| &meeting.base == *base))
            .cloned()
            .or_else(|| rows.first().map(|(_, meeting, _)| meeting.base.clone()));
        let visible_bases: Vec<_> = rows
            .iter()
            .filter(|(_, meeting, _)| is_visible(meeting))
            .map(|(_, meeting, _)| meeting.base.clone())
            .collect();
        #[cfg(feature = "internal-player")]
        if let (Some(base), Some(media), Some(snapshot)) =
            (&self.player_base, &self.player_media, &self.player_snapshot)
            && selected_base.as_ref() != Some(base)
            && !matches!(
                snapshot.state,
                opencap_media::PlaybackState::Closing | opencap_media::PlaybackState::Closed
            )
        {
            let expected_base = base.clone();
            let expected_media = media.clone();
            let expected_generation = snapshot.generation;
            let playing = self.player_start_pending
                || matches!(
                    snapshot.state,
                    opencap_media::PlaybackState::Playing | opencap_media::PlaybackState::Buffering
                );
            let failed = snapshot.state == opencap_media::PlaybackState::Failed;
            let name = base.file_name().unwrap_or_default().to_string_lossy();
            view = view.child(
                card().child(label(format!("当前播放：{name}"))).child(
                    button(
                        "library-background-player-toggle",
                        if playing {
                            "暂停播放"
                        } else {
                            "继续播放"
                        },
                        false,
                    )
                    .ghost()
                    .disabled(self.active() || failed || self.pending_player.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.active()
                            || this.pending_player.is_some()
                            || this.player_base.as_ref() != Some(&expected_base)
                            || this.player_media.as_ref() != Some(&expected_media)
                        {
                            return;
                        }
                        if let Some(player) = &this.player {
                            let current = player.snapshot();
                            let still_playing = this.player_start_pending
                                || matches!(
                                    current.state,
                                    opencap_media::PlaybackState::Playing
                                        | opencap_media::PlaybackState::Buffering
                                );
                            if current.generation != expected_generation
                                || still_playing != playing
                                || matches!(
                                    current.state,
                                    opencap_media::PlaybackState::Failed
                                        | opencap_media::PlaybackState::Closing
                                        | opencap_media::PlaybackState::Closed
                                )
                            {
                                this.message = "播放状态已变化，请重新操作。".into();
                            } else {
                                let command = if playing {
                                    opencap_media::PlaybackCommand::Pause
                                } else {
                                    opencap_media::PlaybackCommand::Play
                                };
                                this.send_player(command);
                            }
                        }
                        cx.notify();
                    })),
                ),
            );
        }
        let mut rows_view = div().w_full().flex().flex_col().gap_1();
        let mut detail_view: Option<Div> = None;
        let mut last_month: Option<String> = None;
        let mut visible_row_positions = Vec::new();
        let mut row_position = 0usize;
        for (i, meeting, hit) in rows {
            if grouped {
                let month = meeting_group_key(meeting, weekly);
                if last_month.as_deref() != Some(month.as_str()) {
                    let collapsed = self.meeting_collapsed_months.contains(&month);
                    let group_count = self
                        .meetings
                        .iter()
                        .filter(|other| meeting_group_key(other, weekly) == month)
                        .count();
                    let month_for_click = month.clone();
                    rows_view = rows_view.child(
                        Button::new(("meeting-month", i))
                            .accessibility_label(format!(
                                "{}，{}场，{}",
                                meeting_group_label(&month, weekly),
                                group_count,
                                if collapsed { "已收起" } else { "已展开" }
                            ))
                            .ghost()
                            .w_full()
                            .h(px(40.))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(0xf8fafd))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.meeting_collapsed_months.remove(&month_for_click) {
                                    this.meeting_collapsed_months
                                        .insert(month_for_click.clone());
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                ui::icon(if collapsed {
                                                    gpui::component::IconName::ChevronRight
                                                } else {
                                                    gpui::component::IconName::ChevronDown
                                                })
                                                .size(px(14.)),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(meeting_group_label(&month, weekly)),
                                            ),
                                    )
                                    .child(label(format!("{group_count} 场"))),
                            ),
                    );
                    row_position += 1;
                    last_month = Some(month.clone());
                }
            }
            let row_base = meeting.base.clone();
            let selected_row = selected_base.as_ref() == Some(&meeting.base);
            let subtitle = hit
                .map(|hit| {
                    format!(
                        "{}{} · {}",
                        if hit.stale {
                            "索引可能过期 · "
                        } else {
                            ""
                        },
                        hit.field,
                        hit.excerpt.replace('\n', " ")
                    )
                })
                .unwrap_or_else(|| format!("{} · {}", meeting.display_stamp, meeting.status()));
            let accessible_row = format!("{}，{}，{}", meeting.title(), subtitle, meeting.age());
            if is_visible(meeting) {
                visible_row_positions.push(row_position);
                row_position += 1;
                rows_view = rows_view.child(
                    div()
                        .id(("meeting-row", i))
                        .role(gpui::Role::ListBoxOption)
                        .aria_label(accessible_row)
                        .aria_selected(selected_row)
                        .when(selected_row, |row| row.aria_active_descendant())
                        .h(px(if self.config.meeting_list_compact {
                            48.
                        } else {
                            60.
                        }))
                        .flex_shrink_0()
                        .px_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(if selected_row { 0xc7d6ff } else { SURFACE }))
                        .flex()
                        .items_center()
                        .gap_3()
                        .cursor_pointer()
                        .when(selected_row, |d| d.bg(rgb(SOFT)))
                        .hover(|style| style.bg(rgb(SOFT)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(meeting.title()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .truncate()
                                        .child(subtitle),
                                ),
                        )
                        .child(ui::badge(
                            if !meeting.available {
                                "文件缺失"
                            } else if meeting.transcript_read_error {
                                "转写数据未能读取"
                            } else if meeting.summary_available {
                                "已生成纪要"
                            } else if meeting.transcript_available {
                                "仅逐字稿"
                            } else {
                                "待转录"
                            },
                            if meeting.summary_available {
                                ui::SUCCESS
                            } else {
                                ACCENT
                            },
                            if meeting.summary_available {
                                0xe6f4ef
                            } else {
                                SOFT
                            },
                        ))
                        .on_click(cx.listener(
                            move |this, event: &gpui::ClickEvent, window, cx| {
                                this.meeting_list_focus.focus(window, cx);
                                this.select_library_meeting(row_base.clone());
                                if event.click_count() == 2 {
                                    this.open_library_meeting_for_review(&row_base, cx);
                                }
                                cx.notify();
                            },
                        )),
                );
            }
            if !selected_row {
                continue;
            }
            let rename_base = meeting.base.clone();
            let relink_folder_base = meeting.base.clone();
            let entity_base = meeting.base.clone();
            let migrate_reference = meeting.base.clone();
            let migrate_archive = meeting.base.clone();
            let managed = meeting.manifest_available;
            let transcript = meeting.base.with_extension("transcript.json");
            let exists = meeting.available && meeting.transcript_available;
            let media = meeting.media.clone();
            let playable = meeting.available && meeting.media_available == Some(true);
            let audio_export = meeting
                .manifest_meeting_id
                .clone()
                .zip(media.clone())
                .filter(|(_, source)| {
                    playable
                        && !meeting.media_is_candidate
                        && source == &meeting.base.with_extension("mp4")
                })
                .map(|(id, source)| (meeting.base.clone(), id, source));
            #[cfg(feature = "internal-player")]
            let internal_action = media.clone().filter(|_| playable).map(|path| {
                let base = meeting.base.clone();
                button(
                    ("play-internal", i),
                    if meeting.media_is_candidate {
                        "内置播放候选媒体"
                    } else {
                        "播放原音 / 视频"
                    },
                    false,
                )
                .icon(gpui::component::IconName::Play)
                .disabled(self.active())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_internal_player(base.clone(), path.clone());
                    cx.notify();
                }))
            });
            #[cfg(not(feature = "internal-player"))]
            let internal_action: Option<Button> = None;
            let has_internal_action = internal_action.is_some();
            let selected = transcript.clone();
            let hit_field = hit.map(|hit| hit.field.clone());
            let hit_query = self.meeting_search_query.clone().unwrap_or_default();
            let hit_start = hit.and_then(|hit| hit.start_ms);
            let hit_end = hit.and_then(|hit| hit.end_ms);
            let hit_semantic_source = hit.and_then(|hit| hit.semantic_source.clone());
            let hit_source_stamp = hit.and_then(|hit| hit.source_stamp.clone());
            #[cfg(feature = "internal-player")]
            let hit_play_action = if (hit_field.as_deref() == Some("逐字稿")
                && !hit_query.is_empty()
                || (hit_field.as_deref() == Some("语义·逐字稿")
                    && hit_start.zip(hit_end).is_some()
                    && hit_semantic_source.is_some()))
                && exists
            {
                let transcript = transcript.clone();
                let expected_base = meeting.base.clone();
                let query = hit_query.clone();
                let source_stamp = hit_source_stamp.clone();
                let semantic_source = hit_semantic_source.clone();
                Some(
                    button(
                        ("play-search-hit", i),
                        if meeting.media_is_candidate {
                            "来源待核对"
                        } else {
                            "回听命中"
                        },
                        false,
                    )
                    .ghost()
                    .disabled(busy || !playable || meeting.media_is_candidate)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !search::source_is_current(&expected_base, &source_stamp) {
                            this.transcription_status = "索引来源已变化，请刷新后再回听。".into();
                            cx.notify();
                            return;
                        }
                        let target = transcript.clone();
                        let expected_base = expected_base.clone();
                        let source_stamp = source_stamp.clone();
                        let query = query.clone();
                        let semantic_source = semantic_source.clone();
                        this.load_transcript_async_then(target, cx, move |this, cx| {
                            if this.meeting_search_query.as_deref() != Some(query.as_str()) {
                                this.transcription_status =
                                    "搜索条件已变化，请重新选择要回听的结果。".into();
                                return;
                            }
                            if this.transcript_output.as_ref() == Some(&expected_base)
                                && search::source_is_current(&expected_base, &source_stamp)
                            {
                                let folded = query.to_lowercase();
                                let mut ambiguous = false;
                                let mut semantic_anchor_invalid = false;
                                let start = if let Some(start) = hit_start {
                                    let source = semantic_source
                                        .as_ref()
                                        .and_then(|source| {
                                            verified_semantic_source(&this.transcript, source)
                                        })
                                        .filter(|segment| {
                                            segment.end_ms > segment.start_ms
                                                && segment.start_ms < hit_end.unwrap_or(start)
                                                && segment.end_ms > start
                                        });
                                    semantic_anchor_invalid = source.is_none();
                                    source.map(|segment| segment.start_ms)
                                } else {
                                    let mut matches =
                                        this.transcript.segments.iter().filter(|segment| {
                                            segment.is_confirmed()
                                                && !segment.review.as_ref().is_some_and(|review| {
                                                    review.state
                                                        == opencap_asr::ReviewState::Pending
                                                })
                                                && segment.end_ms > segment.start_ms
                                                && segment.text.to_lowercase().contains(&folded)
                                        });
                                    let first = matches.next();
                                    ambiguous = matches.next().is_some();
                                    first.filter(|_| !ambiguous).map(|segment| segment.start_ms)
                                };
                                let task_running =
                                    this.asr.as_ref().is_some_and(|asr| !asr.finished());
                                this.transcript_view.update(cx, |view, cx| {
                                    view.replace(&this.transcript, true, task_running, cx);
                                    if hit_start.is_none() {
                                        view.set_query(query.clone());
                                    }
                                    cx.notify();
                                });
                                if let Some(start) = start {
                                    this.seek_player_from_segment(start, cx);
                                } else {
                                    this.transcription_status = if semantic_anchor_invalid {
                                        "语义来源与当前逐字稿不一致；请更新索引后核对。"
                                    } else if ambiguous {
                                        "多个句段包含搜索词；请在逐字稿中选择要回听的发言。"
                                    } else {
                                        "未找到可定位的完整句段；请在逐字稿中核对搜索结果。"
                                    }
                                    .into();
                                }
                            } else if !search::source_is_current(&expected_base, &source_stamp) {
                                this.transcription_status =
                                    "索引来源已变化，请刷新后再回听。".into();
                            }
                        });
                        cx.notify();
                    })),
                )
            } else {
                None
            };
            #[cfg(not(feature = "internal-player"))]
            let hit_play_action: Option<Button> = None;
            let source = media.clone();
            let retry_source = media.clone();
            let relink = meeting.relink_asset_id.clone();
            let relink_is_candidate = meeting.relink_is_candidate;
            let relink_base = meeting.base.clone();
            let folder = meeting
                .base
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();
            let external_media = media.clone();
            let maintenance = div()
                .w(px(230.))
                .flex()
                .flex_col()
                .gap_1()
                .when(has_internal_action, |d| {
                    d.child(
                        button(("play-external", i), "在系统播放器中打开", false)
                            .ghost()
                            .w_full()
                            .justify_start()
                            .disabled(!playable || self.active())
                            .on_click(move |_, _, _| {
                                if let Some(path) = &external_media {
                                    let _ = opencap_platform::open_path(path);
                                }
                            }),
                    )
                })
                .child(
                    button(("rename-meeting-files", i), "修改名称与文件", false)
                        .ghost()
                        .w_full()
                        .justify_start()
                        .disabled(!managed || !meeting.available || self.rename_job.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.begin_rename(entity_base.clone(), true, window, cx)
                        })),
                )
                .when(
                    !meeting.manifest_present && !meeting.manifest_read_error,
                    |d| {
                        d.child(
                            button(("migrate-reference", i), "迁移副本 · 引用媒体", false)
                                .ghost()
                                .w_full()
                                .justify_start()
                                .disabled(!meeting.available || self.rename_job.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.begin_legacy_migration(
                                        migrate_reference.clone(),
                                        false,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            button(("migrate-archive", i), "迁移副本 · 复制媒体", false)
                                .ghost()
                                .w_full()
                                .justify_start()
                                .disabled(
                                    !meeting.available || !playable || self.rename_job.is_some(),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.begin_legacy_migration(
                                        migrate_archive.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    },
                );
            detail_view = Some(
                card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(meeting.title()),
                            )
                            .child(ui::badge(
                                if !meeting.available { "文件缺失" } else if meeting.transcript_read_error { "转写数据未能读取" } else if meeting.summary_available { "已生成纪要" } else if meeting.transcript_available { "仅逐字稿" } else { "待转录" },
                                if meeting.summary_available { ui::SUCCESS } else { ACCENT },
                                if meeting.summary_available { 0xe6f4ef } else { SOFT },
                            )),
                    )
                    .child(label(format!("{} · {}", meeting.display_stamp, meeting.details)))
                    .child(
                        div()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(0xf8fafd))
                            .flex().flex_col().gap_2()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(if meeting.summary_available { "纪要摘要" } else { "会议内容" }))
                            .child(meeting.preview.clone()),
                    )
                    .when(hit.is_some(), |d| {
                        let hit = hit.unwrap();
                        let range = hit.start_ms.zip(hit.end_ms)
                            .filter(|(start, end)| end >= start)
                            .map(|(start, end)| format!(" · 来源句段 {}–{}（非逐词定位）", opencap_asr::timestamp(start, '.'), opencap_asr::timestamp(end, '.')))
                            .unwrap_or_default();
                        d.child(label(format!(
                            "{}命中{}{}：{}",
                            if hit.stale { "索引可能过期 · " } else { "" },
                            hit.field,
                            range,
                            hit.excerpt.replace('\n', " ")
                        )))
                    })
                    .when(self.rename_base.as_ref() == Some(&meeting.base), |d| {
                        d.child(self.rename_editor(cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .when(!meeting.available, |d| d.child(
                                primary_button(("relink-meeting-folder", i), "重新定位此会议…")
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.choose_relink_meeting(relink_folder_base.clone(), cx);
                                    })),
                            ))
                            .child(
                                primary_button(
                                    ("open-meeting", i),
                                    if exists {
                                        if self.active() { "查看逐字稿（只读）" } else { "查看逐字稿 / 纪要" }
                                    } else {
                                        "转录并查看"
                                    },
                                )
                                .icon(gpui::component::IconName::FileText)
                                .disabled(self.quit_pending || (!exists && (busy || !playable || meeting.media_is_candidate)) || (exists && busy && !self.active()))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if exists {
                                            if this.active() {
                                                let base = opencap_core::meeting_base(&selected);
                                                this.open_library_meeting_for_review(&base, cx);
                                                return;
                                            }
                                            let target = selected.clone();
                                            let field = hit_field.clone();
                                            let query = hit_query.clone();
                                            let semantic_source = hit_semantic_source.clone();
                                            let stamp = hit_source_stamp.clone();
                                            this.load_transcript_async_then(target.clone(), cx, move |this, cx| {
                                            if field.is_some() && this.meeting_search_query.as_deref() != Some(query.as_str()) {
                                                this.transcription_status = "搜索条件已变化，请重新选择结果。".into();
                                                return;
                                            }
                                            if field.is_some() && !search::source_is_current(&opencap_core::meeting_base(&target), &stamp) {
                                                this.transcription_status = "索引来源已变化，已打开当前稿件；请刷新后重新定位。".into();
                                                return;
                                            }
                                            match field.as_deref() {
                                                Some(field)
                                                    if field.ends_with("逐字稿")
                                                        && !query.is_empty() =>
                                                {
                                                    let semantic = field.starts_with("语义·");
                                                    let segment = if semantic {
                                                        semantic_source.as_ref().and_then(|source|
                                                            verified_semantic_source(&this.transcript, source).map(|segment| segment.id))
                                                    } else { None };
                                                    let task_running = this.asr.as_ref().is_some_and(|asr| !asr.finished());
                                                    this.transcript_view.update(cx, |view, cx| {
                                                        view.replace(&this.transcript, true, task_running, cx);
                                                        if let Some(segment) = segment {
                                                            view.reveal_segment(segment, cx);
                                                        } else if !semantic {
                                                            view.set_query(query.clone());
                                                        }
                                                        cx.notify();
                                                    });
                                                    if semantic && segment.is_none() {
                                                        this.transcription_status = "语义来源尚未验证或已变化；已打开逐字稿，请更新索引后定位。".into();
                                                    }
                                                    this.meeting_page = MeetingPage::Transcript;
                                                }
                                                Some(field) if field.ends_with("纪要") => {
                                                    this.meeting_page = MeetingPage::Minutes
                                                }
                                                Some(field) if field.ends_with("笔记") => {
                                                    this.meeting_page = MeetingPage::Notes
                                                }
                                                _ => this.meeting_page = MeetingPage::Transcript,
                                            }
                                            });
                                        } else if let Some(source) = &source {
                                            this.start_file_transcription_guarded(source.clone(), cx);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .when(!has_internal_action, |d| d.child(
                                button(("play-meeting", i), if meeting.media_is_candidate { "播放候选媒体" } else { "播放原音 / 视频" }, false)
                                    .icon(gpui::component::IconName::Play)
                                    .disabled(!playable || self.active())
                                    .on_click(move |_, _, _| {
                                        if let Some(path) = &media {
                                            let _ = opencap_platform::open_path(path);
                                        }
                                    }),
                            ))
                            .when_some(internal_action, |d, action| d.child(action))
                            .child(div().flex_1().min_w(px(16.)))
                            .child(
                                ui::icon_button(("rename-meeting", i), gpui::assets::IconName::Pencil, "修改显示名称")
                                    .disabled(busy || !meeting.available)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.begin_rename(rename_base.clone(), false, window, cx)
                                    })),
                            )
                            .when_some(audio_export, |d, (base, id, source)| {
                                let running = self.audio_export_job.as_ref().is_some_and(|(job_base, _, _, _)| job_base == &base);
                                let cancelling = self.audio_export_job.as_ref().is_some_and(|(job_base, _, flag, _)| job_base == &base && flag.load(std::sync::atomic::Ordering::Acquire));
                                let expected_cancel = self.audio_export_job.as_ref().filter(|(job_base, _, _, _)| job_base == &base).map(|(_, _, flag, _)| flag.clone());
                                let stage_text = match self.audio_export_stage_seen {
                                    1 => "正在核对源录屏…", 2 => "正在导出音频…", 3 => "正在复核源录屏…", 4 => "正在登记并核对音频…", _ => "正在准备音频导出…",
                                };
                                d.child(ui::icon_button(("export-recording-audio", i), gpui::assets::IconName::Download, "导出独立音频")
                                    .disabled(self.active() || self.quit_pending || self.audio_export_job.is_some())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_recording_audio_export(base.clone(), id.clone(), source.clone(), cx);
                                    })))
                                    .when(running, |d| d.child(button(("cancel-recording-audio", i), if cancelling { "正在取消…" } else { "取消音频导出" }, false)
                                        .ghost().disabled(cancelling).on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some((_, _, flag, _)) = &this.audio_export_job
                                                && expected_cancel.as_ref().is_some_and(|expected| std::sync::Arc::ptr_eq(expected, flag)) {
                                                flag.store(true, std::sync::atomic::Ordering::Release);
                                                this.message = "正在取消音频导出，等待当前读取结束…".into();
                                            }
                                            cx.notify();
                                        }))))
                                    .when(running, |d| d.child(label(stage_text)))
                            })
                            .when_some(hit_play_action, |d, action| d.child(action))
                            .when(relink.is_some(), |d| {
                                let asset_id = relink.clone().unwrap_or_default();
                                d.child(
                                    button(("relink-meeting", i), if relink_is_candidate { "重新定位候选媒体…" } else { "重新定位原媒体…" }, false)
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.choose_relink_media(
                                                relink_base.clone(),
                                                asset_id.clone(),
                                                relink_is_candidate,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .when(exists && playable && !meeting.media_is_candidate, |d| {
                                d.child(
                                    ui::icon_button(("retranscribe-meeting", i), gpui::component::IconName::RefreshCw, "用当前模型另存重转")
                                    .disabled(busy)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if let Some(source) = &retry_source {
                                                this.start_file_transcription_guarded(source.clone(), cx);
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .child(
                                ui::icon_button(("meeting-folder", i), gpui::component::IconName::Folder, "打开会议文件夹")
                                    .on_click(move |_, _, _| {
                                        let _ = opencap_platform::open_path(&folder);
                                    }),
                            )
                            .child(gpui::component::popover::Popover::new(("meeting-more-popover", i))
                                .anchor(Anchor::TopRight)
                                .trigger(ui::icon_button(("meeting-more", i), gpui::component::IconName::Ellipsis, "更多维护操作"))
                                .child(maintenance)),
                    )
                    .when(self.player_base_matches(&meeting.base), |d| {
                        #[cfg(feature = "internal-player")]
                        {
                            d.when(self.transcript_output.as_ref() != Some(&meeting.base), |d|
                                d.child(label("此会议尚未加载逐字稿，可先回放媒体。")))
                                .child(self.player_controls(cx))
                        }
                        #[cfg(not(feature = "internal-player"))]
                        {
                            d
                        }
                    }),
            );
        }
        let count = self
            .meeting_search_result
            .as_ref()
            .map(|result| result.total)
            .unwrap_or_else(|| {
                if self.meeting_search_query.is_some() {
                    0
                } else {
                    self.meetings.len()
                }
            });
        let pages = if grouped {
            1
        } else {
            count.saturating_sub(1) / MEETINGS_PER_PAGE + 1
        };
        let visible_page = if grouped { 0 } else { self.meeting_list_page };
        let list_toolbar = div()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(0xf8fafd))
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div().flex().gap_1().children(
                    ["时间序", "按月折叠", "按周折叠"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, title)| {
                            let selected = match i {
                                1 => self.config.meeting_group_by_month && !weekly,
                                2 => weekly,
                                _ => !self.config.meeting_group_by_month && !weekly,
                            };
                            button(("meeting-group-mode", i), title, selected)
                                .ghost()
                                .when(selected, |b| b.bg(rgb(SURFACE)).text_color(rgb(ACCENT)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.config.meeting_group_by_month = i == 1;
                                    this.config.meeting_group_by_week = i == 2;
                                    this.meeting_collapsed_months.clear();
                                    this.meeting_list_page = 0;
                                    this.meeting_rows_scroll.set_offset(point(px(0.), px(0.)));
                                    this.save();
                                    cx.notify();
                                }))
                        }),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(grouped, |d| {
                        d.child(
                            ui::icon_button(
                                "meeting-toggle-all",
                                gpui::component::IconName::ChevronsUpDown,
                                "全部展开 / 收起",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.meeting_collapsed_months.is_empty() {
                                    this.meeting_collapsed_months = this
                                        .meetings
                                        .iter()
                                        .map(|meeting| {
                                            meeting_group_key(
                                                meeting,
                                                this.config.meeting_group_by_week,
                                            )
                                        })
                                        .collect();
                                } else {
                                    this.meeting_collapsed_months.clear();
                                }
                                cx.notify();
                            })),
                        )
                    })
                    .child(
                        ui::icon_button(
                            "meeting-density",
                            gpui::assets::IconName::Rows3,
                            if self.config.meeting_list_compact {
                                "切换到普通列表"
                            } else {
                                "切换到紧凑列表"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.config.meeting_list_compact = !this.config.meeting_list_compact;
                            this.save();
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::icon_button(
                            "refresh-meetings",
                            gpui::component::IconName::RefreshCw,
                            "刷新会议列表",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_meetings();
                            cx.notify();
                        })),
                    ),
            );
        let mut list_panel = div()
            .h_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .overflow_hidden()
            .child(list_toolbar)
            .when(self.meeting_search_query.is_some(), |d| {
                d.child(
                    label(
                        self.meeting_search_result
                            .as_ref()
                            .map(|result| {
                                if result.semantic_model.is_some() && !result.semantic_fallback {
                                    format!("{} 场候选 · 混合相关度排序", result.total)
                                } else {
                                    format!("{} 场会议匹配 · 最近更新优先", result.total)
                                }
                            })
                            .unwrap_or_else(|| {
                                if self.meeting_search_task.is_some() {
                                    "正在搜索会议…".into()
                                } else {
                                    "搜索未完成，请重试。".into()
                                }
                            }),
                    )
                    .px_3()
                    .pt_2(),
                )
            })
            .child(
                rows_view
                    .id("meeting-rows-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.meeting_rows_scroll)
                    .track_focus(&self.meeting_list_focus)
                    .role(gpui::Role::ListBox)
                    .aria_label("会议列表")
                    .aria_description("上下键选择，回车打开，翻页请使用下方按钮")
                    .p_2()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if !this.meeting_list_focus.is_focused(window)
                            || this.meeting_page != MeetingPage::Library
                            || this.meeting_list_page != visible_page
                            || event.keystroke.modifiers.modified()
                            || visible_bases.is_empty()
                        {
                            return;
                        }
                        let current_rows: Vec<_> = if grouped {
                            visible_bases.clone()
                        } else if let Some(result) = &this.meeting_search_result {
                            result
                                .hits
                                .iter()
                                .filter_map(|hit| {
                                    this.meetings
                                        .iter()
                                        .find(|meeting| meeting.base == hit.base)
                                        .map(|meeting| meeting.base.clone())
                                })
                                .collect()
                        } else if this.meeting_search_query.is_some() {
                            Vec::new()
                        } else {
                            this.meetings
                                .iter()
                                .skip(visible_page * MEETINGS_PER_PAGE)
                                .take(MEETINGS_PER_PAGE)
                                .map(|meeting| meeting.base.clone())
                                .collect()
                        };
                        if current_rows != visible_bases {
                            return;
                        }
                        let current = this
                            .meeting_selected_base
                            .as_ref()
                            .and_then(|base| visible_bases.iter().position(|item| item == base))
                            .unwrap_or(0);
                        let (index, open) = match event.keystroke.key.as_str() {
                            "up" => (current.saturating_sub(1), false),
                            "down" => ((current + 1).min(visible_bases.len() - 1), false),
                            "home" => (0, false),
                            "end" => (visible_bases.len() - 1, false),
                            "enter" if !event.is_held => (current, true),
                            _ => return,
                        };
                        let base = visible_bases[index].clone();
                        if this.meeting_selected_base.as_ref() != Some(&base) {
                            this.select_library_meeting(base.clone());
                        }
                        this.meeting_rows_scroll
                            .scroll_to_item(visible_row_positions[index]);
                        if open {
                            this.open_library_meeting_for_review(&base, cx);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    })),
            );
        if pages > 1 {
            list_panel = list_panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        button("meeting-previous-page", "上一页", false)
                            .disabled(self.meeting_list_page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.meeting_list_page = this.meeting_list_page.saturating_sub(1);
                                this.search_meeting_page();
                                this.meeting_rows_scroll.set_offset(point(px(0.), px(0.)));
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    )
                    .child(label(format!(
                        "第 {} / {} 页",
                        self.meeting_list_page + 1,
                        pages
                    )))
                    .child(
                        button("meeting-next-page", "下一页", false)
                            .disabled(self.meeting_list_page + 1 >= pages)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.meeting_list_page =
                                    (this.meeting_list_page + 1).min(pages - 1);
                                this.search_meeting_page();
                                this.meeting_rows_scroll.set_offset(point(px(0.), px(0.)));
                                this.page_scroll.set_offset(point(px(0.), px(0.)));
                                cx.notify();
                            })),
                    ),
            );
        }
        let details = detail_view.unwrap_or_else(|| card().child(label("选择一场会议查看详情。")));
        let detail_panel = div()
            .id("meeting-detail-scroll")
            .h_full()
            .min_h(px(0.))
            .min_w(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.meeting_detail_scroll)
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(details);
        let wide = ui::two_columns(width);
        let body = if wide {
            use gpui::component::resizable::resizable_panel;
            div().flex_1().min_h(px(0.)).child(
                ui::spaced_split("meeting-library-split")
                    .child(
                        resizable_panel()
                            .size(px(width * 0.48))
                            .size_range(px(280.)..px(width - 320.))
                            .pr_2()
                            .child(list_panel),
                    )
                    .child(
                        resizable_panel()
                            .size(px(width * 0.52))
                            .size_range(px(300.)..px(width - 300.))
                            .pl_2()
                            .child(detail_panel),
                    ),
            )
        } else {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_3()
                .child(div().flex_1().min_h(px(0.)).child(list_panel))
                .child(div().flex_1().min_h(px(0.)).child(detail_panel))
        };
        div()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .child(
                div()
                    .id("meeting-library-header-scroll")
                    .max_h(px((height * 0.42).max(120.)))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(view),
            )
            .child(body)
    }

    fn player_base_matches(&self, base: &Path) -> bool {
        #[cfg(feature = "internal-player")]
        {
            self.player.is_some() && self.player_base.as_deref() == Some(base)
        }
        #[cfg(not(feature = "internal-player"))]
        {
            let _ = base;
            false
        }
    }
    fn notes_view(&self, cx: &Context<Self>) -> Div {
        let export = primary_button("export-notes-word", "导出笔记 Word…")
            .disabled(self.transcript_output.is_none())
            .on_click(cx.listener(|this, _, _, cx| {
                this.persist_notes(cx);
                if this.notes_load_failed
                    || this.notes_initial_load_pending
                    || this.notes_reload_busy
                    || this.notes_base != this.transcript_output
                    || this.notes.read(cx).value().as_str() != this.notes_saved
                {
                    this.notes_status = "笔记尚未保存或不属于当前会议，未导出。".into();
                    cx.notify();
                    return;
                }
                if let Some(base) = &this.transcript_output {
                    let md = format!(
                        "# {} · 个人笔记\n\n{}",
                        opencap_asr::escape_markdown(&this.transcript.title()),
                        opencap_asr::escape_markdown(&this.notes.read(cx).value())
                    );
                    let path = base.with_extension("notes.docx");
                    let result = opencap_core::managed_meeting_activity(base)
                        .and_then(|_activity| opencap_asr::export_minutes_docx(&md, &path));
                    match result {
                        Ok(()) => {
                            this.export_document(path, cx);
                        }
                        Err(e) => this.notes_status = format!("导出失败：{e}"),
                    }
                }
                cx.notify();
            }));
        card()
            .h_full()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(ui::section_title(
                        "个人笔记",
                        "记录自己的观察与补充，内容自动保存到本机。",
                    ))
                    .child(export)
                    .child(
                        button("save-notes-recovery", "另存笔记副本", false)
                            .ghost()
                            .disabled(
                                self.notes_base.is_none()
                                    || self.notes_recovery_busy
                                    || self.notes_reload_busy
                                    || self.notes_initial_load_pending,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.save_notes_recovery(cx))),
                    )
                    .child(
                        button("reload-notes", "重新载入笔记", false)
                            .ghost()
                            .disabled(
                                self.notes_base.is_none()
                                    || self.notes_recovery_busy
                                    || self.notes_reload_busy
                                    || self.notes_initial_load_pending,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.reload_notes(cx))),
                    )
                    .when_some(self.notes_base.clone(), |d, base| {
                        let path = base.with_extension("notes.txt");
                        let exists = path.is_file();
                        let folder = base.parent().unwrap_or(Path::new(".")).to_path_buf();
                        d.child(
                            button("open-saved-notes", "打开已保存笔记", false)
                                .ghost()
                                .disabled(!exists)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Err(error) = opencap_platform::open_path(&path) {
                                        this.notes_status =
                                            format!("无法打开已保存笔记：{error:#}");
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            button("open-notes-folder", "打开笔记文件夹", false)
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Err(error) = opencap_platform::open_path(&folder) {
                                        this.notes_status =
                                            format!("无法打开笔记文件夹：{error:#}");
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(div().flex_1().min_h(px(0.)).child(
                Textarea::new(&self.notes).h(relative(1.)).disabled(
                    self.transcript_output.is_none()
                        || self.notes_initial_load_pending
                        || self.notes_reload_busy,
                ),
            ))
            .child(label(self.notes_status.clone()))
    }
}

#[cfg(test)]
mod export_tests {
    use super::{copy_export, meeting_details};

    #[test]
    fn media_inventory_lists_real_registered_files_and_rejects_unsafe_paths() {
        use opencap_core::{AssetOwnership, MeetingAsset, MeetingManifest};
        let root =
            std::env::temp_dir().join(format!("opencap-media-inventory-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("video.mp4"), b"video").unwrap();
        std::fs::write(root.join("audio.m4a"), b"audio").unwrap();
        let asset = |id: &str, role: &str, path: &str| MeetingAsset {
            asset_id: id.into(),
            role: role.into(),
            ownership: AssetOwnership::Managed,
            path: path.into(),
            extra: Default::default(),
        };
        let manifest = MeetingManifest {
            schema_version: 1,
            meeting_id: "test".into(),
            title: "meeting".into(),
            managed_directory: root.clone(),
            transcription_runs: Vec::new(),
            extra: Default::default(),
            assets: vec![
                asset("v", "recording_video", "video.mp4"),
                asset("a", "recording_audio", "audio.m4a"),
                asset("missing", "candidate_media", "missing.m4a"),
                asset("unsafe", "recording_audio", "../outside.m4a"),
            ],
        };
        let items = super::list_meeting_media(&root.join("meeting"), Some(&manifest), None);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().filter(|item| item.available).count(), 2);
        assert!(
            items
                .iter()
                .any(|item| item.role == "candidate_media" && !item.available)
        );
        let legacy =
            super::list_meeting_media(&root.join("video"), None, Some(&root.join("audio.m4a")));
        assert_eq!(legacy.len(), 2);
        assert!(
            legacy
                .iter()
                .any(|item| item.role == "same_name_media" && item.path == root.join("video.mp4"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn weekly_groups_use_iso_week_year_across_new_year() {
        let before = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_609_502_400);
        let after = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_609_761_600);
        assert_eq!(super::week_key_for_time(before), "2020-W53");
        assert_eq!(super::week_key_for_time(after), "2021-W01");
        assert_eq!(
            super::meeting_group_label("2020-W53", true),
            "2020 年第 53 周"
        );
    }
    #[test]
    fn one_meeting_with_media_and_prefix_aliases_is_listed_once_without_moving_files() {
        let root =
            std::env::temp_dir().join(format!("opencap-library-dedup-{}", std::process::id()));
        std::fs::create_dir_all(root.join("一场会议")).unwrap();
        let base = root.join("一场会议/会议");
        let media = base.with_extension("m4a");
        std::fs::write(&media, b"preserved recording").unwrap();
        std::fs::write(base.with_extension("recording.json"), b"{}").unwrap();
        let t = opencap_asr::Transcript {
            source: Some(media.clone()),
            complete: true,
            ..Default::default()
        };
        t.checkpoint(&base).unwrap();
        let alias = root.join("一场会议/../一场会议/会议");
        let files = super::discover_paths(&root, vec![base.clone(), media.clone(), alias]);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].media.as_ref().unwrap().canonicalize().unwrap(),
            media.canonicalize().unwrap()
        );
        assert_eq!(files[0].complete, Some(true));
        assert_eq!(std::fs::read(&media).unwrap(), b"preserved recording");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn different_model_transcripts_of_the_same_recording_remain_separate() {
        let root = std::env::temp_dir().join(format!(
            "opencap-library-model-comparison-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let media = root.join("source.m4a");
        std::fs::write(&media, b"original recording").unwrap();
        let first = root.join("first");
        let second = root.join("second");
        for (base, kind) in [
            (&first, opencap_asr::ModelKind::Offline),
            (&second, opencap_asr::ModelKind::ParaformerSmall),
        ] {
            opencap_asr::Transcript {
                source: Some(media.clone()),
                model: kind.directory().into(),
                complete: true,
                ..Default::default()
            }
            .checkpoint(base)
            .unwrap();
        }
        let meetings = super::discover_paths(&root, vec![]);
        assert_eq!(meetings.len(), 2);
        assert!(meetings.iter().any(|m| m.status().contains("SenseVoice")));
        assert!(meetings.iter().any(|m| m.status().contains("Paraformer")));
        assert!(meetings.iter().all(|m| m.media.as_ref() == Some(&media)));
        assert_eq!(std::fs::read(media).unwrap(), b"original recording");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn meeting_preview_uses_saved_body_duration_and_actual_media_size() {
        let dir =
            std::env::temp_dir().join(format!("opencap-library-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("generic");
        let media = base.with_extension("m4a");
        std::fs::write(&media, vec![0; 2_000_000]).unwrap();
        let transcript = opencap_asr::Transcript {
            source_duration_ms: Some(1_200_000),
            segments: vec![opencap_asr::Segment {
                extra: Default::default(),
                id: 0,
                revision: 1,
                start_ms: 0,
                end_ms: 10000,
                text: "客户反馈与下周安排。".into(),
                finalized: true,
                correcting: false,
                review: None,
            }],
            ..Default::default()
        };
        let (_, preview, details) =
            meeting_details(&base, Some(&media), Some(&transcript), None, true);
        assert!(preview.contains("客户反馈"));
        assert!(details.contains("20 分"));
        assert!(details.contains("2.0 MB"));
        opencap_asr::rename_meeting(&base, "产品评审").unwrap();
        std::fs::write(
            base.with_extension("summary.md"),
            "# 新产品主题\n\n会议议定分批发布。",
        )
        .unwrap();
        let (title, preview, _) =
            meeting_details(&base, Some(&media), Some(&transcript), None, true);
        assert_eq!(title, "产品评审");
        assert!(preview.contains("分批发布"));
        assert!(!preview.contains("客户反馈"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn exporting_keeps_the_local_document_and_refuses_to_replace_original_media() {
        let dir = std::env::temp_dir().join(format!(
            "opencap-export-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("original.m4a");
        let document = dir.join("transcript.docx");
        let copy = dir.join("shared-copy.docx");
        std::fs::write(&original, b"unchanged audio").unwrap();
        std::fs::write(&document, b"document bytes").unwrap();
        copy_export(&document, &copy, Some(&original)).unwrap();
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&document).unwrap()
        );
        assert!(copy_export(&document, &original, Some(&original)).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"unchanged audio");
        assert_eq!(std::fs::read(&document).unwrap(), b"document bytes");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
