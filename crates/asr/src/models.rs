use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const DEFAULT_HOTWORDS: &str = "录屏,语音转录,会议纪要,实时字幕";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum ModelKind {
    Streaming,
    Offline,
}
impl ModelKind {
    pub fn directory(self) -> &'static str {
        match self {
            Self::Streaming => "sherpa-onnx-streaming-zipformer-zh-14M-2023-02-23",
            Self::Offline => "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
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
            Self::Offline => &["model.int8.onnx", "tokens.txt", "silero_vad.onnx"],
        }
    }
    pub fn path(self, root: &Path) -> PathBuf {
        root.join(self.directory())
    }
    pub fn ready(self, root: &Path) -> bool {
        self.files().iter().all(|name| {
            self.path(root)
                .join(name)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() > 0)
        })
    }
}
/// Reuse installed models without downloading in the recording hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveModelPlan {
    pub kind: ModelKind,
    pub context_revision: bool,
}
/// Cache this snapshot in the UI; filesystem checks do not belong in rendering.
#[derive(Debug, Clone, Copy)]
pub struct ModelAvailability {
    pub streaming: bool,
    pub offline: bool,
}
impl ModelAvailability {
    pub fn inspect(root: &Path) -> Self {
        Self {
            streaming: ModelKind::Streaming.ready(root),
            offline: ModelKind::Offline.ready(root),
        }
    }
    pub fn live_plan(self, revision: bool) -> Option<LiveModelPlan> {
        if self.streaming {
            Some(LiveModelPlan {
                kind: ModelKind::Streaming,
                context_revision: revision && self.offline,
            })
        } else if self.offline {
            Some(LiveModelPlan {
                kind: ModelKind::Offline,
                context_revision: false,
            })
        } else {
            None
        }
    }
}
impl LiveModelPlan {
    pub fn installed(root: &Path, revision: bool) -> Option<Self> {
        ModelAvailability::inspect(root).live_plan(revision)
    }
    pub fn description(self) -> &'static str {
        match (self.kind, self.context_revision) {
            (ModelKind::Offline, _) => "逐句模式 · 停顿后出字；下载流式模型可边说边显示",
            (ModelKind::Streaming, true) => "流式模式 · 边说边显示，停顿后后台校正",
            (ModelKind::Streaming, false) => "流式模式 · 边说边显示，当前句持续更新",
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
        let url = format!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{}.tar.bz2",
            kind.directory()
        );
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()?;
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
        // Only install the known regular model files; skip archive links and metadata.
        let extracted = staging.join(kind.directory());
        fs::create_dir(&extracted)?;
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
        if kind == ModelKind::Offline {
            let mut vad = download(
                &client,
                "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
            )?;
            let mut file = fs::File::create(extracted.join("silero_vad.onnx"))?;
            std::io::copy(&mut vad, &mut file)?;
            file.sync_all()?;
        }
        anyhow::ensure!(kind.ready(&staging), "下载包缺少模型文件");
        let dest = kind.path(root);
        if kind.ready(root) {
            return Ok(dest);
        }
        if dest.exists() {
            fs::remove_dir_all(&dest).context("无法替换不完整模型目录")?;
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
                context_revision: false
            })
        );
        install(ModelKind::Streaming);
        assert_eq!(
            LiveModelPlan::installed(&root, true),
            Some(LiveModelPlan {
                kind: ModelKind::Streaming,
                context_revision: true
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
                context_revision: false
            })
        );
        fs::write(ModelKind::Streaming.path(&root).join("tokens.txt"), []).unwrap();
        assert_eq!(LiveModelPlan::installed(&root, false), None);
        fs::remove_dir_all(root).unwrap();
    }
}
