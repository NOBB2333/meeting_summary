use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: u64,
    pub revision: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub finalized: bool,
    /// Endpoint reached; the independent sentence decoder may still replace it.
    #[serde(default)]
    pub correcting: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<crate::TranscriptCorrection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_duration_ms: Option<u64>,
    pub source: Option<PathBuf>,
    pub model: String,
    pub complete: bool,
    pub cancelled: bool,
    pub gaps: Vec<(u64, u64)>,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub speaker_turns: Vec<SpeakerTurn>,
    #[serde(default)]
    pub speaker_names: std::collections::BTreeMap<u32, crate::SpeakerName>,
    #[serde(default)]
    pub speaker_voiceprints: std::collections::BTreeMap<u32, crate::Voiceprint>,
    #[serde(default)]
    pub speaker_memory_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_analysis_seconds: Option<f64>,
    #[serde(default)]
    pub output_warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerTurn {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
}
impl Transcript {
    pub fn speaker_name(&self, id: u32) -> String {
        self.speaker_names
            .get(&id)
            .map(|n| {
                if n.matched {
                    format!("{}（声纹匹配待核对）", n.name)
                } else {
                    n.name.clone()
                }
            })
            .unwrap_or_else(|| format!("说话人 {id}"))
    }
    pub fn speaker_count(&self) -> usize {
        self.speaker_turns
            .iter()
            .map(|s| s.speaker)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }
    pub fn title(&self) -> String {
        self.source
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "会议录音".into())
    }
    /// Human-readable output, with explicit provenance for optional AI proofreading.
    pub fn document_markdown(&self) -> String {
        let mut text = format!("# {} · 逐字稿\n\n", escape_markdown(&self.title()));
        text.push_str(if self.correction.is_some() {
            "本稿经过 AI 全文语义校正，原始识别文字保留，可在应用内恢复。请核对重要内容。\n\n"
        } else if self.complete {
            "本稿为语音识别原文，未经过 AI 改写。\n\n"
        } else {
            "转录未完成，以下仅为已确认原文。\n\n"
        });
        if self.speaker_turns.is_empty() {
            text.push_str("说话人：尚未分析；可在应用内运行本地说话人分析。\n\n");
        }
        for s in self.segments.iter().filter(|s| s.finalized) {
            let label = self.speaker_label(s.start_ms, s.end_ms);
            let label = if label.is_empty() {
                "说话人未分析"
            } else {
                &label
            };
            text.push_str(&format!(
                "**{} · {} · 段{}**\n\n{}\n\n",
                escape_markdown(label),
                timestamp(s.start_ms, '.'),
                s.id,
                escape_markdown(&s.text)
            ));
        }
        text.push_str(&self.speaker_index_markdown());
        text
    }
    pub fn export_word(&self, base: &Path) -> Result<()> {
        crate::export_minutes_docx(
            &self.document_markdown(),
            &base.with_extension("transcript.docx"),
        )
    }
    pub fn speaker_index_markdown(&self) -> String {
        let mut people = std::collections::BTreeMap::<u32, (u64, u64)>::new();
        for t in &self.speaker_turns {
            let entry = people.entry(t.speaker).or_insert((t.start_ms, 0));
            entry.0 = entry.0.min(t.start_ms);
            entry.1 += t.end_ms.saturating_sub(t.start_ms);
        }
        if people.is_empty() {
            return String::new();
        }
        let mut md = String::from(
            "\n\n## 说话人索引（本地分析）\n\n未命名编号仅用于本场会议；姓名来自用户命名或本地声纹匹配，需核对。交替或重叠句子的归属需结合逐字稿核实。\n\n| 说话人 | 首次发言 | 累计发言 |\n| --- | --- | --- |\n",
        );
        for (speaker, (start, duration)) in people {
            md.push_str(&format!(
                "| {} | {} | {:.1} 秒 |\n",
                escape_markdown(&self.speaker_name(speaker)).replace('|', "\\|"),
                timestamp(start, '.'),
                duration as f64 / 1000.
            ));
        }
        md
    }
    /// Utterances can span turns. Without word timestamps we report ambiguity
    /// instead of silently assigning the entire sentence to one person.
    pub fn speaker_label(&self, start: u64, end: u64) -> String {
        if self.speaker_turns.is_empty() {
            return String::new();
        }
        let mut overlaps = std::collections::BTreeMap::<u32, u64>::new();
        for turn in &self.speaker_turns {
            let overlap = end
                .min(turn.end_ms)
                .saturating_sub(start.max(turn.start_ms));
            if overlap > 0 {
                *overlaps.entry(turn.speaker).or_default() += overlap;
            }
        }
        let total: u64 = overlaps.values().sum();
        if total == 0 {
            return "说话人未确定".into();
        }
        let labels = overlaps
            .iter()
            // A brief interruption inside a long ASR sentence still matters.
            // Keep substantial short turns rather than assigning all words to
            // the dominant speaker; we do not have word-level alignment.
            .filter(|(_, ms)| **ms >= 250 || **ms as f64 / total as f64 >= 0.15)
            .map(|(id, _)| self.speaker_name(*id))
            .collect::<Vec<_>>();
        if labels.len() == 1 {
            labels[0].clone()
        } else {
            format!("{}（交替或重叠）", labels.join(" / "))
        }
    }
    /// A hypothesis is a replacement for one utterance, never an appended delta.
    /// Late updates cannot roll back a final utterance or a newer revision.
    pub fn apply(&mut self, segment: Segment) -> bool {
        if let Some(old) = self.segments.iter_mut().find(|s| s.id == segment.id) {
            if old.finalized || old.revision >= segment.revision {
                return false;
            }
            *old = segment;
        } else {
            let index = self.segments.partition_point(|s| s.id < segment.id);
            self.segments.insert(index, segment);
        }
        true
    }
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .filter(|s| s.finalized)
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Lightweight recovery data. Full document generation belongs at completion/export.
    pub fn checkpoint(&self, base: &Path) -> Result<()> {
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(
            &base.with_extension("transcript.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn save(&self, base: &Path) -> Result<()> {
        let warnings = self.write_outputs(base);
        anyhow::ensure!(warnings.is_empty(), "{}", warnings.join("；"));
        Ok(())
    }
    /// Preserve completed analysis even when an external editor locks a derived file.
    /// Callers show these warnings and offer retry without rerunning inference.
    pub fn sync_outputs(&mut self, base: &Path) {
        self.output_warnings.clear();
        self.output_warnings = self.write_outputs(base);
        if let Err(error) = self.checkpoint(base) {
            let message = format!("转写数据未同步：{error:#}");
            if !self.output_warnings.contains(&message) {
                self.output_warnings.push(message);
            }
        }
    }
    fn write_outputs(&self, base: &Path) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Err(error) = self.checkpoint(base) {
            warnings.push(format!("转写数据未同步：{error:#}"));
        }
        let mut md = String::from("# 转录\n\n");
        if self.correction.is_some() {
            md.push_str("AI 全文语义校正稿；原始识别文字保留，可在应用内恢复。\n\n");
        }
        if !self.complete {
            md.push_str("转录尚未完整完成；原媒体保留，可重新转录。\n\n");
        }
        let mut srt = String::new();
        for (i, s) in self.segments.iter().filter(|s| s.finalized).enumerate() {
            md.push_str(&format!(
                "- [段{} {}–{} {}] {}\n",
                s.id,
                timestamp(s.start_ms, '.'),
                timestamp(s.end_ms, '.'),
                self.speaker_label(s.start_ms, s.end_ms),
                s.text
            ));
            srt.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                timestamp(s.start_ms, ','),
                timestamp(s.end_ms.max(s.start_ms + 1), ','),
                format!("{} {}", self.speaker_label(s.start_ms, s.end_ms), s.text).trim()
            ));
        }
        let mut plain = format!("{} · 逐字稿\n\n", self.title());
        if self.correction.is_some() {
            plain.push_str("AI 全文语义校正稿；原始识别文字保留，可在应用内恢复。\n\n");
        }
        for s in self.segments.iter().filter(|s| s.finalized) {
            let speaker = self.speaker_label(s.start_ms, s.end_ms);
            plain.push_str(&format!(
                "{} {}\n{}\n\n",
                if speaker.is_empty() {
                    "说话人未分析"
                } else {
                    &speaker
                },
                timestamp(s.start_ms, '.'),
                s.text
            ));
        }
        for (extension, bytes) in [
            ("transcript.md", md.as_bytes()),
            ("transcript.txt", plain.as_bytes()),
            ("srt", srt.as_bytes()),
        ] {
            if let Err(error) = atomic_write(&base.with_extension(extension), bytes) {
                warnings.push(format!(
                    "{extension} 未同步（可能正被其他应用占用）：{error:#}"
                ));
            }
        }
        if self.complete
            && let Err(error) = self.export_word(base)
        {
            warnings.push(format!("Word 未同步（可能正被其他应用占用）：{error:#}"));
        }
        warnings
    }
}
pub fn escape_markdown(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if "\\`*_{}[]<>#+-.!|".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    // Callers serialize writes per job; a distinct sibling is never a recording.
    let temporary = path.with_extension(format!(
        "{}.writing",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&temporary, bytes)?;
    // std::fs::rename replaces an existing file on Windows too. Never delete
    // the old artifact before the replacement has succeeded.
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}
pub fn timestamp(ms: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_blocked_word_export_keeps_analysis_and_syncs_other_outputs_then_retries() {
        let root =
            std::env::temp_dir().join(format!("opencap-speaker-sync-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let base = root.join("会议");
        let word = base.with_extension("transcript.docx");
        std::fs::create_dir_all(&word).unwrap(); // Inject a derived-file write failure.
        let mut transcript = Transcript {
            complete: true,
            segments: vec![s(1, "预算不能增加", true)],
            speaker_turns: vec![SpeakerTurn {
                start_ms: 0,
                end_ms: 5000,
                speaker: 1,
            }],
            ..Default::default()
        };
        transcript.speaker_names.insert(
            1,
            crate::SpeakerName {
                name: "张三".into(),
                matched: false,
                profile_id: None,
            },
        );
        transcript.sync_outputs(&base);
        assert_eq!(transcript.speaker_count(), 1);
        assert!(
            transcript
                .output_warnings
                .iter()
                .any(|w| w.contains("Word"))
        );
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert_eq!(saved.speaker_count(), 1);
        assert_eq!(saved.speaker_name(1), "张三");
        assert!(!saved.output_warnings.is_empty());
        for ext in ["transcript.txt", "transcript.md", "srt"] {
            let text = std::fs::read_to_string(base.with_extension(ext)).unwrap();
            assert!(
                text.contains("张三") && text.contains("预算不能增加"),
                "{ext}: {text}"
            );
        }
        std::fs::remove_dir(&word).unwrap();
        transcript.sync_outputs(&base);
        assert!(transcript.output_warnings.is_empty());
        assert!(word.is_file());
        let saved: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert!(saved.output_warnings.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn speaker_count_counts_groups_instead_of_the_largest_label() {
        let transcript = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: 1,
                },
                SpeakerTurn {
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: 3,
                },
                SpeakerTurn {
                    start_ms: 2000,
                    end_ms: 3000,
                    speaker: 3,
                },
            ],
            ..Default::default()
        };
        assert_eq!(transcript.speaker_count(), 2);
    }
    fn s(revision: u64, text: &str, finalized: bool) -> Segment {
        Segment {
            id: 1,
            revision,
            start_ms: 1200,
            end_ms: 2400,
            text: text.into(),
            finalized,
            correcting: false,
        }
    }
    #[test]
    fn context_replaces_wrong_character_without_duplicates() {
        let mut t = Transcript::default();
        assert!(t.apply(s(1, "工", false)));
        assert!(t.apply(s(2, "公司", false)));
        assert!(t.apply(s(3, "公司下周开会", true)));
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.text(), "公司下周开会");
        assert!(!t.apply(s(2, "公", false)));
        assert!(!t.apply(s(4, "错误迟到结果", false)));
    }
    #[test]
    fn unconfirmed_text_cannot_enter_minutes() {
        let mut t = Transcript::default();
        t.apply(s(1, "可能上线", false));
        assert!(t.text().is_empty());
        assert_eq!(timestamp(3_661_007, ','), "01:01:01,007");
    }
    #[test]
    fn existing_transcripts_load_without_a_correction_state() {
        let legacy = r#"{"id":1,"revision":2,"start_ms":0,"end_ms":1000,"text":"历史原文","finalized":true}"#;
        let segment: Segment = serde_json::from_str(legacy).unwrap();
        assert!(segment.finalized && !segment.correcting);
        let mut transcript = Transcript::default();
        transcript.apply(segment);
        assert_eq!(transcript.text(), "历史原文");
    }
    #[test]
    fn brief_interruption_is_marked_ambiguous_inside_a_long_sentence() {
        let t = Transcript {
            speaker_turns: vec![
                SpeakerTurn {
                    start_ms: 0,
                    end_ms: 4000,
                    speaker: 1,
                },
                SpeakerTurn {
                    start_ms: 4000,
                    end_ms: 4300,
                    speaker: 2,
                },
                SpeakerTurn {
                    start_ms: 4300,
                    end_ms: 10000,
                    speaker: 1,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            t.speaker_label(0, 10000),
            "说话人 1 / 说话人 2（交替或重叠）"
        );
    }
    #[test]
    fn completed_transcript_exports_separate_word_and_plain_text_without_ai() {
        let dir = std::env::temp_dir().join(format!(
            "opencap-transcript-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = dir.join("meeting");
        let mut transcript = Transcript::default();
        transcript.apply(s(1, "不能上线，预算 2 万元。", true));
        transcript.checkpoint(&base).unwrap();
        let restored: Transcript =
            serde_json::from_slice(&std::fs::read(base.with_extension("transcript.json")).unwrap())
                .unwrap();
        assert_eq!(restored.text(), transcript.text());
        assert!(!restored.complete);
        for extension in ["transcript.md", "transcript.txt", "srt", "transcript.docx"] {
            assert!(!base.with_extension(extension).exists());
        }
        transcript.save(&base).unwrap();
        assert!(!base.with_extension("transcript.docx").exists());
        transcript.complete = true;
        transcript.save(&base).unwrap();
        assert!(
            std::fs::read_to_string(base.with_extension("transcript.txt"))
                .unwrap()
                .contains("不能上线，预算 2 万元。")
        );
        assert!(
            std::fs::read(base.with_extension("transcript.docx"))
                .unwrap()
                .starts_with(b"PK")
        );
        assert!(!base.with_extension("summary.docx").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
