//! Local punctuation restoration; recognized words are never intentionally rewritten.
use crate::models::{RemoteFile, copy_verified, download};
use anyhow::{Context, Result};
use opencap_core::TranscriptionLanguage;
use sherpa_onnx::{OfflinePunctuation, OfflinePunctuationConfig};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DIR: &str = "punctuation";
const MODEL: RemoteFile = RemoteFile {
    name: "model.int8.onnx",
    size: 75_519_198,
    sha256: "65a3fb9f5ad7bfb96bf69e0dc4481df97f6ee60513c1d94ce981ba6effd524b1",
};
const ARCHIVE: RemoteFile = RemoteFile {
    name: "punctuation.tar.bz2",
    size: 64_717_756,
    sha256: "c0d5aa5f8eeb686032345e180bedf39319dc2e0556781c6264bcadba8328a6e1",
};
const ARCHIVE_DIR: &str = "sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8";
const MIRROR: &str = "https://huggingface.co/lorneluo/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8/resolve/fc2be466e3c11927b306a31cdee23c9c38da44cc/model.int8.onnx";
const UPSTREAM: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/punctuation-models/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8.tar.bz2";

pub fn punctuation_ready(root: &Path) -> bool {
    root.join(DIR)
        .join(MODEL.name)
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() == MODEL.size)
}

/// Install only an independently verified official model or byte-identical mirror.
pub fn prepare_punctuation(
    root: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let dest = root.join(DIR);
    if punctuation_ready(root) {
        return Ok(dest);
    }
    fs::create_dir_all(root)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging = root.join(format!(
        ".punctuation-download-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<PathBuf> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()?;
        let model_path = staging.join(MODEL.name);
        let direct = (|| -> Result<()> {
            let mut response = download(&client, MIRROR)?;
            let mut output = fs::File::create(&model_path)?;
            progress(0, Some(MODEL.size));
            copy_verified(&mut response, &mut output, &MODEL, |bytes| {
                progress(bytes, Some(MODEL.size))
            })?;
            output.sync_all()?;
            Ok(())
        })();
        if direct.is_err() {
            let mut response = download(&client, UPSTREAM).context("下载中英标点模型失败")?;
            let archive_path = staging.join(ARCHIVE.name);
            let mut archive_file = fs::File::create(&archive_path)?;
            progress(0, Some(ARCHIVE.size));
            copy_verified(&mut response, &mut archive_file, &ARCHIVE, |bytes| {
                progress(bytes, Some(ARCHIVE.size))
            })?;
            archive_file.sync_all()?;
            let mut archive =
                tar::Archive::new(bzip2::read::BzDecoder::new(fs::File::open(archive_path)?));
            let mut found = false;
            for entry in archive.entries()? {
                let mut entry = entry?;
                if !entry.header().entry_type().is_file()
                    || entry.path()?.as_ref() != Path::new(ARCHIVE_DIR).join(MODEL.name)
                {
                    continue;
                }
                let mut output = fs::File::create(&model_path)?;
                copy_verified(&mut entry, &mut output, &MODEL, |_| {})?;
                output.sync_all()?;
                found = true;
                break;
            }
            anyhow::ensure!(found, "标点模型压缩包缺少模型文件");
        }
        let extracted = staging.join(DIR);
        fs::create_dir(&extracted)?;
        fs::rename(&model_path, extracted.join(MODEL.name))?;
        if punctuation_ready(root) {
            return Ok(dest);
        }
        if dest.exists() {
            let backup = root.join(format!(".incomplete-punctuation-{nonce}"));
            fs::rename(&dest, &backup).context("无法保留原有标点模型目录")?;
            if let Err(error) = fs::rename(&extracted, &dest) {
                let _ = fs::rename(&backup, &dest);
                return Err(error).context("无法保存标点模型");
            }
        } else {
            fs::rename(extracted, &dest)?;
        }
        Ok(dest)
    })();
    let _ = fs::remove_dir_all(staging);
    result
}

/// Use only one model thread and invoke it at a limited rate on the ASR worker.
pub(crate) struct Punctuator {
    model: Option<OfflinePunctuation>,
    enabled: bool,
    language: TranscriptionLanguage,
}
impl Punctuator {
    pub fn new(root: &Path, enabled: bool, language: TranscriptionLanguage) -> Self {
        let model = if enabled && punctuation_ready(root) {
            let mut config = OfflinePunctuationConfig::default();
            config.model.ct_transformer = Some(
                root.join(DIR)
                    .join(MODEL.name)
                    .to_string_lossy()
                    .into_owned(),
            );
            OfflinePunctuation::create(&config)
        } else {
            None
        };
        Self {
            model,
            enabled,
            language,
        }
    }
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }
    fn predicted(&self, text: &str) -> Option<String> {
        if !self.enabled || text.chars().count() > 200 || text.chars().any(is_punctuation) {
            return None;
        }
        let prediction = self.model.as_ref()?.add_punctuation(text)?;
        let mut output = merge_punctuation(text, &prediction)?;
        if self.language == TranscriptionLanguage::English
            || (!text.chars().any(is_cjk) && text.chars().any(|c| c.is_ascii_alphabetic()))
        {
            output = output
                .chars()
                .map(|c| match c {
                    '，' => ',',
                    '。' => '.',
                    '？' => '?',
                    '！' => '!',
                    '；' => ';',
                    '：' => ':',
                    other => other,
                })
                .collect();
        }
        Some(output)
    }
    pub fn finish(&self, text: &str) -> String {
        if !self.enabled || text.trim().is_empty() {
            return text.to_owned();
        }
        let mut result = self.predicted(text).unwrap_or_else(|| text.to_owned());
        if !result.trim_end().chars().last().is_some_and(is_punctuation) {
            result.push(
                if self.language == TranscriptionLanguage::English
                    || (!text.chars().any(is_cjk) && text.chars().any(|c| c.is_ascii_alphabetic()))
                {
                    '.'
                } else {
                    '。'
                },
            );
        }
        result
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x323af)
}
fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '？' | '！' | '；' | '：' | '、' | ',' | '.' | '?' | '!' | ';' | ':'
    )
}
/// Project only predicted marks back onto the exact recognized input. This
/// preserves whitespace, English word boundaries, numbers, and all other symbols.
fn merge_punctuation(input: &str, output: &str) -> Option<String> {
    let letters: Vec<_> = input.chars().filter(|c| !c.is_whitespace()).collect();
    let mut marks = vec![String::new(); letters.len() + 1];
    let mut at = 0;
    for c in output.chars() {
        if c.is_whitespace() {
            continue;
        }
        if is_punctuation(c) {
            if at == 0 {
                return None;
            }
            marks[at].push(c);
        } else {
            if letters.get(at) != Some(&c) {
                return None;
            }
            at += 1;
        }
    }
    if at != letters.len() {
        return None;
    }
    let mut result = String::with_capacity(input.len() + output.len().saturating_sub(input.len()));
    at = 0;
    for c in input.chars() {
        result.push(c);
        if !c.is_whitespace() {
            at += 1;
            result.push_str(&marks[at]);
        }
    }
    Some(result)
}

/// Keep already computed punctuation while newer hypotheses extend the same prefix.
#[derive(Default)]
pub(crate) struct PartialPunctuation {
    last: Option<Instant>,
    raw: String,
    rendered: String,
}
impl PartialPunctuation {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn render(&mut self, text: &str, punctuation: &Punctuator) -> String {
        if text.chars().count() >= 8
            && self
                .last
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            self.last = Some(Instant::now());
            if let Some(mut result) = punctuation.predicted(text) {
                while result.chars().last().is_some_and(is_punctuation) {
                    result.pop();
                }
                self.raw = text.to_owned();
                self.rendered = result;
            }
        }
        if !self.raw.is_empty()
            && let Some(suffix) = text.strip_prefix(&self.raw)
        {
            return format!("{}{suffix}", self.rendered);
        }
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_check_rejects_changed_or_missing_words() {
        assert_eq!(
            merge_punctuation("大家好  this is OpenCap", "大家好，this is OpenCap。"),
            Some("大家好，  this is OpenCap。".into())
        );
        assert!(merge_punctuation("预算一百万", "预算一千万。").is_none());
        assert!(merge_punctuation("金额-50", "金额50。").is_none());
    }
    #[test]
    fn fallback_adds_only_a_sentence_boundary_and_preserves_existing_marks() {
        let p = Punctuator::new(
            Path::new("/not-installed"),
            true,
            TranscriptionLanguage::Chinese,
        );
        assert_eq!(p.finish("大家好"), "大家好。");
        assert_eq!(p.finish("你好，张三？"), "你好，张三？");
        assert_eq!(p.finish("version 3.5"), "version 3.5.");
        let english = Punctuator::new(
            Path::new("/not-installed"),
            true,
            TranscriptionLanguage::English,
        );
        assert_eq!(english.finish("hello world"), "hello world.");
        let disabled = Punctuator::new(
            Path::new("/not-installed"),
            false,
            TranscriptionLanguage::Chinese,
        );
        assert_eq!(disabled.finish("大家好"), "大家好");
    }
    #[test]
    fn partial_cache_only_reuses_an_unchanged_prefix() {
        let mut cache = PartialPunctuation {
            last: Some(Instant::now()),
            raw: "大家好今天开会".into(),
            rendered: "大家好，今天开会".into(),
        };
        let p = Punctuator::new(
            Path::new("/not-installed"),
            true,
            TranscriptionLanguage::Chinese,
        );
        assert_eq!(cache.render("大家好今天开会议", &p), "大家好，今天开会议");
        assert_eq!(cache.render("大家好明天开会", &p), "大家好明天开会");
        cache.clear();
        assert_eq!(cache.render("大家好今天开会议", &p), "大家好今天开会议");
    }

    #[test]
    #[ignore = "requires verified local punctuation model in OPENCAP_PUNCT_TEST_ROOT"]
    fn installed_model_restores_chinese_and_english_without_changing_words() {
        let root = PathBuf::from(std::env::var_os("OPENCAP_PUNCT_TEST_ROOT").unwrap());
        let p = Punctuator::new(&root, true, TranscriptionLanguage::Chinese);
        assert!(p.has_model());
        let began = Instant::now();
        let chinese = p.finish("我们都是木头人不会说话不会动");
        assert!(chinese.contains('，'), "{chinese}");
        assert!(chinese.ends_with('。'), "{chinese}");
        assert_eq!(
            merge_punctuation("我们都是木头人不会说话不会动", &chinese),
            Some(chinese.clone())
        );
        let english = p.finish("how are you i am fine thank you");
        assert!(english.chars().any(is_punctuation), "{english}");
        assert_eq!(
            merge_punctuation("how are you i am fine thank you", &english),
            Some(english.clone())
        );
        eprintln!(
            "chinese={chinese}; english={english}; two calls={:?}",
            began.elapsed()
        );
    }
}
