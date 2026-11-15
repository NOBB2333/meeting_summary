use anyhow::{Context, Result};
use opencap_core::{LiveAsrModel, OfflineAsrModel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const DEFAULT_HOTWORDS: &str = "录屏,语音转录,会议纪要,实时字幕";
const MAX_LEGACY_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_LEGACY_WEIGHT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TOKENS_BYTES: u64 = 16 * 1024 * 1024;
const MAX_VAD_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct RemoteFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}
struct RemoteModel {
    repo: &'static str,
    revision: &'static str,
    files: &'static [RemoteFile],
}
fn remote_model(kind: ModelKind) -> Option<RemoteModel> {
    let (repo, revision, files): (_, _, &'static [RemoteFile]) = match kind {
        ModelKind::StreamingStandard => (
            "k2-fsa/sherpa-onnx-streaming-zipformer-multi-zh-hans-2023-12-12",
            "ac54a23c9d106dfbd178be831329fabe261bac58",
            &[
                RemoteFile {
                    name: "encoder-epoch-20-avg-1-chunk-16-left-128.int8.onnx",
                    size: 70109350,
                    sha256: "1870fad66d2d7b0d9ea8045859ddd32621d505d363b3242a9de13183809751cb",
                },
                RemoteFile {
                    name: "decoder-epoch-20-avg-1-chunk-16-left-128.onnx",
                    size: 5165083,
                    sha256: "67d37eedeb20aca87ea41682849649c174601b45e3dbdf16c2170de7f3b4b89d",
                },
                RemoteFile {
                    name: "joiner-epoch-20-avg-1-chunk-16-left-128.int8.onnx",
                    size: 1033416,
                    sha256: "7c00afc5450290d0e0df2bda461a20c6891110b9f111291396ed57559db8b4e9",
                },
                RemoteFile {
                    name: "tokens.txt",
                    size: 18626,
                    sha256: "6722bd1585f46f84456b29c3550a343a3cc375b971645773c02ed8e0b4e2405c",
                },
            ],
        ),
        ModelKind::StreamingEnhanced => (
            "csukuangfj/sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30",
            "ad658fa0201659a09ea3c176129a191c77ecae8f",
            &[
                RemoteFile {
                    name: "encoder.int8.onnx",
                    size: 161141793,
                    sha256: "5ac51e27981bb4dab01bb9be4958453ba50c3b61c063ddda0eab23fd3671aa4f",
                },
                RemoteFile {
                    name: "decoder.onnx",
                    size: 5165083,
                    sha256: "06522ad63cec0fdf6809f4e1db9bb4f7d710c34582e3b35db62ac60eccafac7e",
                },
                RemoteFile {
                    name: "joiner.int8.onnx",
                    size: 1033416,
                    sha256: "b34584dc6f561089e1d747fedebb3765f2caa72c927ef54d7ca55e5ae40a814b",
                },
                RemoteFile {
                    name: "tokens.txt",
                    size: 20628,
                    sha256: "6193c7ea1c96d0d9a1e9652789b40d13a8a913b434a5451e93158f5a09fd6652",
                },
            ],
        ),
        ModelKind::ParaformerSmall => (
            "csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09",
            "63ddc3cd0f2810b68289a7b3876e62ef5d53d6df",
            &[
                RemoteFile {
                    name: "model.int8.onnx",
                    size: 81828675,
                    sha256: "3ef6c19369b912f7caf3cef8e545c5ccd1a33d9d7ec792a46668dc41c4b229ec",
                },
                RemoteFile {
                    name: "tokens.txt",
                    size: 75352,
                    sha256: "4b2d964e18b9cf139b473003b6698fb2ed9a2a5ec55b93daa677b28f578897aa",
                },
            ],
        ),
        ModelKind::ParaformerLarge => (
            "csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09",
            "906992d326ebf0c5171cde675aa0902be9e5bc6c",
            &[
                RemoteFile {
                    name: "model.int8.onnx",
                    size: 227330205,
                    sha256: "90bc03034ae1bef9575f8cc798cd1519c8be8aa9e8b458a033e32017ff4d584c",
                },
                RemoteFile {
                    name: "tokens.txt",
                    size: 75354,
                    sha256: "6c0e3b35cece259829e6cb5b8d90d13db88f61ea3a2953d11898e4b2bfd7a2e2",
                },
            ],
        ),
        _ => return None,
    };
    Some(RemoteModel {
        repo,
        revision,
        files,
    })
}

pub(crate) fn copy_verified(
    reader: &mut impl Read,
    writer: &mut impl Write,
    item: &RemoteFile,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        anyhow::ensure!(bytes <= item.size, "{} 文件大小异常", item.name);
        writer.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        progress(bytes);
    }
    anyhow::ensure!(
        bytes == item.size && format!("{:x}", hasher.finalize()) == item.sha256,
        "{} 校验失败，请重试",
        item.name
    );
    Ok(bytes)
}

pub(crate) struct CancellableReader<'a, R> {
    pub reader: R,
    pub cancelled: &'a AtomicBool,
}
impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("下载已取消"));
        }
        self.reader.read(buffer)
    }
}
pub(crate) fn ensure_download_active(cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "下载已取消");
    Ok(())
}

/// Serialize the short publication step across cooperating application
/// processes; downloads themselves remain independent and cancellable.
pub(crate) fn model_install_guard(root: &Path) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".opencap-model-install.lock"))?;
    lock.try_lock()
        .map_err(|error| anyhow::anyhow!("另一个模型安装正在提交，请稍后重试：{error}"))?;
    Ok(lock)
}

#[derive(Serialize, serde::Deserialize)]
struct InstallJournal {
    destination: String,
    backup: String,
}

fn install_journal_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!(".opencap-install-{name}.json"))
}
pub(crate) fn model_path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Called with the model-install lock. Preserve the old directory if a
/// process stopped between the two publication renames.
pub(crate) fn recover_model_install(root: &Path, name: &str) -> Result<()> {
    let journal = install_journal_path(root, name);
    let file = match fs::File::open(&journal) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 4096,
        "模型安装恢复日志过大：{}",
        journal.display()
    );
    let record: InstallJournal =
        serde_json::from_slice(&bytes).context("模型安装恢复日志损坏，原文件保留")?;
    let prefix = format!(".incomplete-{name}-");
    anyhow::ensure!(
        record.destination == name
            && record.backup.strip_prefix(&prefix).is_some_and(
                |suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
            ),
        "模型安装恢复日志身份不匹配，原文件保留"
    );
    let dest = root.join(name);
    let backup = root.join(&record.backup);
    let dest_exists = model_path_exists(&dest)?;
    let backup_exists = model_path_exists(&backup)?;
    if !dest_exists && backup_exists {
        let metadata = fs::symlink_metadata(&backup)?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "模型备份不是普通目录：{}",
            backup.display()
        );
        opencap_core::rename_no_replace(&backup, &dest).context("无法恢复模型原目录")?;
        opencap_core::sync_parent_directory(&dest)?;
    } else {
        anyhow::ensure!(
            dest_exists,
            "模型原目录及备份均不存在；恢复日志保留：{}",
            journal.display()
        );
    }
    fs::remove_file(&journal)?;
    opencap_core::sync_parent_directory(&journal)?;
    Ok(())
}

pub(crate) fn journal_model_replace(root: &Path, name: &str, nonce: u128) -> Result<PathBuf> {
    let backup_name = format!(".incomplete-{name}-{nonce}");
    match fs::symlink_metadata(root.join(&backup_name)) {
        Ok(_) => anyhow::bail!("模型备份位置已存在，未覆盖"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let journal = install_journal_path(root, name);
    let record = InstallJournal {
        destination: name.to_owned(),
        backup: backup_name.clone(),
    };
    opencap_core::atomic_create_bytes(&journal, &serde_json::to_vec(&record)?)?;
    Ok(root.join(backup_name))
}

pub(crate) fn finish_model_replace(root: &Path, name: &str) -> Result<()> {
    let journal = install_journal_path(root, name);
    fs::remove_file(&journal)?;
    opencap_core::sync_parent_directory(&journal)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelFileFingerprint {
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelFingerprint {
    /// SHA-256 of a deterministic list of filenames, lengths and byte hashes.
    pub manifest_sha256: String,
    pub files: std::collections::BTreeMap<String, ModelFileFingerprint>,
    pub cache_key: &'static str,
}

/// Compare only files with pinned hashes; auxiliary VAD is not covered.
pub(crate) fn known_weights_match(kind: ModelKind, actual: &ModelFingerprint) -> Option<bool> {
    remote_model(kind).map(|remote| {
        remote.files.iter().all(|expected| {
            actual
                .files
                .get(expected.name)
                .is_some_and(|file| file.bytes == expected.size && file.sha256 == expected.sha256)
        })
    })
}

/// Hash the bytes actually present, caching by file length and modification
/// time during this process. This is provenance, not a signature or proof of
/// model quality; files changed while hashing are rejected.
pub(crate) fn fingerprint_model(
    kind: ModelKind,
    root: &Path,
    cancelled: &AtomicBool,
    use_cache: bool,
) -> Result<ModelFingerprint> {
    type Cached = (u64, std::time::SystemTime, String);
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, Cached>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut files = std::collections::BTreeMap::new();
    let mut cache_hits = 0usize;
    for name in kind.files() {
        ensure_download_active(cancelled)?;
        let path = kind.path(root).join(name);
        let before =
            fs::metadata(&path).with_context(|| format!("无法读取模型文件 {}", path.display()))?;
        anyhow::ensure!(before.is_file(), "模型路径不是普通文件：{}", path.display());
        let modified = before.modified()?;
        let cached = use_cache
            .then(|| {
                cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&path)
                    .filter(|(len, mtime, _)| *len == before.len() && *mtime == modified)
                    .map(|(_, _, hash)| hash.clone())
            })
            .flatten();
        let hash = if let Some(hash) = cached {
            cache_hits += 1;
            hash
        } else {
            // The shared helper also compares the opened file identity with
            // the path after reading, including on Windows.
            let observed = opencap_core::fingerprint_media(&path, Some(cancelled))
                .with_context(|| format!("无法核对模型文件 {}", path.display()))?;
            let after = fs::metadata(&path)?;
            anyhow::ensure!(
                observed.size_bytes == before.len()
                    && after.len() == before.len()
                    && after.modified()? == modified,
                "计算指纹时模型文件发生变化：{}",
                path.display()
            );
            let hash = observed.sha256;
            cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(path, (observed.size_bytes, modified, hash.clone()));
            hash
        };
        files.insert(
            (*name).to_owned(),
            ModelFileFingerprint {
                bytes: before.len(),
                sha256: hash,
            },
        );
    }
    let canonical = serde_json::to_vec(&files)?;
    let cache_key = if cache_hits == 0 {
        "fresh_file_reads"
    } else if cache_hits == files.len() {
        "process_cache_length_and_mtime"
    } else {
        "mixed_fresh_and_process_cache_length_and_mtime"
    };
    Ok(ModelFingerprint {
        manifest_sha256: format!("{:x}", Sha256::digest(canonical)),
        files,
        cache_key,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum ModelKind {
    Streaming,
    StreamingStandard,
    StreamingEnhanced,
    Offline,
    ParaformerSmall,
    ParaformerLarge,
}

/// Describes the integration implemented here, not an unmeasured model claim.
#[derive(Debug, Clone, Copy)]
pub struct BackendCapabilities {
    pub streaming: &'static str,
    pub languages: &'static str,
    pub punctuation: &'static str,
    pub text_normalization: &'static str,
    pub hotwords: &'static str,
    pub timing: &'static str,
    pub speakers: &'static str,
    pub required: &'static str,
    pub optional: &'static str,
    pub providers: &'static str,
}
impl BackendCapabilities {
    pub fn description(&self) -> String {
        format!(
            "{} · {}\n标点：{}；文本规范化：{}；热词：{}\n时间：{}；说话人：{}\n必需：{}；可选：{}；执行设备：{}",
            self.streaming,
            self.languages,
            self.punctuation,
            self.text_normalization,
            self.hotwords,
            self.timing,
            self.speakers,
            self.required,
            self.optional,
            self.providers
        )
    }
}
impl ModelKind {
    pub const ALL: [Self; 6] = [
        Self::Streaming,
        Self::StreamingStandard,
        Self::StreamingEnhanced,
        Self::Offline,
        Self::ParaformerSmall,
        Self::ParaformerLarge,
    ];
    pub fn from_live(selection: LiveAsrModel) -> Option<Self> {
        match selection {
            LiveAsrModel::Auto | LiveAsrModel::Compact => Some(Self::Streaming),
            LiveAsrModel::Standard => Some(Self::StreamingStandard),
            LiveAsrModel::Enhanced => Some(Self::StreamingEnhanced),
            LiveAsrModel::Sentence => None,
        }
    }
    pub fn from_offline(selection: OfflineAsrModel) -> Self {
        match selection {
            OfflineAsrModel::SenseVoice => Self::Offline,
            OfflineAsrModel::ParaformerSmall => Self::ParaformerSmall,
            OfflineAsrModel::ParaformerLarge => Self::ParaformerLarge,
        }
    }
    pub fn is_streaming(self) -> bool {
        matches!(
            self,
            Self::Streaming | Self::StreamingStandard | Self::StreamingEnhanced
        )
    }
    pub fn capabilities(self) -> BackendCapabilities {
        if self.is_streaming() {
            BackendCapabilities {
                streaming: "原生流式",
                languages: "以中文为主；其他语言效果未验收",
                punctuation: "可选 CT 模型；缺失时仅补句末",
                text_normalization: "未确认",
                hotwords: "按词表过滤后请求，不保证一定识别",
                timing: "句段及可用的原始 token 起点；未逐词对齐",
                speakers: "不原生提供",
                required: "流式模型与词表",
                optional: "CT 标点、文件模型句末复核",
                providers: "当前集成 CPU",
            }
        } else if self == Self::Offline {
            BackendCapabilities {
                streaming: "VAD 逐句，非原生流式",
                languages: "可请求中文、英文或自动；自动模式的日韩粤效果未验收",
                punctuation: "模型原生输出",
                text_normalization: "当前路径请求 ITN；准确率未验收",
                hotwords: "当前文件模型路径不支持",
                timing: "VAD 句段及可用的原始 token 起点；未逐词对齐",
                speakers: "不原生提供",
                required: "模型、词表和 Silero VAD",
                optional: "会后说话人分析",
                providers: "当前集成 CPU",
            }
        } else {
            BackendCapabilities {
                streaming: "VAD 逐句，非原生流式",
                languages: "中文及部分英文；样本效果未验收",
                punctuation: "可选 CT 模型；缺失时仅补句末",
                text_normalization: "未确认",
                hotwords: "当前文件模型路径不支持",
                timing: "VAD 句段及可用的原始 token 起点；未逐词对齐",
                speakers: "不原生提供",
                required: "模型、词表和 Silero VAD",
                optional: "CT 标点、会后说话人分析",
                providers: "当前集成 CPU",
            }
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Streaming => "轻量实时 Zipformer",
            Self::StreamingStandard => "标准实时 Zipformer",
            Self::StreamingEnhanced => "增强实时 Zipformer",
            Self::Offline => "SenseVoice 多语言",
            Self::ParaformerSmall => "Paraformer 轻量中英",
            Self::ParaformerLarge => "Paraformer 标准中英",
        }
    }
    pub fn directory(self) -> &'static str {
        match self {
            Self::Streaming => "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23",
            Self::StreamingStandard => "sherpa-onnx-streaming-zipformer-multi-zh-hans-2023-12-12",
            Self::StreamingEnhanced => "sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30",
            Self::Offline => "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
            Self::ParaformerSmall => "sherpa-onnx-paraformer-zh-small-2024-03-09",
            Self::ParaformerLarge => "sherpa-onnx-paraformer-zh-2024-03-09",
        }
    }
    pub fn files(self) -> &'static [&'static str] {
        match self {
            Self::Streaming => &[
                "encoder-epoch-99-avg-1.int8.onnx",
                "decoder-epoch-99-avg-1.onnx",
                "joiner-epoch-99-avg-1.int8.onnx",
                "tokens.txt",
            ],
            Self::StreamingStandard => &[
                "encoder-epoch-20-avg-1-chunk-16-left-128.int8.onnx",
                "decoder-epoch-20-avg-1-chunk-16-left-128.onnx",
                "joiner-epoch-20-avg-1-chunk-16-left-128.int8.onnx",
                "tokens.txt",
            ],
            Self::StreamingEnhanced => &[
                "encoder.int8.onnx",
                "decoder.onnx",
                "joiner.int8.onnx",
                "tokens.txt",
            ],
            Self::Offline | Self::ParaformerSmall | Self::ParaformerLarge => {
                &["model.int8.onnx", "tokens.txt", "silero_vad.onnx"]
            }
        }
    }
    pub fn path(self, root: &Path) -> PathBuf {
        root.join(self.directory())
    }
    pub fn ready(self, root: &Path) -> bool {
        let remote = remote_model(self);
        self.files().iter().all(|name| {
            self.path(root).join(name).metadata().is_ok_and(|m| {
                m.is_file()
                    && remote
                        .as_ref()
                        .and_then(|r| r.files.iter().find(|f| f.name == *name))
                        .map_or(m.len() > 0, |f| m.len() == f.size)
            })
        })
    }
}
/// Reuse installed models without downloading in the recording hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveModelPlan {
    pub kind: ModelKind,
    pub context_revision: bool,
    pub revision_model: ModelKind,
}
/// Cache this snapshot in the UI; filesystem checks do not belong in rendering.
#[derive(Debug, Clone, Copy)]
pub struct ModelAvailability {
    pub streaming: bool,
    pub offline: bool,
    pub streaming_standard: bool,
    pub streaming_enhanced: bool,
    pub paraformer_small: bool,
    pub paraformer_large: bool,
    pub punctuation: bool,
}
impl ModelAvailability {
    pub fn inspect(root: &Path) -> Self {
        Self {
            streaming: ModelKind::Streaming.ready(root),
            offline: ModelKind::Offline.ready(root),
            streaming_standard: ModelKind::StreamingStandard.ready(root),
            streaming_enhanced: ModelKind::StreamingEnhanced.ready(root),
            paraformer_small: ModelKind::ParaformerSmall.ready(root),
            paraformer_large: ModelKind::ParaformerLarge.ready(root),
            punctuation: crate::punctuation_ready(root),
        }
    }
    pub fn ready(self, kind: ModelKind) -> bool {
        match kind {
            ModelKind::Streaming => self.streaming,
            ModelKind::StreamingStandard => self.streaming_standard,
            ModelKind::StreamingEnhanced => self.streaming_enhanced,
            ModelKind::Offline => self.offline,
            ModelKind::ParaformerSmall => self.paraformer_small,
            ModelKind::ParaformerLarge => self.paraformer_large,
        }
    }
    pub fn live_plan(self, revision: bool) -> Option<LiveModelPlan> {
        self.live_plan_for(LiveAsrModel::Auto, OfflineAsrModel::SenseVoice, revision)
    }
    pub fn live_plan_for(
        self,
        live: LiveAsrModel,
        offline: OfflineAsrModel,
        revision: bool,
    ) -> Option<LiveModelPlan> {
        let revision_model = ModelKind::from_offline(offline);
        let kind = if live == LiveAsrModel::Auto {
            if self.streaming {
                ModelKind::Streaming
            } else {
                revision_model
            }
        } else {
            ModelKind::from_live(live).unwrap_or(revision_model)
        };
        self.ready(kind).then_some(LiveModelPlan {
            kind,
            context_revision: kind.is_streaming() && revision && self.ready(revision_model),
            revision_model,
        })
    }
}
impl LiveModelPlan {
    pub fn installed(root: &Path, revision: bool) -> Option<Self> {
        ModelAvailability::inspect(root).live_plan(revision)
    }
    pub fn description(self) -> &'static str {
        if !self.kind.is_streaming() {
            "逐句模式 · 停顿后出字"
        } else if self.context_revision {
            "流式模式 · 边说边显示，停顿后用所选文件模型校正"
        } else {
            "流式模式 · 边说边显示，当前句持续更新"
        }
    }
}
pub fn model_root() -> PathBuf {
    // A developer shell may already export the production override. Never let
    // it redirect a Dev build's downloads or voice memory into production.
    let override_dir = if opencap_core::is_dev_profile() {
        None
    } else {
        std::env::var_os("OPENCAP_MODEL_DIR")
    };
    override_dir.map(PathBuf::from).unwrap_or_else(|| {
        opencap_core::config_path()
            .parent()
            .unwrap_or(Path::new("."))
            .join("models")
    })
}
/// Models are local after the first explicit download. Installation is atomic.
pub fn prepare(
    kind: ModelKind,
    root: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    prepare_cancellable(kind, root, &AtomicBool::new(false), progress)
}

pub fn prepare_cancellable(
    kind: ModelKind,
    root: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    ensure_download_active(cancelled)?;
    fs::create_dir_all(root)?;
    {
        let _guard = model_install_guard(root)?;
        recover_model_install(root, kind.directory())?;
        if kind.ready(root) {
            return Ok(kind.path(root));
        }
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging = root.join(format!(".download-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Blocking reqwest applies this to each read operation, not the
            // entire streaming download. Bound stalled reads so cancellation
            // can be observed without limiting a steadily progressing model.
            .timeout(Duration::from_secs(60))
            .build()?;
        let extracted = staging.join(kind.directory());
        fs::create_dir(&extracted)?;
        if let Some(remote) = remote_model(kind) {
            let total: u64 = remote.files.iter().map(|f| f.size).sum();
            let mut completed = 0;
            progress(0, Some(total));
            for item in remote.files {
                ensure_download_active(cancelled)?;
                let url = format!(
                    "https://huggingface.co/{}/resolve/{}/{}",
                    remote.repo, remote.revision, item.name
                );
                let response = download(&client, &url, cancelled)
                    .with_context(|| format!("下载 {} 失败", item.name))?;
                let mut response = CancellableReader {
                    reader: response,
                    cancelled,
                };
                let mut output = fs::File::create(extracted.join(item.name))?;
                let mut tick = Instant::now();
                let bytes = copy_verified(&mut response, &mut output, item, |bytes| {
                    if tick.elapsed() >= Duration::from_millis(300) {
                        progress(completed + bytes, Some(total));
                        tick = Instant::now();
                    }
                })?;
                output.sync_all()?;
                completed += bytes;
                progress(completed, Some(total));
            }
        } else {
            let url = format!(
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{}.tar.bz2",
                kind.directory()
            );
            let response = download(&client, &url, cancelled)?;
            let total = response.content_length();
            anyhow::ensure!(
                total.is_none_or(|size| size <= MAX_LEGACY_ARCHIVE_BYTES),
                "模型压缩包超过允许大小"
            );
            let mut response = CancellableReader {
                reader: response,
                cancelled,
            };
            let archive_path = staging.join("model.tar.bz2");
            let mut file = fs::File::create(&archive_path)?;
            let mut buf = [0u8; 64 * 1024];
            let mut bytes = 0;
            let mut last = Instant::now();
            progress(0, total);
            loop {
                let n = response.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes += n as u64;
                anyhow::ensure!(bytes <= MAX_LEGACY_ARCHIVE_BYTES, "模型压缩包超过允许大小");
                file.write_all(&buf[..n])?;
                if last.elapsed() >= Duration::from_millis(300) {
                    progress(bytes, total);
                    last = Instant::now();
                }
            }
            anyhow::ensure!(
                total.is_none_or(|size| size == bytes),
                "模型压缩包下载不完整"
            );
            file.sync_all()?;
            drop(file);
            progress(bytes, total);
            let mut archive =
                tar::Archive::new(bzip2::read::BzDecoder::new(fs::File::open(archive_path)?));
            let mut installed = std::collections::BTreeSet::new();
            // Only install known regular model files; skip archive links and metadata.
            for entry in archive.entries()? {
                ensure_download_active(cancelled)?;
                let entry = entry?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let path = entry.path()?.into_owned();
                if path.parent() != Some(Path::new(kind.directory())) {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if kind.files().contains(&name) {
                    anyhow::ensure!(
                        installed.insert(name.to_owned()),
                        "模型压缩包含重复文件：{name}"
                    );
                    let max_bytes = if name == "tokens.txt" {
                        MAX_TOKENS_BYTES
                    } else {
                        MAX_LEGACY_WEIGHT_BYTES
                    };
                    let expected = entry.size();
                    anyhow::ensure!(
                        expected > 0 && expected <= max_bytes,
                        "模型文件大小异常：{name}"
                    );
                    let mut output = fs::File::create(extracted.join(name))?;
                    let copied = std::io::copy(
                        &mut CancellableReader {
                            reader: entry,
                            cancelled,
                        }
                        .take(max_bytes + 1),
                        &mut output,
                    )?;
                    anyhow::ensure!(
                        copied == expected && copied <= max_bytes,
                        "模型文件不完整或过大：{name}"
                    );
                    output.sync_all()?;
                }
            }
        }
        if !kind.is_streaming() {
            ensure_download_active(cancelled)?;
            let existing = [
                ModelKind::Offline,
                ModelKind::ParaformerSmall,
                ModelKind::ParaformerLarge,
            ]
            .into_iter()
            .map(|model| model.path(root).join("silero_vad.onnx"))
            .find(|path| {
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_VAD_BYTES)
            });
            if let Some(existing) = existing {
                let mut source = CancellableReader {
                    reader: fs::File::open(existing)?,
                    cancelled,
                };
                let mut output = fs::File::create(extracted.join("silero_vad.onnx"))?;
                let bytes = std::io::copy(&mut (&mut source).take(MAX_VAD_BYTES + 1), &mut output)?;
                anyhow::ensure!(
                    bytes > 0 && bytes <= MAX_VAD_BYTES,
                    "已有 VAD 模型为空或超过允许大小"
                );
                output.sync_all()?;
            } else {
                let vad = download(
                    &client,
                    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
                    cancelled,
                )?;
                let mut file = fs::File::create(extracted.join("silero_vad.onnx"))?;
                let bytes = std::io::copy(
                    &mut CancellableReader {
                        reader: vad,
                        cancelled,
                    }
                    .take(MAX_VAD_BYTES + 1),
                    &mut file,
                )?;
                anyhow::ensure!(
                    bytes > 0 && bytes <= MAX_VAD_BYTES,
                    "VAD 模型文件为空或超过允许大小"
                );
                file.sync_all()?;
            }
        }
        ensure_download_active(cancelled)?;
        anyhow::ensure!(kind.ready(&staging), "下载包缺少模型文件");
        let _install_guard = model_install_guard(root)?;
        ensure_download_active(cancelled)?;
        recover_model_install(root, kind.directory())?;
        let dest = kind.path(root);
        if kind.ready(root) {
            return Ok(dest);
        }
        if model_path_exists(&dest)? {
            let backup = journal_model_replace(root, kind.directory(), nonce)?;
            opencap_core::rename_no_replace(&dest, &backup)
                .context("无法保留原有不完整模型目录")?;
            if let Err(error) = opencap_core::rename_no_replace(&extracted, &dest) {
                return match opencap_core::rename_no_replace(&backup, &dest) {
                    Ok(()) => Err(error).context("无法保存模型，原目录已恢复"),
                    Err(restore) => Err(error).context(format!(
                        "无法保存模型；原目录保留在 {}，恢复失败：{restore}",
                        backup.display()
                    )),
                };
            }
            opencap_core::sync_parent_directory(&dest)
                .context("模型已安装，但目录持久化确认失败")?;
            finish_model_replace(root, kind.directory())
                .context("模型已安装，但恢复日志清理失败")?;
            return Ok(dest);
        }
        opencap_core::rename_no_replace(&extracted, &dest)?;
        opencap_core::sync_parent_directory(&dest).context("模型已安装，但目录持久化确认失败")?;
        Ok(dest)
    })();
    let cleanup = fs::remove_dir_all(&staging);
    match (result, cleanup) {
        (Ok(path), Ok(())) => Ok(path),
        (Ok(_), Err(error)) => Err(error).context("模型已安装，但临时下载目录未能清理"),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("临时下载目录清理失败：{cleanup}")))
        }
    }
}
pub(crate) fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    cancelled: &AtomicBool,
) -> Result<reqwest::blocking::Response> {
    let mut error = None;
    for attempt in 0..3 {
        ensure_download_active(cancelled)?;
        match client
            .get(url)
            .send()
            .and_then(|response| response.error_for_status())
        {
            Ok(response) => {
                ensure_download_active(cancelled)?;
                return Ok(response);
            }
            Err(e) => error = Some(e),
        }
        if attempt < 2 {
            let until = Instant::now() + Duration::from_millis(500 * (attempt + 1));
            while Instant::now() < until {
                ensure_download_active(cancelled)?;
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(error.unwrap().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_download_rejects_corrupted_truncated_and_oversized_weights() {
        let item = RemoteFile {
            name: "test.onnx",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let mut output = Vec::new();
        assert_eq!(
            copy_verified(&mut &b"abc"[..], &mut output, &item, |_| {}).unwrap(),
            3
        );
        assert_eq!(output, b"abc");
        for mut input in [&b"abd"[..], &b"ab"[..], &b"abcd"[..]] {
            assert!(copy_verified(&mut input, &mut Vec::new(), &item, |_| {}).is_err());
        }
    }
    #[test]
    fn live_plan_reuses_offline_and_missing_optional_revision_never_blocks_streaming() {
        let root = std::env::temp_dir().join(format!(
            "opencap-model-plan-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        assert_eq!(LiveModelPlan::installed(&root, true), None);
        let install = |kind: ModelKind| {
            fs::create_dir_all(kind.path(&root)).unwrap();
            for name in kind.files() {
                fs::write(kind.path(&root).join(name), b"fixture").unwrap();
            }
        };
        install(ModelKind::Offline);
        assert_eq!(
            LiveModelPlan::installed(&root, true),
            Some(LiveModelPlan {
                kind: ModelKind::Offline,
                context_revision: false,
                revision_model: ModelKind::Offline,
            })
        );
        install(ModelKind::Streaming);
        assert_eq!(
            LiveModelPlan::installed(&root, true),
            Some(LiveModelPlan {
                kind: ModelKind::Streaming,
                context_revision: true,
                revision_model: ModelKind::Offline,
            })
        );
        assert!(
            !LiveModelPlan::installed(&root, false)
                .unwrap()
                .context_revision
        );
        fs::remove_file(ModelKind::Offline.path(&root).join("silero_vad.onnx")).unwrap();
        assert_eq!(
            LiveModelPlan::installed(&root, true),
            Some(LiveModelPlan {
                kind: ModelKind::Streaming,
                context_revision: false,
                revision_model: ModelKind::Offline,
            })
        );
        fs::write(ModelKind::Streaming.path(&root).join("tokens.txt"), []).unwrap();
        assert_eq!(LiveModelPlan::installed(&root, false), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_selection_never_silently_switches_models() {
        let mut available = ModelAvailability {
            streaming: true,
            offline: true,
            streaming_standard: false,
            streaming_enhanced: false,
            paraformer_small: false,
            paraformer_large: false,
            punctuation: false,
        };
        assert_eq!(
            available.live_plan_for(LiveAsrModel::Enhanced, OfflineAsrModel::SenseVoice, true),
            None
        );
        available.streaming_enhanced = true;
        let plan = available
            .live_plan_for(
                LiveAsrModel::Enhanced,
                OfflineAsrModel::ParaformerSmall,
                true,
            )
            .unwrap();
        assert_eq!(plan.kind, ModelKind::StreamingEnhanced);
        assert!(
            !plan.context_revision,
            "a missing selected correction model must not be substituted"
        );
        available.paraformer_small = true;
        let plan = available
            .live_plan_for(
                LiveAsrModel::Enhanced,
                OfflineAsrModel::ParaformerSmall,
                true,
            )
            .unwrap();
        assert!(plan.context_revision);
        assert_eq!(plan.revision_model, ModelKind::ParaformerSmall);
        let sentence = available
            .live_plan_for(
                LiveAsrModel::Sentence,
                OfflineAsrModel::ParaformerSmall,
                true,
            )
            .unwrap();
        assert_eq!(sentence.kind, ModelKind::ParaformerSmall);
        assert!(!sentence.context_revision);
        available.streaming = false;
        assert_eq!(
            available
                .live_plan_for(LiveAsrModel::Auto, OfflineAsrModel::ParaformerSmall, true)
                .unwrap()
                .kind,
            ModelKind::ParaformerSmall
        );
        assert_eq!(
            available.live_plan_for(LiveAsrModel::Compact, OfflineAsrModel::SenseVoice, false),
            None
        );
    }

    #[test]
    fn catalog_lists_only_required_files_with_pinned_sizes_and_hashes() {
        for kind in [
            ModelKind::StreamingStandard,
            ModelKind::StreamingEnhanced,
            ModelKind::ParaformerSmall,
            ModelKind::ParaformerLarge,
        ] {
            let remote = remote_model(kind).unwrap();
            assert_eq!(remote.revision.len(), 40);
            for file in remote.files {
                assert!(kind.files().contains(&file.name));
                assert!(file.size > 0);
                assert_eq!(file.sha256.len(), 64);
                assert!(file.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            }
            assert_eq!(
                remote.files.len(),
                kind.files().len() - usize::from(!kind.is_streaming())
            );
        }
    }
}
