//! Explicit local enrollment and conservative matching of clean, non-overlapping speech.
use crate::{SpeakerTurn, Transcript, speaker_models::EMBEDDING};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerName {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub name: String,
    #[serde(default)]
    pub matched: bool,
    #[serde(default)]
    pub profile_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Voiceprint {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub model: String,
    pub vector: Vec<f32>,
    pub voiced_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceProfile {
    pub id: String,
    pub name: String,
    samples: Vec<Voiceprint>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Default, Serialize, Deserialize)]
struct Memory {
    #[serde(default)]
    profiles: Vec<VoiceProfile>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
fn path(root: &Path) -> PathBuf {
    root.with_file_name("speaker-memory.json")
}
fn memory_write_guard(root: &Path) -> Result<std::fs::File> {
    let lock_path = root.with_file_name("speaker-memory.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    file.lock().context("无法锁定声纹记忆")?;
    Ok(file)
}
fn read(root: &Path) -> Result<Memory> {
    use std::io::Read;
    const LIMIT: u64 = 8_000_000;
    let path = path(root);
    let before = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Memory::default()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        before.is_file() && !before.file_type().is_symlink() && before.len() <= LIMIT,
        "声纹记忆不是普通文件或超过大小限制"
    );
    let file = opencap_core::open_regular_file_for_read(&path)?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let opened = file.metadata()?;
    anyhow::ensure!(
        opened.len() == before.len()
            && opened.modified()? == before.modified()?
            && identity
                == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(&path)?)?,
        "声纹记忆读取前发生变化"
    );
    let mut bytes = Vec::new();
    (&file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        bytes.len() as u64 <= LIMIT
            && current.is_file()
            && !current.file_type().is_symlink()
            && opened.len() == bytes.len() as u64
            && opened.len() == after.len()
            && opened.modified()? == after.modified()?
            && after.len() == current.len()
            && after.modified()? == current.modified()?
            && identity
                == same_file::Handle::from_file(opencap_core::open_regular_file_for_read(&path)?)?,
        "声纹记忆读取期间发生变化"
    );
    let memory: Memory = serde_json::from_slice(&bytes).context("声纹记忆格式无效")?;
    let mut ids = std::collections::BTreeSet::new();
    anyhow::ensure!(
        memory
            .profiles
            .iter()
            .all(|profile| !profile.id.is_empty() && ids.insert(&profile.id)),
        "声纹档案身份为空或重复；原文件保留"
    );
    Ok(memory)
}
fn write(root: &Path, memory: &Memory) -> Result<()> {
    let path = path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(memory)?;
    anyhow::ensure!(
        bytes.len() <= 8_000_000,
        "声纹记忆已满，请先忘记不再使用的声音"
    );
    let parent = path.parent().context("声纹记忆目录无效")?;
    let temporary = parent.join(format!(
        ".opencap-voice-{}.tmp",
        opencap_core::new_stable_id()
    ));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        opencap_core::sync_parent_directory(&path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
pub fn voice_profiles(root: &Path) -> Result<Vec<VoiceProfile>> {
    Ok(read(root)?.profiles)
}
pub fn forget_voice(root: &Path, id: &str) -> Result<()> {
    let _guard = memory_write_guard(root)?;
    let mut memory = read(root)?;
    memory.profiles.retain(|p| p.id != id);
    write(root, &memory)
}
fn normalize(vector: &mut [f32]) -> bool {
    if vector.is_empty() || vector.iter().any(|v| !v.is_finite()) {
        return false;
    }
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-6 {
        return false;
    }
    for value in vector {
        *value /= norm;
    }
    true
}
fn similarity(a: &Voiceprint, b: &Voiceprint) -> Option<f32> {
    if a.model != b.model
        || a.vector.len() != b.vector.len()
        || a.voiced_ms < 3000
        || b.voiced_ms < 3000
    {
        return None;
    }
    let mut av = a.vector.clone();
    let mut bv = b.vector.clone();
    if !normalize(&mut av) || !normalize(&mut bv) {
        return None;
    }
    Some(av.iter().zip(bv).map(|(a, b)| a * b).sum())
}
/// A conservative heuristic, not a calibrated probability or identity guarantee.
fn candidates(print: &Voiceprint, memory: &Memory) -> Vec<(usize, f32)> {
    let mut scores: Vec<_> = memory
        .profiles
        .iter()
        .enumerate()
        .filter_map(|(index, p)| {
            p.samples
                .iter()
                .filter_map(|sample| similarity(print, sample))
                .max_by(f32::total_cmp)
                .map(|score| (index, score))
        })
        .collect();
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    scores
}
#[cfg(test)]
fn apply_matches(transcript: &mut Transcript, memory: &Memory) {
    apply_matches_for_model(transcript, memory, None);
}
fn apply_matches_for_model(transcript: &mut Transcript, memory: &Memory, model: Option<&str>) {
    let mut proposed = Vec::new();
    for (&speaker, print) in &transcript.speaker_voiceprints {
        if model.is_some_and(|expected| print.model != expected) {
            continue;
        }
        let scores = candidates(print, memory);
        if let Some(&(index, score)) = scores.first()
            && score >= 0.72
            && scores
                .get(1)
                .is_none_or(|(_, second)| score - second >= 0.10)
        {
            proposed.push((speaker, index));
        }
    }
    // Two groups competing for one profile stay unassigned rather than merging people.
    for &(speaker, index) in &proposed {
        if proposed.iter().filter(|(_, other)| *other == index).count() != 1 {
            continue;
        }
        let profile = &memory.profiles[index];
        // An existing manual or previously preserved name may have no usable
        // voiceprint in this pass. It still reserves its explicit profile;
        // do not silently assign the same identity to a different group.
        if transcript.speaker_names.iter().any(|(&other, name)| {
            other != speaker && name.profile_id.as_deref() == Some(profile.id.as_str())
        }) {
            continue;
        }
        transcript
            .speaker_names
            .entry(speaker)
            .or_insert_with(|| SpeakerName {
                extra: Default::default(),
                name: profile.name.clone(),
                matched: true,
                profile_id: Some(profile.id.clone()),
            });
    }
}
pub fn name_speaker(
    transcript: &mut Transcript,
    root: &Path,
    speaker: u32,
    name: &str,
    remember: bool,
) -> Result<()> {
    anyhow::ensure!(
        transcript
            .speaker_turns
            .iter()
            .any(|t| t.speaker == speaker),
        "说话人不存在"
    );
    let name = name.trim();
    anyhow::ensure!(name.chars().count() <= 80, "姓名过长");
    anyhow::ensure!(!remember || !name.is_empty(), "请先填写姓名，再记住声音");
    anyhow::ensure!(
        !name.chars().any(char::is_control),
        "姓名不能包含换行或控制字符"
    );
    if name.is_empty() {
        transcript.speaker_names.remove(&speaker);
        return Ok(());
    }
    let mut profile_id = None;
    if remember {
        let mut print = transcript
            .speaker_voiceprints
            .get(&speaker)
            .context("没有足够的独立发言声纹；请重新分析或仅保存本场姓名")?
            .clone();
        anyhow::ensure!(
            print.voiced_ms >= 3000 && normalize(&mut print.vector),
            "独立发言不足 3 秒，暂不能记住声音"
        );
        let _guard = memory_write_guard(root)?;
        let mut memory = read(root)?;
        let existing = memory.profiles.iter().position(|p| p.name == name);
        let index = existing.unwrap_or_else(|| {
            let id = opencap_core::new_stable_id();
            memory.profiles.push(VoiceProfile {
                id,
                name: name.into(),
                samples: Vec::new(),
                extra: BTreeMap::new(),
            });
            memory.profiles.len() - 1
        });
        let profile = &mut memory.profiles[index];
        let duplicate = profile.samples.iter().any(|sample| {
            sample.model == print.model
                && sample.voiced_ms == print.voiced_ms
                && sample.vector == print.vector
        });
        if !duplicate {
            if profile.samples.len() >= 5 {
                profile.samples.remove(0);
            }
            profile.samples.push(print);
        }
        profile_id = Some(profile.id.clone());
        write(root, &memory)?;
    }
    let extra = transcript
        .speaker_names
        .get(&speaker)
        .map(|old| old.extra.clone())
        .unwrap_or_default();
    transcript.speaker_names.insert(
        speaker,
        SpeakerName {
            extra,
            name: name.into(),
            matched: false,
            profile_id,
        },
    );
    Ok(())
}
/// Sweep overlap boundaries once; overlapping speakers are never enrolled.
fn clean_regions(turns: &[SpeakerTurn]) -> BTreeMap<u32, Vec<(u64, u64)>> {
    let mut events = Vec::with_capacity(turns.len() * 2);
    for t in turns.iter().filter(|t| t.end_ms > t.start_ms) {
        events.push((t.start_ms, t.speaker, 1i32));
        events.push((t.end_ms, t.speaker, -1));
    }
    events.sort_unstable();
    let mut active = BTreeMap::<u32, i32>::new();
    let mut regions = BTreeMap::<u32, Vec<(u64, u64)>>::new();
    let mut previous = 0;
    for (time, speaker, delta) in events {
        if time > previous && active.len() == 1 {
            let id = *active.keys().next().unwrap();
            let list = regions.entry(id).or_default();
            if let Some(last) = list.last_mut().filter(|last| last.1 == previous) {
                last.1 = time;
            } else {
                list.push((previous, time));
            }
        }
        *active.entry(speaker).or_default() += delta;
        active.retain(|_, count| *count > 0);
        previous = time;
    }
    regions
}
pub(crate) fn extract_and_match(
    transcript: &mut Transcript,
    samples: &[f32],
    root: &Path,
    threads: usize,
    cancelled: &AtomicBool,
) -> Result<()> {
    let model = root.join("diarization").join(EMBEDDING);
    let fingerprint =
        opencap_core::fingerprint_media(&model, Some(cancelled)).context("无法核对声纹模型")?;
    let identity = format!("{EMBEDDING}:sha256:{}", fingerprint.sha256);
    let mut extracted = BTreeMap::new();
    let extractor = SpeakerEmbeddingExtractor::create(&SpeakerEmbeddingExtractorConfig {
        model: Some(model.to_string_lossy().into()),
        num_threads: threads as i32,
        ..Default::default()
    })
    .context("声纹提取模型加载失败")?;
    for (speaker, mut regions) in clean_regions(&transcript.speaker_turns) {
        anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "已取消声纹提取");
        regions.sort_by_key(|(start, end)| std::cmp::Reverse(end - start));
        let mut voice = Vec::new();
        for (start, end) in regions {
            let start = ((start + 120) * 16) as usize;
            let end = (end.saturating_sub(120) * 16) as usize;
            if end <= start {
                continue;
            }
            let part = &samples[start.min(samples.len())..end.min(samples.len())];
            if part.is_empty()
                || part.iter().any(|v| !v.is_finite())
                || part.iter().map(|v| v * v).sum::<f32>() / (part.len() as f32) < 0.000009
            {
                continue;
            }
            let count = part.len().min((16000 * 8usize).saturating_sub(voice.len()));
            voice.extend_from_slice(&part[..count]);
            if voice.len() >= 16000 * 8 {
                break;
            }
        }
        if voice.len() < 16000 * 3 {
            continue;
        }
        let stream = extractor.create_stream().context("创建声纹流失败")?;
        stream.accept_waveform(16000, &voice);
        stream.input_finished();
        if !extractor.is_ready(&stream) {
            continue;
        }
        if let Some(mut vector) = extractor.compute(&stream)
            && normalize(&mut vector)
        {
            extracted.insert(
                speaker,
                Voiceprint {
                    extra: Default::default(),
                    model: identity.clone(),
                    vector,
                    voiced_ms: voice.len() as u64 / 16,
                },
            );
        }
    }
    anyhow::ensure!(
        opencap_core::fingerprint_media(&model, Some(cancelled))? == fingerprint,
        "声纹模型在分析期间发生变化，未应用声纹匹配"
    );
    let memory = read(root)?;
    transcript.speaker_voiceprints.extend(extracted);
    apply_matches_for_model(transcript, &memory, Some(&identity));
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn print(v: &[f32]) -> Voiceprint {
        Voiceprint {
            extra: Default::default(),
            model: "test-v1".into(),
            vector: v.into(),
            voiced_ms: 4000,
        }
    }
    #[test]
    fn enrollment_roundtrip_matching_ambiguity_model_mismatch_and_forgetting() {
        let dir = std::env::temp_dir().join(format!("opencap-voice-{}", std::process::id()));
        let root = dir.join("models");
        std::fs::create_dir_all(&root).unwrap();
        let mut t = Transcript {
            speaker_turns: vec![SpeakerTurn {
                extra: Default::default(),
                start_ms: 0,
                end_ms: 4000,
                speaker: 1,
            }],
            ..Default::default()
        };
        t.speaker_voiceprints.insert(1, print(&[1., 0., 0.]));
        name_speaker(&mut t, &root, 1, "仅本场", false).unwrap();
        assert!(!path(&root).exists());
        name_speaker(&mut t, &root, 1, "张三", true).unwrap();
        let memory = read(&root).unwrap();
        assert_eq!(memory.profiles[0].name, "张三");
        t.speaker_names.clear();
        apply_matches(&mut t, &memory);
        assert_eq!(t.speaker_names[&1].name, "张三");
        assert!(t.speaker_names[&1].matched);
        t.speaker_names.clear();
        t.speaker_voiceprints.insert(2, print(&[0.99, 0.01, 0.]));
        apply_matches(&mut t, &memory);
        assert!(t.speaker_names.is_empty());
        t.speaker_voiceprints.remove(&2);
        let mut ambiguous = Memory {
            profiles: memory.profiles.clone(),
            extra: BTreeMap::new(),
        };
        let mut other = ambiguous.profiles[0].clone();
        other.id = "different".into();
        other.name = "李四".into();
        ambiguous.profiles.push(other);
        apply_matches(&mut t, &ambiguous);
        assert!(t.speaker_names.is_empty());
        t.speaker_voiceprints.get_mut(&1).unwrap().model = "new-model".into();
        apply_matches(&mut t, &memory);
        assert!(t.speaker_names.is_empty());
        t.speaker_voiceprints.insert(1, print(&[0., 1., 0.]));
        apply_matches(&mut t, &memory);
        assert!(t.speaker_names.is_empty());
        t.speaker_voiceprints.get_mut(&1).unwrap().voiced_ms = 2999;
        assert!(name_speaker(&mut t, &root, 1, "短语音", true).is_err());
        forget_voice(&root, &memory.profiles[0].id).unwrap();
        assert!(voice_profiles(&root).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn overlapping_regions_are_excluded_and_adjacent_turns_are_kept() {
        let turn = |start_ms, end_ms, speaker| SpeakerTurn {
            extra: Default::default(),
            start_ms,
            end_ms,
            speaker,
        };
        let regions = clean_regions(&[turn(0, 5000, 1), turn(1000, 3000, 2), turn(5000, 8000, 2)]);
        assert_eq!(regions[&1], vec![(0, 1000), (3000, 5000)]);
        assert_eq!(regions[&2], vec![(5000, 8000)]);
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires installed local speaker model and synthetic OPENCAP_SPEAKER_TEST_TRANSCRIPT"]
    fn installed_model_enrollment_and_matching_use_isolated_local_memory() {
        let input = std::env::var_os("OPENCAP_SPEAKER_TEST_TRANSCRIPT")
            .expect("synthetic transcript required");
        let mut t: Transcript = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
        let source = t.source.as_ref().unwrap();
        let source = if source.is_absolute() {
            source.clone()
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(source)
        }
        .canonicalize()
        .unwrap();
        let mut samples = Vec::new();
        crate::audio::decode_file(&source, |packet, _| {
            let start = packet.pts_us.max(0) as usize * 16000 / 1_000_000;
            samples.resize(samples.len().max(start + packet.samples.len()), 0.);
            samples[start..start + packet.samples.len()].copy_from_slice(&packet.samples);
            Ok(())
        })
        .unwrap();
        let dir =
            std::env::temp_dir().join(format!("opencap-speaker-model-test-{}", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let _cleanup = Cleanup(dir.clone());
        let root = dir.join("models");
        std::os::unix::fs::symlink(
            crate::model_root().join("diarization"),
            root.join("diarization"),
        )
        .unwrap();
        t.speaker_names.clear();
        t.speaker_voiceprints.clear();
        extract_and_match(&mut t, &samples, &root, 2, &AtomicBool::new(false)).unwrap();
        assert!(t.speaker_voiceprints.len() >= 2);
        let ids: Vec<_> = t.speaker_voiceprints.keys().copied().collect();
        for id in &ids {
            name_speaker(&mut t, &root, *id, &format!("测试说话人{id}"), true).unwrap();
        }
        t.speaker_names.clear();
        t.speaker_voiceprints.clear();
        extract_and_match(&mut t, &samples, &root, 2, &AtomicBool::new(false)).unwrap();
        for id in ids {
            assert_eq!(t.speaker_names[&id].name, format!("测试说话人{id}"));
            assert!(t.speaker_names[&id].matched);
        }
        eprintln!(
            "real_model_voices={} dimensions={} matched={}",
            t.speaker_voiceprints.len(),
            t.speaker_voiceprints.values().next().unwrap().vector.len(),
            t.speaker_names.len()
        );
    }
}
