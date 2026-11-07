use anyhow::{Context, Result};
use opencap_core::{LiveAsrModel, OfflineAsrModel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const DEFAULT_HOTWORDS: &str = "录屏,语音转录,会议纪要,实时字幕";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum ModelKind {
    Streaming,
    StreamingStandard,
    StreamingEnhanced,
    Offline,
    ParaformerSmall,
    ParaformerLarge,
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
    std::env::var_os("OPENCAP_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
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
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    if kind.ready(root) {
        return Ok(kind.path(root));
    }
    fs::create_dir_all(root)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging = root.join(format!(".download-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()?;
        let extracted = staging.join(kind.directory());
        fs::create_dir(&extracted)?;
        if let Some(remote) = remote_model(kind) {
            let total: u64 = remote.files.iter().map(|f| f.size).sum();
            let mut completed = 0;
            progress(0, Some(total));
            for item in remote.files {
                let url = format!(
                    "https://huggingface.co/{}/resolve/{}/{}",
                    remote.repo, remote.revision, item.name
                );
                let mut response =
                    download(&client, &url).with_context(|| format!("下载 {} 失败", item.name))?;
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
            let mut response = download(&client, &url)?;
            let total = response.content_length();
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
                file.write_all(&buf[..n])?;
                bytes += n as u64;
                if last.elapsed() >= Duration::from_millis(300) {
                    progress(bytes, total);
                    last = Instant::now();
                }
            }
            file.sync_all()?;
            progress(bytes, total);
            let mut archive =
                tar::Archive::new(bzip2::read::BzDecoder::new(fs::File::open(archive_path)?));
            // Only install known regular model files; skip archive links and metadata.
            for entry in archive.entries()? {
                let mut entry = entry?;
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
                    entry.unpack(extracted.join(name))?;
                }
            }
        }
        if !kind.is_streaming() {
            let existing = [
                ModelKind::Offline,
                ModelKind::ParaformerSmall,
                ModelKind::ParaformerLarge,
            ]
            .into_iter()
            .map(|model| model.path(root).join("silero_vad.onnx"))
            .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0));
            if let Some(existing) = existing {
                fs::copy(existing, extracted.join("silero_vad.onnx"))?;
            } else {
                let mut vad = download(
                    &client,
                    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
                )?;
                let mut file = fs::File::create(extracted.join("silero_vad.onnx"))?;
                std::io::copy(&mut vad, &mut file)?;
                file.sync_all()?;
            }
        }
        anyhow::ensure!(kind.ready(&staging), "下载包缺少模型文件");
        let dest = kind.path(root);
        if kind.ready(root) {
            return Ok(dest);
        }
        if dest.exists() {
            let backup = root.join(format!(".incomplete-{}-{nonce}", kind.directory()));
            fs::rename(&dest, &backup).context("无法保留原有不完整模型目录")?;
            if let Err(error) = fs::rename(&extracted, &dest) {
                let _ = fs::rename(&backup, &dest);
                return Err(error).context("无法保存模型");
            }
            return Ok(dest);
        }
        fs::rename(extracted, &dest)?;
        Ok(dest)
    })();
    let _ = fs::remove_dir_all(staging);
    result
}
pub(crate) fn download(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<reqwest::blocking::Response> {
    let mut error = None;
    for attempt in 0..3 {
        match client
            .get(url)
            .send()
            .and_then(|response| response.error_for_status())
        {
            Ok(response) => return Ok(response),
            Err(e) => error = Some(e),
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(500 * (attempt + 1)));
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
