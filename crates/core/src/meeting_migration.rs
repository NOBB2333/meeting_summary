//! Non-destructive, on-demand migration of a legacy meeting into a new
//! managed directory. Original files and unrecognized neighbours are untouched.
use crate::{AssetOwnership, MeetingAsset, MeetingManifest, meeting_base, new_stable_id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

/// Advisory progress only: copied bytes exclude verification reads.
#[derive(Default)]
pub struct LegacyMigrationProgress {
    phase: AtomicU8,
    copied_bytes: AtomicU64,
}
impl LegacyMigrationProgress {
    pub fn snapshot(&self) -> (u8, u64) {
        (
            self.phase.load(Ordering::Relaxed),
            self.copied_bytes.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone)]
pub struct LegacyMigration {
    pub original_base: PathBuf,
    pub new_base: PathBuf,
    pub meeting_id: String,
    pub media_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportRequest {
    version: u32,
    operation_id: String,
    original_base: PathBuf,
    title: String,
    selected_media: Option<PathBuf>,
    archive_media: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovered_to: Option<PathBuf>,
}

/// An interrupted copy which can be explicitly restarted from its original
/// files. The staging directory is never treated as a completed meeting.
#[derive(Debug, Clone)]
pub struct PendingLegacyImport {
    pub stage: PathBuf,
    pub original_base: PathBuf,
    pub title: String,
    pub archive_media: bool,
    pub request_fingerprint: crate::MediaFingerprint,
}

fn import_stage(root: &Path, stage: &Path) -> Result<(PathBuf, String)> {
    let root = root.canonicalize().context("会议根目录不可用")?;
    let parent = stage
        .parent()
        .context("迁移暂存目录没有父目录")?
        .canonicalize()?;
    ensure!(parent == root, "迁移暂存目录不属于当前会议根目录");
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .context("迁移暂存目录名无效")?;
    let id = name
        .strip_prefix(".opencap-import-")
        .and_then(|name| name.strip_suffix(".dir"))
        .context("迁移暂存目录名无效")?;
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "迁移操作编号无效");
    let stage = root.join(name);
    ensure!(
        fs::symlink_metadata(&stage)?.file_type().is_dir(),
        "迁移暂存目录不是普通目录"
    );
    Ok((stage, id.to_owned()))
}

fn import_lock(root: &Path, id: &str, create: bool) -> Result<fs::File> {
    let path = root.join(format!(".opencap-import-{id}.lock"));
    if !create {
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file(),
            "迁移锁不是普通文件"
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .open(&path)?;
    ensure!(file.metadata()?.is_file(), "迁移锁不是普通文件");
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_file()
            && identity == same_file::Handle::from_file(crate::open_regular_file_for_read(&path)?)?,
        "迁移锁在打开期间发生变化"
    );
    file.try_lock()
        .map_err(|error| anyhow::anyhow!("迁移仍在执行或锁不可用：{error}"))?;
    Ok(file)
}

fn read_import_request(stage: &Path, id: &str) -> Result<(ImportRequest, crate::MediaFingerprint)> {
    ensure!(
        owns_marker(&stage.join(".opencap-import-owner"), id),
        "迁移所有权标记无效"
    );
    let (bytes, _, fingerprint) = crate::meeting_rename::read_title_source(
        &stage.join(".opencap-import-request.json"),
        64 * 1024,
    )?
    .context("迁移请求超过大小限制")?;
    let request: ImportRequest = serde_json::from_slice(&bytes).context("迁移请求无法读取")?;
    ensure!(
        request.version == 1
            && request.operation_id == id
            && request.original_base.is_absolute()
            && request
                .selected_media
                .as_ref()
                .is_none_or(|path| path.is_absolute())
            && !request.title.trim().is_empty()
            && request.title.chars().count() <= 100
            && !request.title.chars().any(char::is_control),
        "迁移请求内容无效"
    );
    Ok((request, fingerprint))
}

pub fn inspect_pending_legacy_import(root: &Path, stage: &Path) -> Result<PendingLegacyImport> {
    let (stage, id) = import_stage(root, stage)?;
    let _lock = import_lock(stage.parent().context("迁移根目录无效")?, &id, false)?;
    let (request, request_fingerprint) = read_import_request(&stage, &id)?;
    ensure!(
        request.recovered_to.is_none(),
        "此暂存已记录恢复结果，请先检查会议列表"
    );
    Ok(PendingLegacyImport {
        stage,
        original_base: request.original_base,
        title: request.title,
        archive_media: request.archive_media,
        request_fingerprint,
    })
}

/// Retry a stopped copy from the original files. The abandoned partial copy is
/// renamed aside for inspection, never deleted or treated as the new meeting.
pub fn retry_pending_legacy_import(
    root: &Path,
    pending: &PendingLegacyImport,
    cancelled: Option<&AtomicBool>,
    progress: Option<&LegacyMigrationProgress>,
) -> Result<(LegacyMigration, Option<String>)> {
    let (stage, id) = import_stage(root, &pending.stage)?;
    let root = stage.parent().context("迁移根目录无效")?;
    let _lock = import_lock(root, &id, false)?;
    let (mut request, fingerprint) = read_import_request(&stage, &id)?;
    ensure!(
        fingerprint == pending.request_fingerprint
            && request.recovered_to.is_none()
            && request.original_base == pending.original_base
            && request.title == pending.title
            && request.archive_media == pending.archive_media,
        "迁移请求已变化，请刷新后检查"
    );
    check_cancel(cancelled)?;
    let migrated = migrate_legacy_copy_cancellable(
        &request.original_base,
        root,
        &request.title,
        request.selected_media.as_deref(),
        request.archive_media,
        cancelled,
        progress,
    )?;
    request.recovered_to = Some(migrated.new_base.clone());
    let mark = (|| -> Result<()> {
        let path = stage.join(".opencap-import-request.json");
        let (_, identity, observed) = crate::meeting_rename::read_title_source(&path, 64 * 1024)?
            .context("迁移请求超过大小限制")?;
        ensure!(
            observed == fingerprint && owns_marker(&stage.join(".opencap-import-owner"), &id),
            "迁移请求或所有权已变化"
        );
        crate::meeting_rename::verify_title_source(&path, &identity, &observed)?;
        crate::meeting_rename::atomic_bytes(&path, &serde_json::to_vec_pretty(&request)?)?;
        Ok(())
    })();
    if let Err(error) = mark {
        return Ok((
            migrated,
            Some(format!(
                "恢复副本已创建，但原暂存未能标记；请先检查 {}：{error:#}",
                stage.display()
            )),
        ));
    }
    let retained = root.join(format!(".opencap-import-recovered-{id}.dir"));
    let warning = match crate::meeting_rename::rename_no_replace(&stage, &retained) {
        Ok(()) => format!("中断时的暂存副本保留在 {}", retained.display()),
        Err(error) => format!(
            "恢复副本已创建，旧暂存保留在 {}：{error:#}",
            stage.display()
        ),
    };
    Ok((migrated, Some(warning)))
}

/// List private import directories without treating them as meetings or
/// deleting them. Another process may still be copying into one of them.
pub fn pending_legacy_imports(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(".opencap-import-")
            .and_then(|name| name.strip_suffix(".dir"))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_ok() && entry.file_type()?.is_dir() {
            pending.push(entry.path());
        }
    }
    pending.sort();
    Ok(pending)
}

fn check_cancel(cancelled: Option<&AtomicBool>) -> Result<()> {
    ensure!(
        !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
        "迁移已取消，原会议保留"
    );
    Ok(())
}

fn fingerprint(path: &Path, cancelled: Option<&AtomicBool>) -> Result<(u64, String)> {
    let value = crate::fingerprint_media(path, cancelled)?;
    Ok((value.size_bytes, value.sha256))
}

fn read_legacy_transcript(path: &Path) -> Result<Vec<u8>> {
    const LIMIT: u64 = 64 * 1024 * 1024;
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "旧转写不是普通文件，原文件未修改"
    );
    let mut file = crate::open_regular_file_for_read(path)?;
    let before = file.metadata()?;
    ensure!(before.len() <= LIMIT, "旧转写超过 64 MiB，原文件未修改");
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "旧转写超过 64 MiB，原文件未修改"
    );
    let after = file.metadata()?;
    ensure!(
        before.len() == bytes.len() as u64
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && fs::symlink_metadata(path)?.file_type().is_file()
            && same_file::Handle::from_file(crate::open_regular_file_for_read(path)?)? == identity,
        "读取期间旧转写发生变化，原文件未修改"
    );
    Ok(bytes)
}

fn owns_marker(path: &Path, operation_id: &str) -> bool {
    (|| -> Result<bool> {
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_file(),
            "标记不是普通文件"
        );
        let mut file = crate::open_regular_file_for_read(path)?;
        let before = file.metadata()?;
        ensure!(before.len() <= 256, "标记过大");
        let identity = same_file::Handle::from_file(file.try_clone()?)?;
        let mut bytes = Vec::new();
        (&mut file).take(257).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(
            before.len() == bytes.len() as u64
                && before.len() == after.len()
                && before.modified()? == after.modified()?
                && fs::symlink_metadata(path)?.file_type().is_file()
                && same_file::Handle::from_file(crate::open_regular_file_for_read(path)?)?
                    == identity,
            "标记读取期间发生变化"
        );
        Ok(bytes == operation_id.as_bytes())
    })()
    .unwrap_or(false)
}

fn copy_verified(
    source: &Path,
    destination: &Path,
    cancelled: Option<&AtomicBool>,
    progress: Option<&LegacyMigrationProgress>,
) -> Result<((u64, String), same_file::Handle)> {
    if let Some(progress) = progress {
        progress.phase.store(1, Ordering::Relaxed);
    }
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "迁移源不是普通文件：{}",
        source.display()
    );
    let mut input = crate::open_regular_file_for_read(source)?;
    let opened = input.metadata()?;
    ensure!(opened.is_file(), "迁移源不是普通文件：{}", source.display());
    let identity = same_file::Handle::from_file(input.try_clone()?)?;
    ensure!(
        fs::symlink_metadata(source)?.file_type().is_file()
            && identity
                == same_file::Handle::from_file(crate::open_regular_file_for_read(source)?)?,
        "打开迁移源时路径发生变化：{}",
        source.display()
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        check_cancel(cancelled)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        if let Some(progress) = progress {
            progress
                .copied_bytes
                .fetch_add(count as u64, Ordering::Relaxed);
        }
        hash.update(&buffer[..count]);
        size = size.saturating_add(count as u64);
    }
    output.sync_all()?;
    if let Some(progress) = progress {
        progress.phase.store(2, Ordering::Relaxed);
    }
    let digest = (size, format!("{:x}", hash.finalize()));
    ensure!(
        fingerprint(destination, cancelled)? == digest,
        "迁移副本校验失败：{}",
        destination.display()
    );
    ensure!(
        fs::symlink_metadata(source)?.file_type().is_file()
            && identity == same_file::Handle::from_path(source)?
            && fingerprint(source, cancelled)? == digest
            && identity == same_file::Handle::from_path(source)?
            && fs::symlink_metadata(source)?.file_type().is_file(),
        "迁移期间源文件发生变化：{}",
        source.display()
    );
    Ok((digest, identity))
}

fn absolute_reference(path: &Path, parent: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        parent.join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

/// Copy only recognized application sidecars. If `archive_media` is false,
/// the media remains an external reference; otherwise the selected source is
/// copied and verified, while the original is still left untouched.
///
/// All copies are prepared in a hidden directory on the destination volume.
/// A failed operation never removes or changes the original meeting. An
/// interrupted process may leave its hidden staging directory for inspection.
pub fn migrate_legacy_copy(
    base: &Path,
    destination_root: &Path,
    title: &str,
    selected_media: Option<&Path>,
    archive_media: bool,
) -> Result<LegacyMigration> {
    migrate_legacy_copy_cancellable(
        base,
        destination_root,
        title,
        selected_media,
        archive_media,
        None,
        None,
    )
}

pub fn migrate_legacy_copy_cancellable(
    base: &Path,
    destination_root: &Path,
    title: &str,
    selected_media: Option<&Path>,
    archive_media: bool,
    cancelled: Option<&AtomicBool>,
    progress: Option<&LegacyMigrationProgress>,
) -> Result<LegacyMigration> {
    check_cancel(cancelled)?;
    let title = title.trim();
    ensure!(
        !title.is_empty() && title.chars().count() <= 100 && !title.chars().any(char::is_control),
        "名称须为 1–100 个非控制字符"
    );
    let original_base = meeting_base(base);
    ensure!(
        crate::meeting_manifest(&original_base)?.is_none(),
        "该会议已有资产清单，无需迁移"
    );
    let source_dir = original_base.parent().context("旧会议目录无效")?;
    let old_name = original_base
        .file_name()
        .and_then(|name| name.to_str())
        .context("旧会议名不是有效 UTF-8")?;
    let mut files = Vec::<(PathBuf, String)>::new();
    for entry in fs::read_dir(source_dir)? {
        check_cancel(cancelled)?;
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&format!("{old_name}.")) else {
            continue;
        };
        if crate::meeting_rename::known_suffix(suffix) {
            ensure!(
                entry.file_type()?.is_file(),
                "旧会议资产不是普通文件：{}",
                entry.path().display()
            );
            files.push((entry.path(), suffix.to_owned()));
        }
    }
    files.sort_by(|left, right| left.1.cmp(&right.1));
    ensure!(
        files
            .iter()
            .any(|(_, suffix)| matches!(suffix.as_str(), "transcript.json" | "recording.json")),
        "未找到可迁移的会议转写或录制报告"
    );
    let transcript_path = files
        .iter()
        .find(|(_, suffix)| suffix == "transcript.json")
        .map(|(path, _)| path);
    let transcript_bytes = transcript_path
        .map(|path| read_legacy_transcript(path))
        .transpose()
        .context("旧转写 JSON 无法读取，原文件未修改")?;
    let transcript = transcript_bytes
        .as_deref()
        .map(serde_json::from_slice::<serde_json::Value>)
        .transpose()
        .context("旧转写 JSON 无法解析，原文件未修改")?;
    let transcript_fingerprint = transcript_bytes
        .as_ref()
        .map(|bytes| (bytes.len() as u64, format!("{:x}", Sha256::digest(bytes))));
    if let Some(value) = &transcript {
        ensure!(value.is_object(), "旧转写不是 JSON 对象");
        for field in ["meeting_id", "input_asset_id"] {
            if let Some(id) = value.pointer(&format!("/run/{field}"))
                && !id.is_null()
            {
                ensure!(
                    id.as_str()
                        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
                    "旧转写的 {field} 不是有效身份，无法安全迁移；原文件未修改"
                );
            }
        }
    }
    let historical_meeting_id = transcript
        .as_ref()
        .and_then(|value| value.pointer("/run/meeting_id"))
        .and_then(|value| value.as_str())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .map(str::to_owned);
    let historical_source_id = transcript
        .as_ref()
        .and_then(|value| value.pointer("/run/input_asset_id"))
        .and_then(|value| value.as_str())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .map(str::to_owned);
    let saved_source = transcript
        .as_ref()
        .and_then(|value| value.get("source"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let historical_fingerprint = if saved_source.is_some()
        && transcript
            .as_ref()
            .and_then(|value| value.pointer("/run/input_integrity"))
            .and_then(|value| value.as_str())
            == Some("hash_matched_before_and_after")
    {
        let value = transcript
            .as_ref()
            .and_then(|value| value.pointer("/run/input_media_fingerprint"))
            .context("旧运行标记已核对媒体，但缺少指纹；原文件未修改")?;
        let fingerprint: crate::MediaFingerprint =
            serde_json::from_value(value.clone()).context("旧运行媒体指纹无效；原文件未修改")?;
        ensure!(
            fingerprint.sha256.len() == 64
                && fingerprint
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "旧运行媒体指纹格式无效；原文件未修改"
        );
        Some(fingerprint)
    } else {
        None
    };
    let source = saved_source
        .as_deref()
        .or(selected_media)
        .map(|path| absolute_reference(path, source_dir))
        .transpose()?;
    if archive_media {
        ensure!(
            source.as_ref().is_some_and(|path| path.is_file()),
            "归档媒体前需要可访问的原音视频"
        );
    }
    fs::create_dir_all(destination_root)?;
    let root = destination_root.canonicalize()?;
    let suggested = crate::meeting::folder_name(title);
    let mut chosen = None;
    for n in 0..1000 {
        let name = if n == 0 {
            suggested.clone()
        } else {
            format!("{suggested}_{n:02}")
        };
        match fs::symlink_metadata(root.join(&name)) {
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                chosen = Some(name);
                break;
            }
            Err(error) => return Err(error).context("无法检查迁移目标目录"),
        }
    }
    let chosen = chosen.context("同名会议过多")?;
    let new_dir = root.join(&chosen);
    let new_base = new_dir.join(&chosen);
    let operation_id = new_stable_id();
    let _import_lock = import_lock(&root, &operation_id, true)?;
    let stage = root.join(format!(".opencap-import-{operation_id}.dir"));
    fs::create_dir(&stage)?;
    let marker = stage.join(".opencap-import-owner");
    let result = (|| -> Result<LegacyMigration> {
        crate::meeting_rename::atomic_create_bytes(&marker, operation_id.as_bytes())?;
        let request = ImportRequest {
            version: 1,
            operation_id: operation_id.clone(),
            original_base: if original_base.is_absolute() {
                original_base.clone()
            } else {
                std::env::current_dir()?.join(&original_base)
            },
            title: title.to_owned(),
            selected_media: source
                .as_ref()
                .map(|path| -> std::io::Result<PathBuf> {
                    if path.is_absolute() {
                        Ok(path.clone())
                    } else {
                        Ok(std::env::current_dir()?.join(path))
                    }
                })
                .transpose()?,
            archive_media,
            recovered_to: None,
        };
        crate::meeting_rename::atomic_create_bytes(
            &stage.join(".opencap-import-request.json"),
            &serde_json::to_vec_pretty(&request)?,
        )?;
        let mut manifest = MeetingManifest {
            schema_version: 1,
            meeting_id: historical_meeting_id.unwrap_or_else(new_stable_id),
            title: title.to_owned(),
            managed_directory: PathBuf::from(&chosen),
            assets: Vec::new(),
            transcription_runs: Vec::new(),
            extra: BTreeMap::new(),
        };
        manifest.extra.insert("legacy_migration".into(), serde_json::json!({
            "original_base": original_base, "original_preserved": true,
            "media_strategy": if archive_media { "verified_copy" } else { "external_reference" },
            "media_provenance": if saved_source.is_some() { "recorded_source_path" } else { "unverified_candidate" }
        }));
        let mut evidence = Vec::new();
        for (source_file, suffix) in &files {
            let filename = format!("{chosen}.{suffix}");
            let (digest, identity) =
                copy_verified(source_file, &stage.join(&filename), cancelled, progress)?;
            if suffix == "transcript.json" {
                ensure!(
                    transcript_fingerprint.as_ref() == Some(&digest),
                    "迁移期间旧转写发生变化，请重试；原文件未修改"
                );
            }
            evidence.push((source_file.clone(), digest, identity));
            manifest.assets.push(MeetingAsset {
                asset_id: new_stable_id(),
                role: crate::meeting_rename::recovered_asset_role(suffix).into(),
                ownership: AssetOwnership::Managed,
                path: PathBuf::from(filename),
                extra: BTreeMap::new(),
            });
        }
        let mut historical_media_verified = false;
        let source_for_transcript = if let Some(source) = &source {
            let (ownership, path) = if archive_media {
                let ext = source
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .context("原媒体没有有效扩展名")?;
                ensure!(
                    ext.chars().all(|c| c.is_ascii_alphanumeric()) && ext.len() <= 12,
                    "原媒体扩展名无效"
                );
                let filename = format!("{chosen}.{}", ext.to_ascii_lowercase());
                let (digest, identity) =
                    copy_verified(source, &stage.join(&filename), cancelled, progress)?;
                if let Some(expected) = &historical_fingerprint {
                    ensure!(
                        digest.0 == expected.size_bytes && digest.1 == expected.sha256,
                        "当前媒体与旧运行指纹不一致，未将其归档为原输入；原文件未修改"
                    );
                    historical_media_verified = true;
                }
                evidence.push((source.clone(), digest.clone(), identity));
                let mut extra = BTreeMap::new();
                extra.insert("sha256".into(), digest.1.into());
                extra.insert("size_bytes".into(), digest.0.into());
                manifest.assets.push(MeetingAsset {
                    asset_id: if saved_source.is_some() {
                        historical_source_id.clone().unwrap_or_else(new_stable_id)
                    } else {
                        new_stable_id()
                    },
                    role: if saved_source.is_some() {
                        "source_media"
                    } else {
                        "candidate_media"
                    }
                    .into(),
                    ownership: AssetOwnership::Managed,
                    path: PathBuf::from(&filename),
                    extra,
                });
                (AssetOwnership::Managed, PathBuf::from(filename))
            } else {
                let mut extra = BTreeMap::new();
                if let Some(expected) = &historical_fingerprint {
                    extra.insert("sha256".into(), expected.sha256.clone().into());
                    extra.insert("size_bytes".into(), expected.size_bytes.into());
                    extra.insert(
                        "fingerprint_origin".into(),
                        "historical_run_expected".into(),
                    );
                }
                manifest.assets.push(MeetingAsset {
                    asset_id: if saved_source.is_some() {
                        historical_source_id.clone().unwrap_or_else(new_stable_id)
                    } else {
                        new_stable_id()
                    },
                    role: if saved_source.is_some() {
                        "source_media"
                    } else {
                        "candidate_media"
                    }
                    .into(),
                    ownership: AssetOwnership::External,
                    path: source.clone(),
                    extra,
                });
                (AssetOwnership::External, source.clone())
            };
            Some(if ownership == AssetOwnership::Managed {
                new_dir.join(path)
            } else {
                path
            })
        } else {
            None
        };
        if let Some(migration) = manifest
            .extra
            .get_mut("legacy_migration")
            .and_then(|value| value.as_object_mut())
        {
            migration.insert(
                "historical_media_verified".into(),
                historical_media_verified.into(),
            );
        }
        if let Some(mut value) = transcript {
            if saved_source.is_some()
                && let Some(source) = source_for_transcript
            {
                value["source"] = serde_json::to_value(source)?;
            }
            if let Some(run) = value.get_mut("run").and_then(|run| run.as_object_mut()) {
                // Backfill only missing identity. Existing IDs were
                // validated and retained when constructing the manifest.
                if run.get("meeting_id").is_none_or(|value| value.is_null()) {
                    run.insert("meeting_id".into(), manifest.meeting_id.clone().into());
                }
                // A recorded source path establishes which manifest asset
                // belongs to this old run; a merely selected replacement
                // does not establish that provenance.
                if saved_source.is_some()
                    && run
                        .get("input_asset_id")
                        .is_none_or(|value| value.is_null())
                    && let Some(asset) = manifest
                        .assets
                        .iter()
                        .find(|asset| asset.role == "source_media")
                {
                    run.insert("input_asset_id".into(), asset.asset_id.clone().into());
                }
            }
            if let Some(run_id) = value
                .pointer("/run/run_id")
                .and_then(|value| value.as_str())
            {
                manifest.transcription_runs.push(run_id.to_owned());
            }
            crate::meeting_rename::atomic_bytes(
                &stage.join(format!("{chosen}.transcript.json")),
                &serde_json::to_vec_pretty(&value)?,
            )?;
        }
        let meeting = stage.join(format!("{chosen}.meeting.json"));
        let mut metadata: serde_json::Value = if meeting.is_file() {
            serde_json::from_slice(&fs::read(&meeting)?)?
        } else {
            serde_json::json!({})
        };
        ensure!(metadata.is_object(), "旧会议名称文件格式错误");
        metadata["title"] = title.into();
        crate::meeting_rename::atomic_bytes(&meeting, &serde_json::to_vec_pretty(&metadata)?)?;
        if !manifest
            .assets
            .iter()
            .any(|asset| asset.path == Path::new(&format!("{chosen}.meeting.json")))
        {
            manifest.assets.push(MeetingAsset {
                asset_id: new_stable_id(),
                role: "meeting.json".into(),
                ownership: AssetOwnership::Managed,
                path: PathBuf::from(format!("{chosen}.meeting.json")),
                extra: BTreeMap::new(),
            });
        }
        crate::meeting_rename::atomic_bytes(
            &stage.join(format!("{chosen}.manifest.json")),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        // Confirm that a concurrent legacy writer did not change any input
        // after it was copied. All originals remain available on failure.
        for (path, expected, identity) in evidence {
            ensure!(
                identity == same_file::Handle::from_path(&path)?
                    && fingerprint(&path, cancelled)? == expected
                    && identity == same_file::Handle::from_path(&path)?,
                "迁移期间源文件发生变化：{}",
                path.display()
            );
        }
        let _root_guard = crate::meeting_rename::acquire_root(&root)?;
        check_cancel(cancelled)?;
        if let Some(progress) = progress {
            progress.phase.store(3, Ordering::Relaxed);
        }
        crate::meeting_rename::rename_no_replace(&stage, &new_dir)?;
        let published_marker = new_dir.join(".opencap-import-owner");
        if owns_marker(&published_marker, &operation_id) {
            let _ = fs::remove_file(new_dir.join(".opencap-import-request.json"));
            let _ = fs::remove_file(published_marker);
        }
        Ok(LegacyMigration {
            original_base,
            new_base,
            meeting_id: manifest.meeting_id,
            media_archived: archive_media && source.is_some(),
        })
    })();
    match result {
        Ok(migrated) => Ok(migrated),
        Err(error) => {
            if owns_marker(&new_dir.join(".opencap-import-owner"), &operation_id) {
                return Err(error.context(format!(
                    "迁移副本可能已发布到 {}，但持久化确认失败；请检查该目录，原会议保留",
                    new_dir.display()
                )));
            }
            let cleanup = if owns_marker(&marker, &operation_id) {
                // Check ownership and refuse an observed symlink replacement.
                if fs::symlink_metadata(&stage)
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                {
                    fs::remove_dir_all(&stage)
                } else {
                    Err(std::io::Error::other("暂存目录已变化，未清理"))
                }
            } else {
                // If publishing the marker failed, remove only an empty stage.
                fs::remove_dir(&stage)
            };
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                    Err(error)
                }
                Err(cleanup_error) => Err(error.context(format!(
                    "迁移未完成；暂存目录 {} 未能清理（{}），原会议保留",
                    stage.display(),
                    cleanup_error
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "opencap-migrate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn legacy(root: &Path) -> (PathBuf, PathBuf) {
        let dir = root.join("legacy");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("Legacy");
        let transcript = serde_json::json!({
            "complete": true,
            "segments": [{
                "id": 0,
                "revision": 1,
                "start_ms": 0,
                "end_ms": 1500,
                "text": "历史逐字稿。",
                "finalized": true
            }]
        });
        fs::write(
            base.with_extension("transcript.json"),
            serde_json::to_vec(&transcript).unwrap(),
        )
        .unwrap();
        fs::write(
            base.with_extension("summary.md"),
            "# 旧纪要\n\n预算待定。\n",
        )
        .unwrap();
        fs::write(base.with_extension("notes.txt"), "旧笔记").unwrap();
        let media = base.with_extension("m4a");
        fs::write(&media, b"legacy media bytes").unwrap();
        (base, media)
    }

    #[test]
    fn legacy_copy_keeps_originals_and_references_external_media() {
        let root = temp_root("external");
        let (base, media) = legacy(&root);
        let result = migrate_legacy_copy(&base, &root, "产品评审", Some(&media), false).unwrap();
        assert!(!result.new_base.is_file());
        assert!(result.new_base.with_extension("transcript.json").is_file());
        assert!(result.new_base.with_extension("summary.md").is_file());
        assert!(result.new_base.with_extension("notes.txt").is_file());
        let manifest = crate::meeting_manifest(&result.new_base).unwrap().unwrap();
        assert_eq!(manifest.title, "产品评审");
        assert_eq!(manifest.meeting_id, result.meeting_id);
        assert!(!result.media_archived);
        // A legacy transcript without a saved source cannot prove which media
        // was the original input, so the migration registers a candidate.
        let media_asset = manifest
            .assets
            .iter()
            .find(|asset| asset.role == "candidate_media")
            .unwrap();
        assert_eq!(media_asset.ownership, AssetOwnership::External);
        assert_eq!(media_asset.path, media);
        // The original meeting remains readable and byte-identical.
        assert_eq!(fs::read(&media).unwrap(), b"legacy media bytes");
        assert!(base.with_extension("transcript.json").is_file());
        assert_eq!(
            fs::read_to_string(base.with_extension("notes.txt")).unwrap(),
            "旧笔记"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archived_media_is_copied_verified_and_the_original_stays() {
        let root = temp_root("archived");
        let (base, media) = legacy(&root);
        let result = migrate_legacy_copy(&base, &root, "产品评审", Some(&media), true).unwrap();
        assert!(result.media_archived);
        let manifest = crate::meeting_manifest(&result.new_base).unwrap().unwrap();
        let media_asset = manifest
            .assets
            .iter()
            .find(|asset| asset.role == "candidate_media")
            .unwrap();
        assert_eq!(media_asset.ownership, AssetOwnership::Managed);
        let copied = result.new_base.parent().unwrap().join(&media_asset.path);
        assert_eq!(fs::read(&copied).unwrap(), b"legacy media bytes");
        assert_eq!(fs::read(&media).unwrap(), b"legacy media bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_or_already_managed_legacy_meetings_are_refused() {
        let root = temp_root("refuse");
        let (base, media) = legacy(&root);
        let cancelled = AtomicBool::new(true);
        assert!(
            migrate_legacy_copy_cancellable(
                &base,
                &root,
                "产品评审",
                Some(&media),
                false,
                Some(&cancelled),
                None
            )
            .is_err()
        );
        assert!(!root.join("产品评审").exists());
        // A copy with a manifest cannot be migrated a second time.
        let result = migrate_legacy_copy(&base, &root, "产品评审", Some(&media), false).unwrap();
        assert!(migrate_legacy_copy(&result.new_base, &root, "再次迁移", None, false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
