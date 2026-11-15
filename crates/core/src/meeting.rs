//! Storage shared by recordings and imported meeting artifacts.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::OnceLock,
    sync::atomic::{AtomicBool, Ordering},
};

/// New IDs describe entities, never paths or displayed titles.
pub fn new_stable_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingManifest {
    pub schema_version: u32,
    pub meeting_id: String,
    pub title: String,
    pub managed_directory: PathBuf,
    #[serde(default)]
    pub assets: Vec<MeetingAsset>,
    #[serde(default)]
    pub transcription_runs: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetOwnership {
    Managed,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingAsset {
    pub asset_id: String,
    pub role: String,
    pub ownership: AssetOwnership,
    /// Managed paths are relative to the meeting folder; external paths are absolute.
    pub path: PathBuf,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

pub fn is_input_media_asset(asset: &MeetingAsset) -> bool {
    asset.role == "source_media"
        || (asset.ownership == AssetOwnership::Managed
            && matches!(asset.role.as_str(), "recording_audio" | "recording_video"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFingerprint {
    pub sha256: String,
    pub size_bytes: u64,
}

/// Open a regular file without waiting for a FIFO writer on Unix.
pub fn open_regular_file_for_read(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("无法打开文件 {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "不是普通文件：{}",
        path.display()
    );
    Ok(file)
}

/// Hash an unchanged open file and reject replacement of the path while it is
/// read. A later external modification is always possible; callers that need
/// evidence across a task should compare independent before and after hashes.
pub fn fingerprint_media(path: &Path, cancelled: Option<&AtomicBool>) -> Result<MediaFingerprint> {
    let mut file = open_regular_file_for_read(path)?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let before = file.metadata()?;
    anyhow::ensure!(before.is_file(), "原媒体不是文件");
    let mut digest = Sha256::new();
    let mut size_bytes = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        anyhow::ensure!(
            !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
            "媒体校验已取消"
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size_bytes = size_bytes
            .checked_add(count as u64)
            .context("媒体文件过大")?;
        digest.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    let current = open_regular_file_for_read(path)?;
    anyhow::ensure!(
        current.metadata()?.is_file() && identity == same_file::Handle::from_file(current)?,
        "媒体路径在校验期间指向了其他文件"
    );
    anyhow::ensure!(
        before.len() == size_bytes
            && before.len() == after.len()
            && before.modified().ok() == after.modified().ok(),
        "媒体在校验期间发生变化"
    );
    Ok(MediaFingerprint {
        sha256: format!("{:x}", digest.finalize()),
        size_bytes,
    })
}

/// Record a verified content hash only for the asset still resolving to the
/// path that was read. Never overwrite a different historical identity.
pub fn record_asset_fingerprint(
    base: &Path,
    expected_meeting_id: &str,
    asset_id: &str,
    source: &Path,
    fingerprint: &MediaFingerprint,
) -> Result<()> {
    let source = source.canonicalize().context("原媒体不可访问")?;
    let _activity = crate::managed_meeting_activity(base)?;
    let _file_guard = crate::meeting_rename::manifest_write_guard(base)?;
    let _guard = manifest_lock().lock();
    let (mut manifest, manifest_baseline) =
        read_manifest_snapshot(base)?.context("会议资产清单不可用")?;
    anyhow::ensure!(
        manifest.meeting_id == expected_meeting_id,
        "会议身份已变化，未记录指纹"
    );
    anyhow::ensure!(
        !asset_id.is_empty()
            && manifest
                .assets
                .iter()
                .filter(|asset| asset.asset_id == asset_id)
                .take(2)
                .count()
                == 1,
        "媒体资产编号不存在或不唯一，未记录指纹"
    );
    let checked_asset = manifest
        .assets
        .iter()
        .find(|asset| asset.asset_id == asset_id)
        .context("找不到媒体资产")?;
    anyhow::ensure!(
        is_input_media_asset(checked_asset),
        "资产不是输入媒体，未记录指纹"
    );
    if checked_asset.ownership == AssetOwnership::Managed {
        anyhow::ensure!(
            manifest
                .assets
                .iter()
                .filter(|other| other.ownership == AssetOwnership::Managed
                    && other.path == checked_asset.path)
                .take(2)
                .count()
                == 1,
            "受管理媒体路径重复，未记录指纹"
        );
    }
    let asset = manifest
        .assets
        .iter_mut()
        .find(|asset| asset.asset_id == asset_id)
        .context("找不到媒体资产")?;
    let asset_path = match asset.ownership {
        AssetOwnership::Managed => checked_managed_asset_path(base, &asset.path)?,
        AssetOwnership::External => asset.path.clone(),
    };
    anyhow::ensure!(
        asset_path.canonicalize()? == source,
        "媒体位置已变更，未记录指纹"
    );
    if let Some(existing) = asset.extra.get("sha256") {
        anyhow::ensure!(
            existing.as_str() == Some(fingerprint.sha256.as_str()),
            "媒体内容与资产已有指纹不一致，原身份记录未改写"
        );
    }
    if let Some(existing) = asset.extra.get("size_bytes") {
        anyhow::ensure!(
            existing.as_u64() == Some(fingerprint.size_bytes),
            "媒体大小与资产已有记录不一致"
        );
    }
    asset
        .extra
        .insert("sha256".into(), fingerprint.sha256.clone().into());
    asset
        .extra
        .insert("size_bytes".into(), fingerprint.size_bytes.into());
    write_manifest(base, &manifest, Some(&manifest_baseline))
}

fn manifest_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Resolve an owned path without treating symlinked components as owned.
/// Missing components remain a valid offline reference.
pub fn checked_managed_asset_path(base: &Path, relative: &Path) -> Result<PathBuf> {
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "受管理资产路径无效"
    );
    let base = meeting_base(base);
    let root = base.parent().context("会议目录无效")?;
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "受管理资产路径包含符号链接：{}",
                    current.display()
                );
                if components.peek().is_some() {
                    anyhow::ensure!(
                        metadata.is_dir(),
                        "受管理资产父路径不是目录：{}",
                        current.display()
                    );
                } else {
                    anyhow::ensure!(
                        metadata.is_file(),
                        "受管理资产不是普通文件：{}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法检查资产路径 {}", current.display()));
            }
        }
    }
    Ok(root.join(relative))
}

pub fn meeting_manifest(base: &Path) -> Result<Option<MeetingManifest>> {
    Ok(read_manifest_snapshot(base)?.map(|(manifest, _)| manifest))
}

pub fn read_manifest_snapshot(base: &Path) -> Result<Option<(MeetingManifest, MediaFingerprint)>> {
    const LIMIT: u64 = 64 * 1024 * 1024;
    let path = meeting_base(base).with_extension("manifest.json");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(metadata.file_type().is_file(), "会议资产清单不是普通文件"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut file = open_regular_file_for_read(&path)?;
    let before = file.metadata()?;
    anyhow::ensure!(before.len() <= LIMIT, "会议资产清单超过 64 MiB");
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "会议资产清单超过 64 MiB");
    let after = file.metadata()?;
    anyhow::ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && std::fs::symlink_metadata(&path)?.file_type().is_file()
            && same_file::Handle::from_file(open_regular_file_for_read(&path)?)? == identity,
        "读取期间会议资产清单发生变化"
    );
    let fingerprint = MediaFingerprint {
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    Ok(Some((
        serde_json::from_slice(&bytes).context("会议资产清单无法读取")?,
        fingerprint,
    )))
}

fn write_manifest(
    base: &Path,
    manifest: &MeetingManifest,
    expected: Option<&MediaFingerprint>,
) -> Result<()> {
    let path = meeting_base(base).with_extension("manifest.json");
    if expected.is_none() {
        return crate::meeting_rename::atomic_create_bytes(
            &path,
            &serde_json::to_vec_pretty(manifest)?,
        );
    }
    let temp = path.with_file_name(format!(".opencap-manifest-{}.tmp", new_stable_id()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)?;
        file.sync_all()?;
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.file_type().is_file() && metadata.len() <= 64 * 1024 * 1024,
            "会议资产清单不是普通文件或超过读取上限，未覆盖"
        );
        anyhow::ensure!(
            Some(&fingerprint_media(&path, None)?) == expected,
            "会议资产清单已被其他操作修改，未覆盖；请重试"
        );
        std::fs::rename(&temp, &path)?;
        crate::meeting_rename::sync_parent_directory(&path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

/// Update only an existing manifest. Legacy files remain untouched until a
/// dedicated migration has established which assets are actually managed.
pub fn register_meeting_asset(
    base: &Path,
    role: &str,
    ownership: AssetOwnership,
    path: &Path,
) -> Result<Option<String>> {
    register_meeting_asset_checked(base, role, ownership, path, None)
}

pub fn register_meeting_asset_checked(
    base: &Path,
    role: &str,
    ownership: AssetOwnership,
    path: &Path,
    expected_meeting_id: Option<&str>,
) -> Result<Option<String>> {
    match ownership {
        AssetOwnership::Managed => {
            checked_managed_asset_path(base, path)?;
        }
        AssetOwnership::External => anyhow::ensure!(path.is_absolute(), "外部资产必须使用绝对路径"),
    }
    let _activity = crate::managed_meeting_activity(base)?;
    let _file_guard = crate::meeting_rename::manifest_write_guard(base)?;
    let _guard = manifest_lock().lock();
    if ownership == AssetOwnership::Managed {
        checked_managed_asset_path(base, path)?;
    }
    let Some((mut manifest, manifest_baseline)) = read_manifest_snapshot(base)? else {
        anyhow::ensure!(
            expected_meeting_id.is_none(),
            "会议清单已不可用，未登记资产"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        expected_meeting_id.is_none_or(|id| id == manifest.meeting_id),
        "会议身份已变化，未登记资产"
    );
    let mut matches = manifest
        .assets
        .iter()
        .filter(|asset| asset.ownership == ownership && asset.path == path);
    if let Some(asset) = matches.next() {
        anyhow::ensure!(
            matches.next().is_none()
                && !asset.asset_id.is_empty()
                && manifest
                    .assets
                    .iter()
                    .filter(|other| other.asset_id == asset.asset_id)
                    .take(2)
                    .count()
                    == 1,
            "资产路径或编号不唯一，无法安全复用"
        );
        return Ok(Some(asset.asset_id.clone()));
    }
    let id = new_stable_id();
    manifest.assets.push(MeetingAsset {
        asset_id: id.clone(),
        role: role.to_owned(),
        ownership,
        path: path.to_path_buf(),
        extra: BTreeMap::new(),
    });
    write_manifest(base, &manifest, Some(&manifest_baseline))?;
    Ok(Some(id))
}

pub fn register_meeting_run(base: &Path, run_id: &str) -> Result<()> {
    register_meeting_run_checked(base, run_id, None)
}

pub fn register_meeting_run_checked(
    base: &Path,
    run_id: &str,
    expected_meeting_id: Option<&str>,
) -> Result<()> {
    register_meeting_run_with_input(base, run_id, expected_meeting_id, None)
}

pub fn register_meeting_run_with_input(
    base: &Path,
    run_id: &str,
    expected_meeting_id: Option<&str>,
    input_asset_id: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(!run_id.trim().is_empty(), "识别运行编号为空，未登记");
    let _activity = crate::managed_meeting_activity(base)?;
    let _file_guard = crate::meeting_rename::manifest_write_guard(base)?;
    let _guard = manifest_lock().lock();
    if let Some((mut manifest, manifest_baseline)) = read_manifest_snapshot(base)? {
        anyhow::ensure!(
            expected_meeting_id.is_none_or(|id| id == manifest.meeting_id),
            "会议身份已变化，未登记识别运行"
        );
        if let Some(id) = input_asset_id {
            let mut assets = manifest.assets.iter().filter(|asset| asset.asset_id == id);
            let asset = assets.next();
            anyhow::ensure!(
                !id.is_empty()
                    && asset.is_some_and(is_input_media_asset)
                    && assets.next().is_none(),
                "输入媒体资产不存在、角色无效或编号重复，未登记识别运行"
            );
            if let Some(asset) = asset
                && asset.ownership == AssetOwnership::Managed
            {
                anyhow::ensure!(
                    manifest
                        .assets
                        .iter()
                        .filter(|other| other.ownership == AssetOwnership::Managed
                            && other.path == asset.path)
                        .take(2)
                        .count()
                        == 1,
                    "受管理媒体路径重复，未登记识别运行"
                );
            }
        }
        let matches = manifest
            .transcription_runs
            .iter()
            .filter(|id| id.as_str() == run_id)
            .take(2)
            .count();
        anyhow::ensure!(matches < 2, "识别运行编号重复，未修改会议清单");
        if matches == 0 {
            manifest.transcription_runs.push(run_id.to_owned());
            write_manifest(base, &manifest, Some(&manifest_baseline))?;
        }
    } else {
        anyhow::ensure!(
            expected_meeting_id.is_none(),
            "会议清单已不可用，未登记识别运行"
        );
    }
    Ok(())
}

/// Resolve an asset through its stable ID. A missing file remains a valid
/// reference; callers decide how to present an offline asset.
pub fn resolve_meeting_asset(base: &Path, asset_id: &str) -> Result<Option<PathBuf>> {
    let Some(manifest) = meeting_manifest(base)? else {
        return Ok(None);
    };
    resolve_asset_in_manifest(base, &manifest, asset_id)
}

/// Resolve against one already-read manifest snapshot, without mixing versions.
pub fn resolve_asset_in_manifest(
    base: &Path,
    manifest: &MeetingManifest,
    asset_id: &str,
) -> Result<Option<PathBuf>> {
    anyhow::ensure!(!asset_id.is_empty(), "资产编号为空");
    let mut matches = manifest
        .assets
        .iter()
        .filter(|asset| asset.asset_id == asset_id);
    let Some(asset) = matches.next() else {
        return Ok(None);
    };
    anyhow::ensure!(matches.next().is_none(), "资产编号不唯一，无法安全解析");
    if asset.ownership == AssetOwnership::Managed {
        anyhow::ensure!(
            manifest
                .assets
                .iter()
                .filter(
                    |other| other.ownership == AssetOwnership::Managed && other.path == asset.path
                )
                .take(2)
                .count()
                == 1,
            "受管理资产路径不唯一，无法安全解析"
        );
    }
    let path = match asset.ownership {
        AssetOwnership::External => {
            anyhow::ensure!(asset.path.is_absolute(), "外部资产路径无效");
            asset.path.clone()
        }
        AssetOwnership::Managed => checked_managed_asset_path(base, &asset.path)?,
    };
    Ok(Some(path))
}

/// Change only an external reference selected by the user. This does not move,
/// rewrite or delete either file; callers must not claim byte identity without
/// a saved digest to compare. Preserve previous locations for manual recovery.
pub struct RelinkedAsset {
    pub path: PathBuf,
    pub content_verified: bool,
    pub meeting_id: String,
    pub previous_path: PathBuf,
}

pub fn relink_external_meeting_asset(
    base: &Path,
    asset_id: &str,
    selected: &Path,
) -> Result<RelinkedAsset> {
    relink_external_meeting_asset_cancellable(base, asset_id, selected, None)
}

pub fn relink_external_meeting_asset_cancellable(
    base: &Path,
    asset_id: &str,
    selected: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<RelinkedAsset> {
    relink_external_meeting_asset_checked(base, asset_id, selected, cancelled, None, None)
}

pub fn relink_external_meeting_asset_checked(
    base: &Path,
    asset_id: &str,
    selected: &Path,
    cancelled: Option<&AtomicBool>,
    expected_role: Option<&str>,
    expected_meeting_id: Option<&str>,
) -> Result<RelinkedAsset> {
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
        "媒体重定位已取消"
    );
    let selected = selected.canonicalize().context("所选原媒体不可访问")?;
    anyhow::ensure!(selected.is_file(), "请选择音频或视频文件");
    let selected_file = open_regular_file_for_read(&selected)?;
    let selected_before = selected_file.metadata()?;
    let selected_identity = same_file::Handle::from_file(selected_file.try_clone()?)?;
    let selected_unchanged = || -> Result<bool> {
        let after = selected_file.metadata()?;
        Ok(selected_before.len() == after.len()
            && selected_before.modified()? == after.modified()?
            && same_file::Handle::from_file(open_regular_file_for_read(&selected)?)?
                == selected_identity)
    };
    let _activity = crate::managed_meeting_activity(base)?;
    let original = meeting_manifest(base)?.context("此会议尚无资产清单，不能自动重定位")?;
    anyhow::ensure!(
        expected_meeting_id.is_none_or(|id| original.meeting_id == id),
        "会议身份已变化，请刷新后重新定位"
    );
    anyhow::ensure!(
        !asset_id.is_empty()
            && original
                .assets
                .iter()
                .filter(|asset| asset.asset_id == asset_id)
                .take(2)
                .count()
                == 1,
        "媒体资产编号不存在或不唯一，无法重定位"
    );
    let original_asset = original
        .assets
        .iter()
        .find(|asset| asset.asset_id == asset_id)
        .context("会议中找不到该资产")?;
    anyhow::ensure!(
        expected_role.is_none_or(|role| original_asset.role == role),
        "媒体资产角色已变化，请刷新后重新定位"
    );
    let original_role = original_asset.role.clone();
    let expected_hash = original_asset
        .extra
        .get("sha256")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let expected_size = original_asset
        .extra
        .get("size_bytes")
        .and_then(|value| value.as_u64());
    let expected_path = original_asset.path.clone();
    let observed = if expected_hash.is_some() {
        Some(fingerprint_media(&selected, cancelled)?)
    } else {
        None
    };
    anyhow::ensure!(
        selected_unchanged()?,
        "所选媒体在核对期间发生变化，原引用保留"
    );
    let _file_guard = crate::meeting_rename::manifest_write_guard(base)?;
    let _guard = manifest_lock().lock();
    let (mut manifest, manifest_baseline) =
        read_manifest_snapshot(base)?.context("此会议尚无资产清单，不能自动重定位")?;
    anyhow::ensure!(
        manifest.meeting_id == original.meeting_id,
        "会议身份已变化，未更新媒体位置"
    );
    anyhow::ensure!(
        manifest
            .assets
            .iter()
            .filter(|asset| asset.asset_id == asset_id)
            .take(2)
            .count()
            == 1,
        "媒体资产编号已发生冲突，未更新位置"
    );
    anyhow::ensure!(
        !manifest
            .assets
            .iter()
            .any(|other| other.asset_id != asset_id
                && other.ownership == AssetOwnership::External
                && other.path == selected),
        "所选位置已登记给其他外部资产，原引用保留"
    );
    let asset = manifest
        .assets
        .iter_mut()
        .find(|asset| asset.asset_id == asset_id)
        .context("会议中找不到该资产")?;
    anyhow::ensure!(
        asset.role == original_role,
        "媒体资产角色已变化，请刷新后重新定位"
    );
    anyhow::ensure!(
        asset.ownership == AssetOwnership::External,
        "受管理资产不能按外部引用重定位"
    );
    anyhow::ensure!(
        asset.path == expected_path,
        "媒体位置已被其他任务更改，请重新定位"
    );
    anyhow::ensure!(
        asset.extra.get("sha256").and_then(|value| value.as_str()) == expected_hash.as_deref()
            && asset
                .extra
                .get("size_bytes")
                .and_then(|value| value.as_u64())
                == expected_size,
        "媒体身份信息已变化，请重新定位"
    );
    if let Some(observed) = &observed {
        anyhow::ensure!(
            Some(observed.sha256.as_str()) == expected_hash.as_deref()
                && expected_size.is_none_or(|size| size == observed.size_bytes),
            "所选文件与原媒体指纹不一致；原引用保留"
        );
    }
    anyhow::ensure!(
        !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
        "媒体重定位已取消，原引用保留"
    );
    anyhow::ensure!(
        selected_unchanged()?,
        "所选媒体在保存前发生变化，原引用保留"
    );
    if asset.path != selected {
        let prior = serde_json::to_value(&asset.path)?;
        let history = asset
            .extra
            .entry("relink_history".into())
            .or_insert_with(|| serde_json::json!([]));
        let locations = history
            .as_array_mut()
            .context("原路径历史格式异常；未修改引用")?;
        if !locations.contains(&prior) {
            locations.push(prior);
        }
        asset.path = selected.clone();
        write_manifest(base, &manifest, Some(&manifest_baseline))?;
    }
    let content_verified = observed.is_some() && selected_unchanged().unwrap_or(false);
    Ok(RelinkedAsset {
        path: selected,
        content_verified,
        meeting_id: manifest.meeting_id,
        previous_path: expected_path,
    })
}

/// One identity for a media path and the corresponding artifact prefix.
/// Resolve existing parent aliases without moving or renaming user files.
pub fn meeting_base(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stem = [
        ".transcript.json",
        ".recording.json",
        ".partial.mp4",
        ".partial.m4a",
        ".mp4",
        ".m4a",
    ]
    .into_iter()
    .find_map(|suffix| name.strip_suffix(suffix))
    .unwrap_or(&name);
    let parent = path.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf())
        .join(stem)
}

pub(crate) fn folder_name(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .take(80)
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '.'
                )
            {
                '_'
            } else {
                c
            }
        })
        // Leave room for long, application-generated sidecar suffixes (for
        // example a full SHA-256 archive name) under common 255-byte limits.
        .scan(0usize, |bytes, c| {
            let next = bytes.saturating_add(c.len_utf8());
            if next > 120 {
                None
            } else {
                *bytes = next;
                Some(c)
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '_']).to_owned();
    if cleaned.is_empty() {
        return "会议".into();
    }
    let upper = cleaned.to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
    {
        format!("会议_{cleaned}")
    } else {
        cleaned
    }
}
/// Atomically allocate a new directory. Never reuse or overwrite an existing meeting.
/// The returned extensionless prefix keeps all generated artifacts in that directory.
pub fn create_meeting_base(root: &Path, label: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root).context("无法创建会议保存目录")?;
    let name = folder_name(label);
    for n in 0..1000 {
        let name = if n == 0 {
            name.clone()
        } else {
            format!("{name}_{n:02}")
        };
        let dir = root.join(&name);
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                let base = dir.join(&name);
                let manifest = MeetingManifest {
                    schema_version: 1,
                    meeting_id: new_stable_id(),
                    title: label.to_owned(),
                    managed_directory: PathBuf::from(&name),
                    assets: Vec::new(),
                    transcription_runs: Vec::new(),
                    extra: BTreeMap::new(),
                };
                write_manifest(&base, &manifest, None).context("无法创建会议身份清单")?;
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("无法创建本场会议文件夹"),
        }
    }
    anyhow::bail!("同名会议过多，请更换保存目录或文件名")
}
/// Include legacy flat files and new meeting folders; do not follow directory symlinks.
pub fn recoverable_recordings(root: &Path) -> Vec<PathBuf> {
    fn scan(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten().take(2000) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                scan(&entry.path(), depth - 1, files);
            } else if kind.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if (name.ends_with(".partial.mp4") || name.ends_with(".partial.m4a"))
                    && entry.metadata().is_ok_and(|m| m.len() > 0)
                {
                    files.push(entry.path());
                }
            }
        }
    }
    let mut files = Vec::new();
    scan(root, 3, &mut files);
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meeting_folders_are_distinct_portable_and_recovery_finds_nested_and_legacy_files() {
        let root = std::env::temp_dir().join(format!(
            "opencap-folders-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = create_meeting_base(&root, "例会/预算:2.0?").unwrap();
        let second = create_meeting_base(&root, "例会/预算:2.0?").unwrap();
        assert_ne!(base.parent(), second.parent());
        assert!(base.parent().unwrap().starts_with(&root));
        assert_eq!(base.file_name().unwrap(), "例会_预算_2_0");
        let recording = base.with_extension("partial.m4a");
        std::fs::write(&recording, b"media fixture").unwrap();
        let legacy = root.join("legacy.partial.mp4");
        std::fs::write(&legacy, b"legacy fixture").unwrap();
        std::fs::write(root.join("empty.partial.mp4"), []).unwrap();
        std::fs::write(root.join("completed.m4a"), b"complete").unwrap();
        let mut expected = vec![recording, legacy];
        expected.sort();
        assert_eq!(recoverable_recordings(&root), expected);
        assert_eq!(
            std::fs::read(base.with_extension("partial.m4a")).unwrap(),
            b"media fixture"
        );
        for label in ["CON", "LPT1", "..", " / ", "NUL", "COM9"] {
            let safe = create_meeting_base(&root, label).unwrap();
            assert_ne!(
                safe.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_uppercase(),
                label
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
