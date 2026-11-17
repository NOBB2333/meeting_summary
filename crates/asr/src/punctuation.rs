//! Local punctuation restoration; recognized words are never intentionally rewritten.
use crate::models::{
    CancellableReader, RemoteFile, copy_verified, download, ensure_download_active,
};
use anyhow::{Context, Result};
use opencap_core::TranscriptionLanguage;
use sherpa_onnx::{OfflinePunctuation, OfflinePunctuationConfig};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{RwLock, atomic::AtomicBool},
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
pub fn punctuation_model_path(root: &Path) -> PathBuf {
    root.join(DIR).join(MODEL.name)
}
pub(crate) fn punctuation_reference_match(fingerprint: &opencap_core::MediaFingerprint) -> bool {
    fingerprint.size_bytes == MODEL.size && fingerprint.sha256 == MODEL.sha256
}
pub(crate) fn installed_model_verified(root: &Path, cancelled: &AtomicBool) -> Result<bool> {
    if !punctuation_ready(root) {
        return Ok(false);
    }
    let fingerprint =
        opencap_core::fingerprint_media(&punctuation_model_path(root), Some(cancelled))?;
    Ok(punctuation_reference_match(&fingerprint))
}

/// Install only an independently verified official model or byte-identical mirror.
pub fn prepare_punctuation(root: &Path, progress: impl FnMut(u64, Option<u64>)) -> Result<PathBuf> {
    prepare_punctuation_cancellable(root, &AtomicBool::new(false), progress)
}

pub fn prepare_punctuation_cancellable(
    root: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    ensure_download_active(cancelled)?;
    let dest = root.join(DIR);
    fs::create_dir_all(root)?;
    {
        let _guard = crate::models::model_install_guard(root)?;
        crate::models::recover_model_install(root, DIR)?;
        if installed_model_verified(root, cancelled)? {
            return Ok(dest);
        }
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging = root.join(format!(
        ".punctuation-download-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<PathBuf> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Blocking reads use this per-operation timeout; long downloads
            // can continue while progress is made, stalled reads terminate.
            .timeout(Duration::from_secs(60))
            .build()?;
        let model_path = staging.join(MODEL.name);
        let direct = (|| -> Result<()> {
            let response = download(&client, MIRROR, cancelled)?;
            let mut response = CancellableReader {
                reader: response,
                cancelled,
            };
            let mut output = fs::File::create(&model_path)?;
            progress(0, Some(MODEL.size));
            copy_verified(&mut response, &mut output, &MODEL, |bytes| {
                progress(bytes, Some(MODEL.size))
            })?;
            output.sync_all()?;
            Ok(())
        })();
        if direct.is_err() {
            ensure_download_active(cancelled)?;
            let response =
                download(&client, UPSTREAM, cancelled).context("下载中英标点模型失败")?;
            let mut response = CancellableReader {
                reader: response,
                cancelled,
            };
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
                ensure_download_active(cancelled)?;
                let entry = entry?;
                if !entry.header().entry_type().is_file()
                    || entry.path()?.as_ref() != Path::new(ARCHIVE_DIR).join(MODEL.name)
                {
                    continue;
                }
                let mut output = fs::File::create(&model_path)?;
                copy_verified(
                    &mut CancellableReader {
                        reader: entry,
                        cancelled,
                    },
                    &mut output,
                    &MODEL,
                    |_| {},
                )?;
                output.sync_all()?;
                found = true;
                break;
            }
            anyhow::ensure!(found, "标点模型压缩包缺少模型文件");
        }
        ensure_download_active(cancelled)?;
        let extracted = staging.join(DIR);
        fs::create_dir(&extracted)?;
        fs::rename(&model_path, extracted.join(MODEL.name))?;
        let _install_guard = crate::models::model_install_guard(root)?;
        ensure_download_active(cancelled)?;
        crate::models::recover_model_install(root, DIR)?;
        if installed_model_verified(root, cancelled)? {
            return Ok(dest);
        }
        if crate::models::model_path_exists(&dest)? {
            let backup = crate::models::journal_model_replace(root, DIR, nonce)?;
            opencap_core::rename_no_replace(&dest, &backup).context("无法保留原有标点模型目录")?;
            if let Err(error) = opencap_core::rename_no_replace(&extracted, &dest) {
                return match opencap_core::rename_no_replace(&backup, &dest) {
                    Ok(()) => Err(error).context("无法保存标点模型，原目录已恢复"),
                    Err(restore) => Err(error).context(format!(
                        "无法保存标点模型；原目录保留在 {}，恢复失败：{restore}",
                        backup.display()
                    )),
                };
            }
        } else {
            opencap_core::rename_no_replace(&extracted, &dest)?;
        }
        opencap_core::sync_parent_directory(&dest)
            .context("标点模型已安装，但目录持久化确认失败")?;
        if root.join(format!(".opencap-install-{DIR}.json")).exists() {
            crate::models::finish_model_replace(root, DIR)
                .context("标点模型已安装，但恢复日志清理失败")?;
        }
        Ok(dest)
    })();
    let cleanup = fs::remove_dir_all(&staging);
    match (result, cleanup) {
        (Ok(path), Ok(())) => Ok(path),
        (Ok(_), Err(error)) => Err(error).context("标点模型已安装，但临时下载目录未能清理"),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("临时下载目录清理失败：{cleanup}")))
        }
    }
}

/// Use only one model thread and invoke it at a limited rate on the ASR worker.
pub(crate) struct Punctuator {
    model: RwLock<Option<OfflinePunctuation>>,
    enabled: bool,
    language: TranscriptionLanguage,
}
impl Punctuator {
    #[cfg(test)]
    pub fn new(root: &Path, enabled: bool, language: TranscriptionLanguage) -> Self {
        Self::with_model_policy(root, enabled, language, true)
    }
    pub(crate) fn with_model_policy(
        root: &Path,
        enabled: bool,
        language: TranscriptionLanguage,
        allow_model: bool,
    ) -> Self {
        let model = if enabled && allow_model {
            Self::load_model(root)
        } else {
            None
        };
        Self {
            model: RwLock::new(model),
            enabled,
            language,
        }
    }
    pub fn has_model(&self) -> bool {
        self.model
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    pub(crate) fn load_model(root: &Path) -> Option<OfflinePunctuation> {
        if !punctuation_ready(root) {
            return None;
        }
        let mut config = OfflinePunctuationConfig::default();
        config.model.ct_transformer =
            Some(punctuation_model_path(root).to_string_lossy().into_owned());
        OfflinePunctuation::create(&config)
    }
    pub(crate) fn install_model(&self, model: OfflinePunctuation) {
        *self.model.write().unwrap_or_else(|e| e.into_inner()) = Some(model);
    }
    fn predicted(&self, text: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let guard = self.model.read().unwrap_or_else(|e| e.into_inner());
        let model = guard.as_ref()?;
        // The model is bounded to short contexts. Long uninterrupted speech
        // must still get punctuation instead of silently falling back.
        let mut output = String::new();
        let mut start = 0;
        for (offset, mark) in text.char_indices().filter(|(_, c)| is_punctuation(*c)) {
            output.push_str(&predict_span(model, &text[start..offset], true)?);
            output.push(mark);
            start = offset + mark.len_utf8();
        }
        output.push_str(&predict_span(model, &text[start..], false)?);
        if !text.chars().any(is_punctuation)
            && (self.language == TranscriptionLanguage::English
                || (!text.chars().any(is_cjk) && text.chars().any(|c| c.is_ascii_alphabetic())))
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

fn predict_span(
    model: &OfflinePunctuation,
    text: &str,
    before_existing_mark: bool,
) -> Option<String> {
    if text.trim().is_empty() {
        return Some(text.to_owned());
    }
    let mut output = String::new();
    let mut start = 0;
    for (index, (offset, _)) in text.char_indices().enumerate() {
        if index > 0 && index % 180 == 0 {
            let input = &text[start..offset];
            output.push_str(&merge_punctuation(input, &model.add_punctuation(input)?)?);
            start = offset;
        }
    }
    let input = &text[start..];
    if !input.trim().is_empty() {
        let predicted = merge_punctuation(input, &model.add_punctuation(input)?)?;
        if before_existing_mark {
            let trimmed = predicted.trim_end_matches(char::is_whitespace);
            output.push_str(trimmed.trim_end_matches(is_punctuation));
            output.push_str(&predicted[trimmed.len()..]);
        } else {
            output.push_str(&predicted);
        }
    } else {
        output.push_str(input);
    }
    Some(output)
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
    fn same_size_corrupt_cache_is_not_a_verified_model() {
        let root = std::env::temp_dir().join(format!(
            "opencap-punctuation-test-{}-{}",
            std::process::id(),
            opencap_core::new_stable_id()
        ));
        fs::create_dir_all(root.join(DIR)).unwrap();
        fs::File::create(punctuation_model_path(&root))
            .unwrap()
            .set_len(MODEL.size)
            .unwrap();
        assert!(punctuation_ready(&root));
        assert!(!installed_model_verified(&root, &AtomicBool::new(false)).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
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
        let with_existing = p.finish("我们都是木头人不会说话不会动。我们都是木头人不会说话不会动");
        assert!(with_existing.matches('，').count() >= 2, "{with_existing}");
        assert!(with_existing.contains('。'), "{with_existing}");
        let long = "我们都是木头人不会说话不会动".repeat(15);
        let long_result = p.finish(&long);
        assert!(long_result.contains('，'), "long input lost punctuation");
        assert_eq!(
            long_result
                .chars()
                .filter(|c| !is_punctuation(*c))
                .collect::<String>(),
            long
        );
        // A live session can begin before the network download finishes. The
        // shared punctuator must pick up the installed model without restarting.
        let late = std::sync::Arc::new(Punctuator::new(
            Path::new("/not-installed"),
            true,
            TranscriptionLanguage::Chinese,
        ));
        let observer = late.clone();
        assert!(!observer.has_model());
        late.install_model(Punctuator::load_model(&root).unwrap());
        assert!(
            observer
                .finish("我们都是木头人不会说话不会动")
                .contains('，')
        );
        eprintln!(
            "chinese={chinese}; english={english}; two calls={:?}",
            began.elapsed()
        );
    }
}
