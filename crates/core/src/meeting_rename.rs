//! Journalled moves for meetings whose ownership is already recorded.
//!
//! A rename is staged on the same volume and can be resumed after a crash.
//! This module never infers that an external source is managed. Callers must
//! first quiesce all writers and close playback handles for the meeting.
use crate::{
    AssetOwnership, MeetingAsset, MeetingManifest, meeting_base, meeting_manifest, new_stable_id,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

const JOURNAL_PREFIX: &str = ".opencap-rename-";
const OWNER_MARKER: &str = ".opencap-rename-owner";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileMove {
    from: PathBuf,
    temporary: PathBuf,
    to: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathMap {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Planned,
    Moving,
    Updating,
    Placing,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    operation_id: String,
    old_base: PathBuf,
    new_base: PathBuf,
    stage: PathBuf,
    title: String,
    /// Captured before any staged rewrite so crash recovery can repeat it.
    #[serde(default)]
    prior_document_title: Option<String>,
    /// Original bytes, allowing recovery to detect changes after planning.
    #[serde(default)]
    original_manifest_fingerprint: Option<crate::MediaFingerprint>,
    manifest: MeetingManifest,
    paths: Vec<PathMap>,
    files: Vec<FileMove>,
    phase: Phase,
    index: usize,
    temporary_step: bool,
}

/// An index must replace `old_base` with `new_base` before the journal is
/// acknowledged. Keeping the journal until then makes this restart-safe.
#[derive(Debug, Clone)]
pub struct RenamedMeeting {
    pub operation_id: String,
    pub old_base: PathBuf,
    pub new_base: PathBuf,
    pub meeting_id: String,
    pub document_titles_to_review: Vec<String>,
    moved_paths: Vec<(PathBuf, PathBuf)>,
}
impl RenamedMeeting {
    /// External paths are returned unchanged; only explicitly managed paths
    /// from this operation are translated.
    pub fn relocate_path(&self, path: &Path) -> PathBuf {
        self.moved_paths
            .iter()
            .find(|(old, _)| old == path)
            .map(|(_, new)| new.clone())
            .unwrap_or_else(|| path.to_path_buf())
    }
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn journal_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{JOURNAL_PREFIX}{id}.json"))
}

pub fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
            // Some filesystems do not implement directory fsync; the file
            // itself was still flushed before publication.
            if !matches!(
                error.raw_os_error(),
                Some(libc::EINVAL) | Some(libc::ENOTSUP)
            ) {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

pub(crate) fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("文件没有父目录")?;
    let temporary = parent.join(format!(".opencap-write-{}.tmp", new_stable_id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_parent_directory(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Publish a new immutable file without exposing partial bytes or replacing
/// any existing destination. The temporary file stays on the same volume.
pub fn atomic_create_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("文件没有父目录")?;
    let temporary = parent.join(format!(".opencap-write-{}.tmp", new_stable_id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rename_no_replace(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec_pretty(journal)?).context("无法持久化重命名日志")
}

/// Atomically reject an existing destination, including an empty directory.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let from_c = CString::new(from.as_os_str().as_bytes())?;
    let to_c = CString::new(to.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("无法移动 {} 到 {}", from.display(), to.display()));
    }
    sync_parent_directory(from)?;
    if from.parent() != to.parent() {
        sync_parent_directory(to)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // No MOVEFILE_REPLACE_EXISTING and no cross-volume copy fallback.
    if unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), 0) } == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("无法移动 {} 到 {}", from.display(), to.display()));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn rename_no_replace(_from: &Path, _to: &Path) -> Result<()> {
    bail!("当前平台不支持安全的无覆盖重命名")
}

pub(crate) fn acquire_root(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".opencap-rename.lock"))?;
    file.try_lock()
        .map_err(|error| anyhow::anyhow!("另一个重命名或恢复操作正在使用此目录：{error}"))?;
    Ok(file)
}

/// Keep a directory scan coherent with respect to a concurrent rename.
/// This does not prevent recording or analysis in unrelated meetings.
pub fn meeting_scan_activity(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".opencap-rename.lock"))?;
    file.try_lock_shared()
        .map_err(|error| anyhow::anyhow!("会议目录正在移动，请稍后刷新：{error}"))?;
    Ok(file)
}

fn acquire_meeting(root: &Path, id: &str, exclusive: bool) -> Result<File> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "会议身份无效，无法锁定");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!(".opencap-meeting-{id}.lock")))?;
    if exclusive {
        file.try_lock().map_err(|error| {
            anyhow::anyhow!("这场会议仍有录制、识别或写入任务，请稍后重试：{error}")
        })?;
    } else {
        file.try_lock_shared()
            .map_err(|error| anyhow::anyhow!("这场会议正在移动，暂时不能写入：{error}"))?;
    }
    Ok(file)
}

/// Hold the returned lease for the entire lifetime of any task that can write
/// this meeting. Other meetings remain independent. Legacy flat files have no
/// managed identity and return `None`; they are not moved by this rename path.
pub fn managed_meeting_activity(base: &Path) -> Result<Option<File>> {
    let base = meeting_base(base);
    let Some(folder) = base.parent() else {
        return Ok(None);
    };
    if folder.file_name() != base.file_name() {
        return Ok(None);
    }
    ensure!(folder.is_dir(), "会议目录不可访问或已移动；请刷新会议列表");
    let Some(manifest) = meeting_manifest(&base)? else {
        return Ok(None);
    };
    let root = folder.parent().context("会议根目录无效")?;
    let lease = acquire_meeting(root, &manifest.meeting_id, false)?;
    let current = meeting_manifest(&base)?.context("会议在任务启动时已移动；请刷新会议列表")?;
    ensure!(
        current.meeting_id == manifest.meeting_id,
        "会议身份已变化；请刷新会议列表"
    );
    Ok(Some(lease))
}

pub(crate) fn manifest_write_guard(base: &Path) -> Result<Option<File>> {
    let base = meeting_base(base);
    let Some(manifest) = meeting_manifest(&base)? else {
        return Ok(None);
    };
    ensure!(
        uuid::Uuid::parse_str(&manifest.meeting_id).is_ok(),
        "会议身份无效"
    );
    let root = base
        .parent()
        .and_then(Path::parent)
        .context("会议根目录无效")?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!(".opencap-manifest-{}.lock", manifest.meeting_id)))?;
    file.lock().context("无法锁定会议资产清单")?;
    Ok(Some(file))
}

/// Serialize read-modify-write operations on the small meeting metadata file,
/// including legacy flat meetings which do not yet have a UUID manifest.
pub fn meeting_metadata_guard(base: &Path) -> Result<File> {
    use sha2::{Digest, Sha256};
    let base = meeting_base(base);
    let folder = base.parent().context("会议目录无效")?;
    let manifest = meeting_manifest(&base)?;
    let (root, key) = if let Some(manifest) = manifest {
        ensure!(
            uuid::Uuid::parse_str(&manifest.meeting_id).is_ok(),
            "会议身份无效"
        );
        (
            folder.parent().context("会议根目录无效")?,
            manifest.meeting_id,
        )
    } else {
        (
            folder,
            format!("{:x}", Sha256::digest(base.to_string_lossy().as_bytes())),
        )
    };
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!(".opencap-metadata-{key}.lock")))?;
    file.lock().context("无法锁定会议元数据")?;
    Ok(file)
}

pub(crate) fn known_suffix(suffix: &str) -> bool {
    if [
        "recording.json",
        "transcript.json",
        "transcript.txt",
        "transcript.md",
        "transcript.docx",
        "srt",
        "summary.md",
        "summary.docx",
        "summary.history.json",
        "summary.request.json",
        "summary.draft.md",
        "summary.live.json",
        "speakers.online.json",
        "notes.txt",
        "notes.docx",
        "meeting.json",
    ]
    .contains(&suffix)
    {
        return true;
    }
    if let Some(rest) = suffix.strip_prefix("summary.v") {
        return rest.split_once('.').is_some_and(|(number, ext)| {
            !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
                && matches!(ext, "md" | "docx")
        });
    }
    if let Some(id) = suffix
        .strip_prefix("notes.recovery-")
        .and_then(|s| s.strip_suffix(".txt"))
    {
        return uuid::Uuid::parse_str(id).is_ok();
    }
    if let Some(id) = suffix
        .strip_prefix("summary.word-backup-")
        .and_then(|s| s.strip_suffix(".docx"))
    {
        return uuid::Uuid::parse_str(id).is_ok();
    }
    if let Some(rest) = suffix.strip_prefix("summary.live.export-") {
        return [".md", ".docx"].into_iter().any(|extension| {
            rest.strip_suffix(extension)
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        });
    }
    if let Some(id) = suffix
        .strip_prefix("transcript.run-recovery-")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return uuid::Uuid::parse_str(id).is_ok();
    }
    if let Some(hash) = suffix
        .strip_prefix("transcript.run-backfill-")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    }
    if let Some(hash) = suffix
        .strip_prefix("summary.live.archive-sha256-")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    }
    if let Some(hash) = suffix
        .strip_prefix("summary.live.v-sha256-")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    }
    [
        "transcript.run-",
        "summary.live.v-",
        "summary.live.archive-",
        "summary.recovery-",
        "speakers.online.archive-",
    ]
    .into_iter()
    .any(|prefix| {
        suffix
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(".json"))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    })
}

/// The canonical manifest role for sidecars recovered from older layouts.
pub(crate) fn recovered_asset_role(suffix: &str) -> &str {
    if suffix.starts_with("transcript.run-") {
        "transcript_history"
    } else if suffix.starts_with("summary.live.archive-") {
        "live_minutes_archive"
    } else if suffix.starts_with("summary.live.export-") {
        "live_minutes_export"
    } else if suffix == "summary.live.json" || suffix.starts_with("summary.live.v-") {
        "live_minutes"
    } else if suffix.starts_with("summary.recovery-") {
        "minutes_recovery"
    } else if suffix.starts_with("summary.word-backup-") {
        "minutes_word_backup"
    } else if suffix.starts_with("speakers.online.archive-") {
        "online_speakers_history"
    } else if suffix == "speakers.online.json" {
        "online_speakers"
    } else if suffix.starts_with("notes.recovery-") {
        "notes_recovery"
    } else {
        suffix
    }
}

fn ensure_plain_parents(root: &Path, rel: &Path) -> Result<()> {
    ensure!(relative(rel), "资产路径无效：{}", rel.display());
    let mut current = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            current.push(component);
            let metadata = fs::symlink_metadata(&current)
                .with_context(|| format!("资产目录不可访问：{}", current.display()))?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "资产目录不能是符号链接：{}",
                current.display()
            );
        }
    }
    Ok(())
}

fn updated_name(path: &Path, old: &str, new: &str) -> PathBuf {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return path.to_path_buf();
    };
    let prefix = format!("{old}.");
    match name.strip_prefix(&prefix) {
        Some(suffix) => path.with_file_name(format!("{new}.{suffix}")),
        None => path.to_path_buf(),
    }
}

fn validate_json(path: &Path, object: bool) -> Result<()> {
    if exists(path) {
        ensure_plain_file(path)?;
        let (bytes, _, _) = read_title_source(path, 64 * 1024 * 1024)?
            .context("会议 JSON 超过读取上限，未开始改名")?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} 格式错误，未开始改名", path.display()))?;
        if object {
            ensure!(value.is_object(), "{} 不是 JSON 对象", path.display());
        }
    }
    Ok(())
}

fn ensure_plain_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "受管理资产不是普通文件：{}",
        path.display()
    );
    Ok(())
}

fn plan(base: &Path, title: &str) -> Result<Journal> {
    let title = title.trim();
    ensure!(
        !title.is_empty()
            && title.chars().count() <= 100
            && !title
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}')),
        "名称须为 1–100 个非控制字符"
    );
    let old_base = meeting_base(base);
    let old_dir = old_base.parent().context("会议目录无效")?;
    let root = old_dir.parent().context("会议根目录无效")?;
    let old_name = old_base
        .file_name()
        .and_then(|v| v.to_str())
        .context("会议名称不是有效 UTF-8")?;
    ensure!(
        old_dir.file_name() == old_base.file_name(),
        "旧记录不在独立受管理目录；请先迁移"
    );
    let directory_meta = fs::symlink_metadata(old_dir)?;
    ensure!(
        directory_meta.is_dir() && !directory_meta.file_type().is_symlink(),
        "会议目录不能是符号链接"
    );
    let manifest_path = old_base.with_extension("manifest.json");
    ensure!(exists(&manifest_path), "旧记录尚无资产清单；请先迁移");
    let (manifest_bytes, _, manifest_fingerprint) =
        read_title_source(&manifest_path, 64 * 1024 * 1024)?.context("会议清单超过读取上限")?;
    let mut manifest: MeetingManifest =
        serde_json::from_slice(&manifest_bytes).context("会议资产清单无法读取")?;
    ensure!(
        manifest.managed_directory == Path::new(old_name),
        "会议清单与目录不一致，未开始改名"
    );
    ensure!(
        !old_base.with_extension("partial.mp4").exists()
            && !old_base.with_extension("partial.m4a").exists(),
        "会议存在未完成的录制，请先恢复或完成录制"
    );
    let canonical_old_dir = old_dir
        .canonicalize()
        .context("无法确认会议目录的实际位置")?;
    for asset in &manifest.assets {
        match asset.ownership {
            AssetOwnership::Managed => ensure_plain_parents(old_dir, &asset.path)?,
            AssetOwnership::External => {
                ensure!(asset.path.is_absolute(), "外部资产路径无效");
                // A lexical comparison is unsafe when either path contains a
                // symlink or `..`. Refuse to move until the external asset can
                // be resolved, rather than accidentally moving its original.
                let resolved = asset.path.canonicalize().with_context(|| {
                    format!(
                        "无法确认外部资产位置，请先恢复或重定位：{}",
                        asset.path.display()
                    )
                })?;
                ensure!(
                    !resolved.starts_with(&canonical_old_dir),
                    "外部资产位于会议目录内；为保护原件不能移动该目录"
                );
            }
        }
    }
    for extension in ["meeting.json", "transcript.json", "recording.json"] {
        validate_json(&old_base.with_extension(extension), true)?;
    }
    let transcript_path = old_base.with_extension("transcript.json");
    let prior_document_title = if exists(&transcript_path) {
        let (bytes, _, _) = read_title_source(&transcript_path, 64 * 1024 * 1024)?
            .context("逐字稿超过读取上限，未开始改名")?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        Some(
            value
                .get("document_title")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    value
                        .get("source")
                        .and_then(|value| value.as_str())
                        .and_then(|source| Path::new(source).file_stem())
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "会议录音".into()),
        )
    } else {
        None
    };
    let new_name = crate::meeting::folder_name(title);
    let new_dir = root.join(&new_name);
    if exists(&new_dir) {
        ensure!(
            same_file::is_same_file(old_dir, &new_dir)?,
            "目标会议目录已存在：{}",
            new_dir.display()
        );
    }
    let new_base = new_dir.join(&new_name);
    let id = new_stable_id();
    let stage = root.join(format!("{JOURNAL_PREFIX}{id}.dir"));
    ensure!(
        !exists(&stage) && !exists(&journal_path(root, &id)),
        "临时重命名位置已存在"
    );
    let mut managed: BTreeSet<PathBuf> = manifest
        .assets
        .iter()
        .filter(|asset| asset.ownership == AssetOwnership::Managed)
        .map(|asset| asset.path.clone())
        .collect();
    // Older recovery scans used the literal suffix as the role. Preserve the
    // stable asset ID when assigning known sidecars their canonical roles.
    // Do not relabel unrelated or externally owned assets.
    for asset in &mut manifest.assets {
        if asset.ownership == AssetOwnership::Managed
            && let Some(name) = asset.path.file_name().and_then(|name| name.to_str())
            && let Some(suffix) = name.strip_prefix(&format!("{old_name}."))
            && known_suffix(suffix)
            && asset.role == suffix
        {
            asset.role = recovered_asset_role(suffix).into();
        }
    }
    // Recover well-defined application sidecars even when an older version did
    // not register them. Never search outside this meeting's own directory.
    for entry in fs::read_dir(old_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(suffix) = name.strip_prefix(&format!("{old_name}."))
            && known_suffix(suffix)
        {
            let path = PathBuf::from(name);
            if managed.insert(path.clone()) {
                manifest.assets.push(MeetingAsset {
                    asset_id: new_stable_id(),
                    role: recovered_asset_role(suffix).into(),
                    ownership: AssetOwnership::Managed,
                    path,
                    extra: BTreeMap::new(),
                });
            }
        }
    }
    let meeting_path = PathBuf::from(format!("{old_name}.meeting.json"));
    if managed.insert(meeting_path.clone()) {
        manifest.assets.push(MeetingAsset {
            asset_id: new_stable_id(),
            role: "meeting.json".into(),
            ownership: AssetOwnership::Managed,
            path: meeting_path,
            extra: BTreeMap::new(),
        });
    }
    managed.insert(PathBuf::from(format!("{old_name}.manifest.json")));
    let paths: Vec<_> = managed
        .iter()
        .map(|from| PathMap {
            from: from.clone(),
            to: updated_name(from, old_name, &new_name),
        })
        .collect();
    let mut files = Vec::new();
    for from in &managed {
        let to = updated_name(from, old_name, &new_name);
        let source = old_dir.join(from);
        let target = old_dir.join(&to);
        if !exists(&source) {
            if *from != to {
                ensure!(!exists(&target), "目标文件已存在：{}", target.display());
            }
            continue;
        }
        ensure_plain_parents(old_dir, from)?;
        ensure_plain_file(&source)?;
        if *from == to {
            continue;
        }
        if exists(&target) {
            ensure!(
                same_file::is_same_file(&source, &target)?,
                "目标文件已存在：{}",
                target.display()
            );
        }
        let temporary = from.with_file_name(format!(".opencap-move-{id}-{}", files.len()));
        ensure!(!exists(&old_dir.join(&temporary)), "临时资产路径已存在");
        files.push(FileMove {
            from: from.clone(),
            temporary,
            to,
        });
    }
    for asset in &mut manifest.assets {
        if asset.ownership == AssetOwnership::Managed {
            asset.path = updated_name(&asset.path, old_name, &new_name);
        }
    }
    manifest.title = title.to_owned();
    manifest.managed_directory = PathBuf::from(&new_name);
    let generation = manifest
        .extra
        .get("rename_generation")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    manifest
        .extra
        .insert("rename_generation".into(), generation.into());
    Ok(Journal {
        version: 1,
        operation_id: id,
        old_base,
        new_base,
        stage,
        title: title.to_owned(),
        prior_document_title,
        original_manifest_fingerprint: Some(manifest_fingerprint),
        manifest,
        paths,
        files,
        phase: Phase::Planned,
        index: 0,
        temporary_step: false,
    })
}

fn validate_journal(root: &Path, journal: &Journal) -> Result<()> {
    ensure!(
        journal.version == 1 && uuid::Uuid::parse_str(&journal.operation_id).is_ok(),
        "重命名日志版本或编号无效"
    );
    let old_dir = journal.old_base.parent().context("旧路径无效")?;
    let new_dir = journal.new_base.parent().context("新路径无效")?;
    ensure!(
        old_dir.parent() == Some(root) && new_dir.parent() == Some(root),
        "重命名日志超出会议根目录"
    );
    ensure!(
        old_dir.file_name() == journal.old_base.file_name()
            && new_dir.file_name() == journal.new_base.file_name(),
        "重命名日志路径不匹配"
    );
    ensure!(
        journal.stage == root.join(format!("{JOURNAL_PREFIX}{}.dir", journal.operation_id)),
        "重命名暂存目录无效"
    );
    ensure!(journal.index <= journal.files.len(), "重命名日志进度无效");
    let old_name = journal
        .old_base
        .file_name()
        .and_then(|v| v.to_str())
        .context("旧会议名称无效")?;
    let new_name = journal
        .new_base
        .file_name()
        .and_then(|v| v.to_str())
        .context("新会议名称无效")?;
    ensure!(
        journal.manifest.managed_directory == Path::new(new_name)
            && !journal.manifest.meeting_id.is_empty(),
        "重命名清单无效"
    );
    for path in &journal.paths {
        ensure!(
            relative(&path.from)
                && relative(&path.to)
                && path.to == updated_name(&path.from, old_name, new_name),
            "重命名映射无效"
        );
        ensure!(
            path.to == Path::new(&format!("{new_name}.manifest.json"))
                || journal.manifest.assets.iter().any(|asset| asset.ownership
                    == AssetOwnership::Managed
                    && asset.path == path.to),
            "重命名映射未登记为受管理资产"
        );
    }
    for (index, file) in journal.files.iter().enumerate() {
        ensure!(
            relative(&file.from) && relative(&file.to) && relative(&file.temporary),
            "重命名文件路径无效"
        );
        ensure!(
            journal
                .paths
                .iter()
                .any(|path| path.from == file.from && path.to == file.to),
            "重命名文件不在清单映射中"
        );
        ensure!(
            file.temporary
                == file
                    .from
                    .with_file_name(format!(".opencap-move-{}-{index}", journal.operation_id)),
            "重命名临时路径无效"
        );
    }
    Ok(())
}

fn verify_identity(journal: &Journal) -> Result<()> {
    let old_name = journal.old_base.file_name().unwrap();
    let new_name = journal.new_base.file_name().unwrap();
    let mut candidates = vec![
        journal.old_base.with_extension("manifest.json"),
        journal.new_base.with_extension("manifest.json"),
    ];
    candidates.push(journal.stage.join(old_name).with_extension("manifest.json"));
    candidates.push(journal.stage.join(new_name).with_extension("manifest.json"));
    let old_manifest = PathBuf::from(format!("{}.manifest.json", old_name.to_string_lossy()));
    if let Some(item) = journal.files.iter().find(|file| file.from == old_manifest) {
        candidates.push(journal.stage.join(&item.temporary));
    }
    for path in candidates {
        if !exists(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "清单不是普通文件：{}",
            path.display()
        );
        let (bytes, _, _) =
            read_title_source(&path, 64 * 1024 * 1024)?.context("会议清单超过读取上限")?;
        let manifest: MeetingManifest =
            serde_json::from_slice(&bytes).context("重命名期间的会议清单无法读取")?;
        ensure!(
            manifest.meeting_id == journal.manifest.meeting_id,
            "重命名会议身份不匹配"
        );
        return Ok(());
    }
    bail!("无法确认待恢复会议的身份，保留日志供检查")
}

fn rewrite_paths(value: &mut serde_json::Value, pointers: &[&str], journal: &Journal) {
    let old_dir = journal.old_base.parent().unwrap();
    let new_dir = journal.new_base.parent().unwrap();
    for pointer in pointers {
        let Some(slot) = value.pointer_mut(pointer) else {
            continue;
        };
        let Some(path) = slot.as_str().map(PathBuf::from) else {
            continue;
        };
        for item in &journal.paths {
            if path == old_dir.join(&item.from) {
                *slot =
                    serde_json::to_value(new_dir.join(&item.to)).unwrap_or(serde_json::Value::Null);
                break;
            }
        }
    }
}

fn markdown_title(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if "\\`*_{}[]<>#+-.!|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Read and later recheck the exact file used for a proposed title update.
pub fn read_title_source(
    path: &Path,
    limit: u64,
) -> Result<Option<(Vec<u8>, same_file::Handle, crate::MediaFingerprint)>> {
    use sha2::{Digest, Sha256};
    ensure_plain_file(path)?;
    let mut file = crate::open_regular_file_for_read(path)?;
    let before = file.metadata()?;
    if before.len() > limit {
        return Ok(None);
    }
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    let after = file.metadata()?;
    ensure_plain_file(path)?;
    ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && same_file::Handle::from_file(crate::open_regular_file_for_read(path)?)? == identity,
        "读取标题期间文件发生变化，未覆盖"
    );
    let fingerprint = crate::MediaFingerprint {
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    Ok(Some((bytes, identity, fingerprint)))
}

pub fn verify_title_source(
    path: &Path,
    identity: &same_file::Handle,
    fingerprint: &crate::MediaFingerprint,
) -> Result<()> {
    ensure_plain_file(path)?;
    ensure!(
        &same_file::Handle::from_file(crate::open_regular_file_for_read(path)?)? == identity
            && &crate::fingerprint_media(path, None)? == fingerprint
            && &same_file::Handle::from_file(crate::open_regular_file_for_read(path)?)? == identity
            && fs::symlink_metadata(path)?.file_type().is_file(),
        "更新标题前文件发生变化，未覆盖"
    );
    Ok(())
}

/// Replace only a recognizable first line. The rest may contain user edits.
fn rewrite_text_title(path: &Path, old: Option<&str>, new: &str, markdown: bool) -> Result<bool> {
    if !exists(path) {
        return Ok(true);
    }
    let Some((bytes, identity, fingerprint)) = read_title_source(path, 64_000_000)? else {
        return Ok(false);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(false);
    };
    let new_prefix = if markdown {
        format!("# {} · 逐字稿\n\n", markdown_title(new))
    } else {
        format!("{new} · 逐字稿\n\n")
    };
    if text.starts_with(&new_prefix) {
        return Ok(true);
    }
    let old_prefix = old.map(|title| {
        if markdown {
            format!("# {} · 逐字稿\n\n", markdown_title(title))
        } else {
            format!("{title} · 逐字稿\n\n")
        }
    });
    let rest = old_prefix
        .as_ref()
        .and_then(|prefix| text.strip_prefix(prefix))
        .or_else(|| markdown.then(|| text.strip_prefix("# 转录\n\n")).flatten());
    let Some(rest) = rest else {
        return Ok(false);
    };
    verify_title_source(path, &identity, &fingerprint)?;
    atomic_bytes(path, format!("{new_prefix}{rest}").as_bytes())?;
    Ok(true)
}

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Update only the original generated single-run title paragraph. Copy all
/// other package entries as compressed data and refuse signed or unfamiliar
/// packages. A user-edited title is preserved for manual review.
fn rewrite_word_title(path: &Path, old: Option<&str>, new: &str) -> Result<bool> {
    if !exists(path) {
        return Ok(true);
    }
    let Some((bytes, identity, fingerprint)) = read_title_source(path, 128_000_000)? else {
        return Ok(false);
    };
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(_) => return Ok(false),
    };
    if archive.len() > 2_000 {
        return Ok(false);
    }
    let mut documents = 0;
    for index in 0..archive.len() {
        let file = match archive.by_index(index) {
            Ok(file) => file,
            Err(_) => return Ok(false),
        };
        if file
            .name()
            .to_ascii_lowercase()
            .starts_with("_xmlsignatures/")
            || !file.comment().is_empty()
            || file.extra_data_fields().next().is_some()
        {
            return Ok(false);
        }
        if file.name() == "word/document.xml" {
            documents += 1;
        }
    }
    if documents != 1 {
        return Ok(false);
    }
    let mut xml = String::new();
    {
        let mut document = match archive.by_name("word/document.xml") {
            Ok(document) => document,
            Err(_) => return Ok(false),
        };
        if document.size() > 64_000_000
            || (&mut document)
                .take(64_000_001)
                .read_to_string(&mut xml)
                .is_err()
            || xml.len() > 64_000_000
        {
            return Ok(false);
        }
    }
    let Some(body) = xml.find("<w:body>") else {
        return Ok(false);
    };
    let Some(start) = xml[body..]
        .find("<w:p ")
        .or_else(|| xml[body..].find("<w:p>"))
        .map(|at| body + at)
    else {
        return Ok(false);
    };
    let Some(end) = xml[start..]
        .find("</w:p>")
        .map(|at| start + at + "</w:p>".len())
    else {
        return Ok(false);
    };
    let paragraph = &xml[start..end];
    if !paragraph.contains("<w:pStyle w:val=\"Title\"") {
        return Ok(false);
    }
    let open = "<w:t xml:space=\"preserve\">";
    let Some(text_start) = paragraph.find(open).map(|at| at + open.len()) else {
        return Ok(false);
    };
    let Some(text_end) = paragraph[text_start..]
        .find("</w:t>")
        .map(|at| text_start + at)
    else {
        return Ok(false);
    };
    if paragraph.matches("<w:t ").count() + paragraph.matches("<w:t>").count() != 1 {
        return Ok(false);
    }
    let current = &paragraph[text_start..text_end];
    let next = xml_text(&format!("{new} · 逐字稿"));
    if current == next {
        return Ok(true);
    }
    let Some(old) = old else {
        return Ok(false);
    };
    if current != xml_text(&format!("{old} · 逐字稿")) {
        return Ok(false);
    }
    let xml = format!(
        "{}{}{}",
        &xml[..start + text_start],
        next,
        &xml[start + text_end..]
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer.set_raw_comment(archive.comment().to_vec().into_boxed_slice());
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        if file.name() == "word/document.xml" {
            let options = file.options();
            drop(file);
            writer.start_file("word/document.xml", options)?;
            writer.write_all(xml.as_bytes())?;
        } else {
            writer.raw_copy_file(file)?;
        }
    }
    let output = writer.finish()?.into_inner();
    verify_title_source(path, &identity, &fingerprint)?;
    atomic_bytes(path, &output)?;
    Ok(true)
}

fn update_metadata(journal: &Journal) -> Result<()> {
    let name = journal.new_base.file_name().context("会议名称无效")?;
    let staged_base = journal.stage.join(name);
    // An unregistered symlink with a known suffix must never be followed when
    // the old and new sanitized file names happen to be identical.
    for extension in [
        "meeting.json",
        "transcript.json",
        "recording.json",
        "transcript.txt",
        "transcript.md",
        "transcript.docx",
        "manifest.json",
    ] {
        let path = staged_base.with_extension(extension);
        if exists(&path) {
            ensure_plain_file(&path)?;
        }
    }
    if let Some(original) = &journal.original_manifest_fingerprint {
        let manifest_path = staged_base.with_extension("manifest.json");
        let (bytes, _, fingerprint) =
            read_title_source(&manifest_path, 64 * 1024 * 1024)?.context("会议清单超过读取上限")?;
        let mut current: MeetingManifest = serde_json::from_slice(&bytes)?;
        let mut target = journal.manifest.clone();
        // Title-review notes are determined while rewriting documents and may
        // already exist if the previous attempt stopped after writing them.
        current.extra.remove("document_titles_to_review");
        target.extra.remove("document_titles_to_review");
        ensure!(
            &fingerprint == original
                || serde_json::to_value(&current)? == serde_json::to_value(&target)?,
            "会议清单在重命名计划后发生变化，未开始更新元数据"
        );
    }
    let meeting = staged_base.with_extension("meeting.json");
    let (mut value, meeting_source) = if exists(&meeting) {
        let (bytes, identity, fingerprint) =
            read_title_source(&meeting, 64 * 1024 * 1024)?.context("会议名称文件超过读取上限")?;
        (
            serde_json::from_slice::<serde_json::Value>(&bytes)?,
            Some((identity, fingerprint)),
        )
    } else {
        (serde_json::json!({}), None)
    };
    ensure!(value.is_object(), "会议名称文件不是 JSON 对象");
    value["title"] = journal.title.clone().into();
    let meeting_bytes = serde_json::to_vec_pretty(&value)?;
    if let Some((identity, fingerprint)) = &meeting_source {
        verify_title_source(&meeting, identity, fingerprint)?;
        atomic_bytes(&meeting, &meeting_bytes)?;
    } else {
        atomic_create_bytes(&meeting, &meeting_bytes)?;
    }
    let mut title_review = Vec::<String>::new();
    let old_title = journal.prior_document_title.as_deref();
    for (extension, markdown) in [("transcript.txt", false), ("transcript.md", true)] {
        if !rewrite_text_title(
            &staged_base.with_extension(extension),
            old_title,
            &journal.title,
            markdown,
        )? {
            title_review.push(extension.into());
        }
    }
    if !rewrite_word_title(
        &staged_base.with_extension("transcript.docx"),
        old_title,
        &journal.title,
    )? {
        title_review.push("transcript.docx".into());
    }
    // Minutes are authored content, and an H1 is not reliably an application
    // generated meeting name. Keep it and surface the need to review it.
    if staged_base.with_extension("summary.md").exists()
        || staged_base.with_extension("summary.docx").exists()
    {
        title_review.push("纪要正文标题".into());
    }
    for (extension, pointers) in [
        ("transcript.json", &["/source"][..]),
        (
            "recording.json",
            &["/stats/output", "/stats/audio_output"][..],
        ),
    ] {
        let path = staged_base.with_extension(extension);
        if !exists(&path) {
            continue;
        }
        let (bytes, identity, fingerprint) =
            read_title_source(&path, 64 * 1024 * 1024)?.context("会议元数据超过读取上限")?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        rewrite_paths(&mut value, pointers, journal);
        if extension == "transcript.json" {
            value["document_title"] = journal.title.clone().into();
        }
        let updated = serde_json::to_vec_pretty(&value)?;
        verify_title_source(&path, &identity, &fingerprint)?;
        atomic_bytes(&path, &updated)?;
    }
    let mut manifest = journal.manifest.clone();
    if title_review.is_empty() {
        manifest.extra.remove("document_titles_to_review");
    } else {
        manifest.extra.insert(
            "document_titles_to_review".into(),
            serde_json::to_value(title_review)?,
        );
    }
    let manifest_path = staged_base.with_extension("manifest.json");
    let (bytes, identity, fingerprint) =
        read_title_source(&manifest_path, 64 * 1024 * 1024)?.context("会议清单超过读取上限")?;
    let current: MeetingManifest = serde_json::from_slice(&bytes)?;
    ensure!(
        current.meeting_id == journal.manifest.meeting_id,
        "会议清单身份已变化，未覆盖"
    );
    let updated = serde_json::to_vec_pretty(&manifest)?;
    if let Some(original) = &journal.original_manifest_fingerprint {
        let current_value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let updated_value: serde_json::Value = serde_json::from_slice(&updated)?;
        ensure!(
            &fingerprint == original || current_value == updated_value,
            "会议清单在重命名计划后发生变化，保留日志与清单供核对"
        );
    }
    verify_title_source(&manifest_path, &identity, &fingerprint)?;
    atomic_bytes(&manifest_path, &updated)?;
    let marker = journal.stage.join(OWNER_MARKER);
    if exists(&marker) {
        let (bytes, identity, fingerprint) =
            read_title_source(&marker, 256)?.context("所有权标记过大")?;
        ensure!(
            bytes == journal.operation_id.as_bytes(),
            "所有权标记不匹配，未覆盖"
        );
        verify_title_source(&marker, &identity, &fingerprint)?;
    } else {
        atomic_create_bytes(&marker, journal.operation_id.as_bytes())?;
    }
    Ok(())
}

fn advance(root: &Path, journal: &mut Journal) -> Result<RenamedMeeting> {
    validate_journal(root, journal)?;
    verify_identity(journal)?;
    let log = journal_path(root, &journal.operation_id);
    let old_dir = journal.old_base.parent().unwrap().to_path_buf();
    let new_dir = journal.new_base.parent().unwrap().to_path_buf();
    if journal.phase == Phase::Planned {
        match (exists(&old_dir), exists(&journal.stage)) {
            (true, false) => rename_no_replace(&old_dir, &journal.stage)?,
            (false, true) => (),
            _ => bail!("旧目录与暂存目录状态冲突；保留日志等待人工处理"),
        }
        journal.phase = Phase::Moving;
        write_journal(&log, journal)?;
    }
    while journal.phase == Phase::Moving && journal.index < journal.files.len() {
        let file = &journal.files[journal.index];
        let from = journal.stage.join(&file.from);
        let temporary = journal.stage.join(&file.temporary);
        let to = journal.stage.join(&file.to);
        ensure_plain_parents(&journal.stage, &file.from)?;
        if !journal.temporary_step {
            match (exists(&from), exists(&temporary)) {
                (true, false) => {
                    ensure_plain_file(&from)?;
                    rename_no_replace(&from, &temporary)?;
                }
                (false, true) => (),
                _ => bail!("资产暂存状态冲突：{}", from.display()),
            }
            ensure_plain_file(&temporary)?;
            journal.temporary_step = true;
            write_journal(&log, journal)?;
        }
        match (exists(&temporary), exists(&to)) {
            (true, false) => {
                ensure_plain_file(&temporary)?;
                rename_no_replace(&temporary, &to)?;
            }
            (false, true) => (),
            _ => bail!("资产目标状态冲突：{}", to.display()),
        }
        ensure_plain_file(&to)?;
        journal.index += 1;
        journal.temporary_step = false;
        write_journal(&log, journal)?;
    }
    if journal.phase == Phase::Moving {
        journal.phase = Phase::Updating;
        write_journal(&log, journal)?;
    }
    if journal.phase == Phase::Updating {
        update_metadata(journal)?;
        journal.phase = Phase::Placing;
        write_journal(&log, journal)?;
    }
    if journal.phase == Phase::Placing {
        match (exists(&journal.stage), exists(&new_dir)) {
            (true, false) => rename_no_replace(&journal.stage, &new_dir)?,
            (false, true) => {
                let marker = new_dir.join(OWNER_MARKER);
                let (bytes, _, _) = read_title_source(&marker, 256)?.context("所有权标记过大")?;
                ensure!(
                    bytes == journal.operation_id.as_bytes(),
                    "目标目录不属于此操作"
                );
            }
            _ => bail!("最终目录状态冲突；保留日志等待恢复"),
        }
        journal.phase = Phase::Committed;
        write_journal(&log, journal)?;
    }
    ensure!(journal.phase == Phase::Committed, "重命名尚未提交");
    let moved_paths = journal
        .paths
        .iter()
        .map(|item| (old_dir.join(&item.from), new_dir.join(&item.to)))
        .collect();
    Ok(RenamedMeeting {
        operation_id: journal.operation_id.clone(),
        old_base: journal.old_base.clone(),
        new_base: journal.new_base.clone(),
        meeting_id: journal.manifest.meeting_id.clone(),
        document_titles_to_review: crate::meeting_manifest(&journal.new_base)?
            .and_then(|manifest| manifest.extra.get("document_titles_to_review").cloned())
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        moved_paths,
    })
}

/// Start a recoverable rename of an explicitly managed meeting directory.
/// The caller must ensure no recorder, player, transcription, AI job, or
/// external program is writing files in this directory.
pub fn rename_managed_meeting(base: &Path, title: &str) -> Result<RenamedMeeting> {
    let mut journal = plan(base, title)?;
    let root = journal
        .old_base
        .parent()
        .and_then(Path::parent)
        .context("会议根目录无效")?
        .to_path_buf();
    let _lock = acquire_root(&root)?;
    let _meeting_lock = acquire_meeting(&root, &journal.manifest.meeting_id, true)?;
    // Re-plan after acquiring the cross-process rename lock.
    let expected_id = journal.manifest.meeting_id.clone();
    journal = plan(base, title)?;
    ensure!(
        journal.manifest.meeting_id == expected_id,
        "会议身份在操作期间改变"
    );
    let log = journal_path(&root, &journal.operation_id);
    write_journal(&log, &journal)?;
    advance(&root, &mut journal).context("重命名未完成；操作日志已保留，可在下次扫描时恢复")
}

/// Resume only journals created in this root. An error leaves the journal and
/// all remaining assets intact for a later retry or manual inspection.
pub fn recover_meeting_renames(root: &Path) -> Result<Vec<RenamedMeeting>> {
    let root = root.canonicalize().context("会议根目录不可访问")?;
    let _lock = acquire_root(&root)?;
    let mut recovered = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(JOURNAL_PREFIX)
            .and_then(|s| s.strip_suffix(".json"))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() || !entry.file_type()?.is_file() {
            continue;
        }
        let (bytes, _, _) = read_title_source(&entry.path(), 64 * 1024 * 1024)?
            .context("重命名日志超过读取上限")?;
        let mut journal: Journal = serde_json::from_slice(&bytes)
            .with_context(|| format!("重命名日志损坏：{}", entry.path().display()))?;
        ensure!(journal.operation_id == id, "重命名日志编号不匹配");
        let _meeting_lock = acquire_meeting(&root, &journal.manifest.meeting_id, true)?;
        recovered.push(advance(&root, &mut journal)?);
    }
    Ok(recovered)
}

/// Acknowledge only after durable index migration. Never delete an unfinished
/// journal or a marker belonging to a different operation.
pub fn acknowledge_meeting_rename(result: &RenamedMeeting) -> Result<()> {
    let root = result
        .new_base
        .parent()
        .and_then(Path::parent)
        .context("会议根目录无效")?;
    let _lock = acquire_root(root)?;
    let log = journal_path(root, &result.operation_id);
    let (bytes, log_identity, log_fingerprint) =
        read_title_source(&log, 64 * 1024 * 1024)?.context("重命名日志超过读取上限")?;
    let journal: Journal = serde_json::from_slice(&bytes)?;
    validate_journal(root, &journal)?;
    ensure!(
        journal.phase == Phase::Committed
            && journal.old_base == result.old_base
            && journal.new_base == result.new_base
            && journal.manifest.meeting_id == result.meeting_id,
        "重命名日志尚未提交或不匹配"
    );
    let _meeting_lock = acquire_meeting(root, &journal.manifest.meeting_id, true)?;
    let manifest_path = result.new_base.with_extension("manifest.json");
    let (manifest_bytes, manifest_identity, manifest_fingerprint) =
        read_title_source(&manifest_path, 64 * 1024 * 1024)?.context("会议清单超过读取上限")?;
    let manifest: MeetingManifest = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        manifest.meeting_id == journal.manifest.meeting_id
            && Some(manifest.managed_directory.as_os_str()) == result.new_base.file_name(),
        "提交后的会议清单身份或目录不匹配，保留重命名日志"
    );
    let marker = result.new_base.parent().unwrap().join(OWNER_MARKER);
    let (marker_bytes, marker_identity, marker_fingerprint) =
        read_title_source(&marker, 256)?.context("重命名所有权标记过大")?;
    ensure!(
        marker_bytes == result.operation_id.as_bytes(),
        "重命名所有权标记不匹配"
    );
    verify_title_source(&log, &log_identity, &log_fingerprint)?;
    verify_title_source(&marker, &marker_identity, &marker_fingerprint)?;
    verify_title_source(&manifest_path, &manifest_identity, &manifest_fingerprint)?;
    fs::remove_file(&log)?;
    sync_parent_directory(&log)?;
    // The marker is harmless if this final cleanup is interrupted.
    verify_title_source(&marker, &marker_identity, &marker_fingerprint)?;
    fs::remove_file(&marker)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetOwnership, MeetingAsset, MeetingManifest};

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "opencap-rename-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        // /var is a symlink to /private/var on macOS; the rename result uses
        // the canonical root.
        root.canonicalize().unwrap()
    }

    fn fixture(root: &Path, old_name: &str) -> PathBuf {
        let dir = root.join(old_name);
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join(old_name);
        fs::write(base.with_extension("mp4"), b"media bytes must survive").unwrap();
        fs::write(
            base.with_extension("transcript.txt"),
            format!("{old_name} · 逐字稿\n\n第一句。\n"),
        )
        .unwrap();
        // The application escapes Markdown-significant characters in the
        // generated title, including the `-` and `_` of a timestamp folder.
        fs::write(
            base.with_extension("transcript.md"),
            "# 2026\\-01\\-01\\_10\\-00\\-00 · 逐字稿\n\n第一句。\n",
        )
        .unwrap();
        fs::write(
            base.with_extension("transcript.json"),
            serde_json::json!({
                "document_title": old_name,
                "source": base.with_extension("mp4"),
                "segments": []
            })
            .to_string(),
        )
        .unwrap();
        let asset = |role: &str, suffix: &str| MeetingAsset {
            asset_id: new_stable_id(),
            role: role.into(),
            ownership: AssetOwnership::Managed,
            path: PathBuf::from(format!("{old_name}.{suffix}")),
            extra: BTreeMap::new(),
        };
        let manifest = MeetingManifest {
            schema_version: 2,
            meeting_id: new_stable_id(),
            title: old_name.into(),
            managed_directory: PathBuf::from(old_name),
            assets: vec![
                asset("recording_video", "mp4"),
                asset("transcript", "transcript.txt"),
                asset("transcript", "transcript.md"),
                asset("transcript", "transcript.json"),
            ],
            transcription_runs: Vec::new(),
            extra: BTreeMap::new(),
        };
        fs::write(
            base.with_extension("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        base
    }

    fn journals(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(JOURNAL_PREFIX))
            })
            .collect()
    }

    #[test]
    fn managed_rename_moves_folder_assets_and_titles_without_touching_media() {
        let root = temp_root("success");
        let old_name = "2026-01-01_10-00-00";
        let old_base = fixture(&root, old_name);
        let before = fs::read(old_base.with_extension("mp4")).unwrap();

        let result = rename_managed_meeting(&old_base, "产品评审").unwrap();

        let new_base = root.join("产品评审").join("产品评审");
        assert_eq!(result.new_base, new_base);
        assert!(!root.join(old_name).exists());
        assert_eq!(fs::read(new_base.with_extension("mp4")).unwrap(), before);
        assert_eq!(
            fs::read_to_string(new_base.with_extension("transcript.txt")).unwrap(),
            "产品评审 · 逐字稿\n\n第一句。\n"
        );
        assert_eq!(
            fs::read_to_string(new_base.with_extension("transcript.md")).unwrap(),
            "# 产品评审 · 逐字稿\n\n第一句。\n"
        );
        let transcript: serde_json::Value =
            serde_json::from_slice(&fs::read(new_base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert_eq!(transcript["document_title"], "产品评审");
        assert_eq!(
            transcript["source"],
            new_base.with_extension("mp4").to_string_lossy().as_ref()
        );
        let manifest = meeting_manifest(&new_base).unwrap().unwrap();
        assert_eq!(manifest.title, "产品评审");
        assert_eq!(manifest.managed_directory, PathBuf::from("产品评审"));
        assert_eq!(manifest.extra["rename_generation"], 1);
        assert!(manifest.assets.iter().all(|asset| {
            asset
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("产品评审.")
        }));
        assert_eq!(
            result.relocate_path(&old_base.with_extension("mp4")),
            new_base.with_extension("mp4")
        );

        // The journal survives until the index has migrated, then recovery is
        // idempotent and acknowledgement removes it.
        assert_eq!(journals(&root).len(), 1);
        let recovered = recover_meeting_renames(&root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].new_base, new_base);
        assert_eq!(journals(&root).len(), 1);
        acknowledge_meeting_rename(&result).unwrap();
        assert!(journals(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_target_directory_is_never_overwritten() {
        let root = temp_root("collision");
        let old_base = fixture(&root, "例会");
        let occupied = root.join("产品评审");
        fs::create_dir_all(&occupied).unwrap();
        fs::write(occupied.join("keep.txt"), b"other meeting").unwrap();

        assert!(rename_managed_meeting(&old_base, "产品评审").is_err());
        assert!(root.join("例会").join("例会.mp4").exists());
        assert_eq!(
            fs::read(occupied.join("keep.txt")).unwrap(),
            b"other meeting"
        );
        assert!(journals(&root).is_empty(), "计划失败不能留下日志");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn case_only_rename_uses_the_journalled_temporary_name() {
        let root = temp_root("case");
        let old_base = fixture(&root, "weekly");
        let result = rename_managed_meeting(&old_base, "Weekly").unwrap();
        let new_base = root.join("Weekly").join("Weekly");
        assert_eq!(result.new_base, new_base);
        assert!(new_base.with_extension("mp4").is_file());
        let manifest = meeting_manifest(&new_base).unwrap().unwrap();
        assert_eq!(manifest.title, "Weekly");
        acknowledge_meeting_rename(&result).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod real_media_tests {
    use super::*;

    #[test]
    #[ignore = "requires OPENCAP_RENAME_TEST_BASE pointing at a managed meeting with real media"]
    fn renaming_a_managed_meeting_keeps_large_media_bytes() {
        let base = std::env::var_os("OPENCAP_RENAME_TEST_BASE")
            .map(PathBuf::from)
            .expect("set OPENCAP_RENAME_TEST_BASE");
        let media = base.with_extension("m4a");
        let before = crate::fingerprint_media(&media, None).unwrap();
        let result = rename_managed_meeting(&base, "真实改名测试").unwrap();
        let renamed_media = result.new_base.with_extension("m4a");
        assert_eq!(
            crate::fingerprint_media(&renamed_media, None).unwrap(),
            before
        );
        let manifest = crate::meeting_manifest(&result.new_base).unwrap().unwrap();
        assert_eq!(manifest.title, "真实改名测试");
        assert!(!base.parent().unwrap().exists());
        acknowledge_meeting_rename(&result).unwrap();
    }
}
