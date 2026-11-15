//! Session-scoped identities for a stateful online diarizer. Backend slots are
//! provisional anonymous labels, not people and not audio input channels.
use crate::{OnlineSpeakerInterval, OnlineSpeakerSnapshot, SpeakerName, SpeakerTurn, Transcript};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineIdentity {
    pub id: u32,
    pub first_seen_ms: u64,
    pub name: Option<String>,
    pub user_locked: bool,
    pub confirmed: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpeakerEdit {
    Name {
        revision: u64,
        speaker: u32,
        name: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
    Confirm {
        revision: u64,
        speaker: u32,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
    Merge {
        revision: u64,
        from: u32,
        into: u32,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
    Split {
        revision: u64,
        speaker: u32,
        new_speaker: u32,
        at_ms: u64,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineSpeakerRegistry {
    pub schema_version: u32,
    pub session_id: String,
    pub run_id: Option<String>,
    #[serde(default)]
    pub model_sha256: Option<String>,
    #[serde(default)]
    pub library_sha256: Option<String>,
    pub capacity: usize,
    pub revision: u64,
    pub backend_revision: u64,
    pub input_end_ms: u64,
    pub finished: bool,
    /// The backend has emitted its terminal snapshot; input verification can
    /// remain pending independently of this immutable stream boundary.
    #[serde(default)]
    pub backend_final: bool,
    pub incomplete_reason: Option<String>,
    pub identities: BTreeMap<u32, OnlineIdentity>,
    pub edits: Vec<SpeakerEdit>,
    slots: BTreeMap<u32, u32>,
    intervals: Vec<OnlineSpeakerInterval>,
    next_id: u32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl OnlineSpeakerRegistry {
    /// Number of saved backend intervals, before presentation conversion.
    pub fn interval_count(&self) -> usize {
        self.intervals.len()
    }

    pub fn new(capacity: usize, run_id: Option<String>) -> Result<Self> {
        ensure!((1..=128).contains(&capacity), "说话人模型容量无效");
        ensure!(
            run_id.as_deref().is_none_or(|id| !id.trim().is_empty()),
            "识别运行身份为空"
        );
        Ok(Self {
            schema_version: 1,
            session_id: opencap_core::new_stable_id(),
            run_id,
            capacity,
            model_sha256: None,
            library_sha256: None,
            // The initial session metadata is itself a checkpointable state,
            // even before the first diarization interval arrives.
            revision: 1,
            backend_revision: 0,
            input_end_ms: 0,
            finished: false,
            backend_final: false,
            incomplete_reason: None,
            identities: BTreeMap::new(),
            edits: Vec::new(),
            slots: BTreeMap::new(),
            intervals: Vec::new(),
            next_id: 1,
            extra: BTreeMap::new(),
        })
    }

    pub fn bind_run(&mut self, run_id: &str) -> Result<()> {
        ensure!(!run_id.trim().is_empty(), "识别运行身份为空");
        if let Some(existing) = &self.run_id {
            ensure!(existing == run_id, "说话人结果属于其他识别运行");
        } else {
            self.run_id = Some(run_id.to_owned());
            // Binding is persisted state even if no speaker interval has
            // arrived yet. Advance the revision so an earlier unbound
            // recovery copy cannot be mistaken for the bound checkpoint.
            self.revision = self.revision.saturating_add(1);
        }
        Ok(())
    }

    fn allocate(&mut self, first_seen_ms: u64) -> Result<u32> {
        let id = self.next_id;
        ensure!(
            id > 0 && !self.identities.contains_key(&id),
            "说话人编号冲突；检查点保留，未覆盖身份"
        );
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("说话人编号已用完"))?;
        self.identities.insert(
            id,
            OnlineIdentity {
                id,
                first_seen_ms,
                name: None,
                user_locked: false,
                confirmed: false,
                extra: BTreeMap::new(),
            },
        );
        Ok(id)
    }

    /// A snapshot replaces only this backend's view. Old or out-of-order
    /// revisions cannot overwrite newer evidence or any manual edit.
    pub fn apply_snapshot(&mut self, mut snapshot: OnlineSpeakerSnapshot) -> Result<bool> {
        if self.finished || self.backend_final || snapshot.revision <= self.backend_revision {
            return Ok(false);
        }
        ensure!(
            snapshot.input_end_ms >= self.input_end_ms,
            "说话人输入时间回退"
        );
        ensure!(snapshot.intervals.len() <= 100_000, "说话人区间过多");
        for interval in &snapshot.intervals {
            ensure!(
                interval.slot > 0 && interval.slot as usize <= self.capacity,
                "说话人槽超出模型容量"
            );
            ensure!(
                interval.start_ms < interval.end_ms
                    && interval.end_ms <= snapshot.input_end_ms.saturating_add(1000),
                "说话人区间超出已处理媒体时间"
            );
        }
        let new_slots = snapshot
            .intervals
            .iter()
            .map(|interval| interval.slot)
            .filter(|slot| !self.slots.contains_key(slot))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let new_slots =
            u32::try_from(new_slots).map_err(|_| anyhow::anyhow!("说话人槽数量无效"))?;
        let end = self
            .next_id
            .checked_add(new_slots)
            .ok_or_else(|| anyhow::anyhow!("说话人编号已用完"))?;
        ensure!(
            self.next_id > 0 && (self.next_id..end).all(|id| !self.identities.contains_key(&id)),
            "说话人编号冲突；快照未应用"
        );
        let mut first = BTreeMap::<u32, u64>::new();
        for interval in &snapshot.intervals {
            first
                .entry(interval.slot)
                .and_modify(|time| *time = (*time).min(interval.start_ms))
                .or_insert(interval.start_ms);
        }
        for (slot, start) in first {
            if !self.slots.contains_key(&slot) {
                let id = self.allocate(start)?;
                self.slots.insert(slot, id);
            }
        }
        // Carry extension fields only where the old and new interval both
        // identify a unique, exactly matching slot and time range.
        let mut old = BTreeMap::new();
        for interval in &self.intervals {
            let key = (interval.slot, interval.start_ms, interval.end_ms);
            old.entry(key)
                .and_modify(|value: &mut Option<_>| *value = None)
                .or_insert_with(|| Some(interval.extra.clone()));
        }
        let mut counts = BTreeMap::<_, usize>::new();
        for interval in &snapshot.intervals {
            *counts
                .entry((interval.slot, interval.start_ms, interval.end_ms))
                .or_default() += 1;
        }
        for interval in &mut snapshot.intervals {
            let key = (interval.slot, interval.start_ms, interval.end_ms);
            if counts.get(&key) == Some(&1)
                && let Some(Some(extra)) = old.get(&key)
            {
                for (name, value) in extra {
                    interval
                        .extra
                        .entry(name.clone())
                        .or_insert_with(|| value.clone());
                }
            }
        }
        self.backend_revision = snapshot.revision;
        self.input_end_ms = snapshot.input_end_ms;
        self.finished = snapshot.finished;
        self.backend_final = snapshot.finished;
        self.intervals = snapshot.intervals;
        self.revision = self.revision.saturating_add(1);
        Ok(true)
    }

    /// Seal a terminal snapshot whose overall completion is still awaiting
    /// file-end or input-identity verification.
    pub fn seal_backend(&mut self) {
        if !self.backend_final {
            self.backend_final = true;
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn next_edit(&mut self) -> Result<u64> {
        ensure!(self.edits.len() < 512, "说话人修订记录过多");
        self.revision = self.revision.saturating_add(1);
        Ok(self.revision)
    }
    pub fn rename(&mut self, speaker: u32, name: &str) -> Result<()> {
        let name = name.trim();
        ensure!(
            name.chars().count() <= 80 && !name.chars().any(char::is_control),
            "姓名须不超过 80 字且不含控制字符"
        );
        ensure!(self.identities.contains_key(&speaker), "说话人不存在");
        let revision = self.next_edit()?;
        let name = (!name.is_empty()).then(|| name.to_owned());
        let identity = self.identities.get_mut(&speaker).unwrap();
        identity.name = name.clone();
        identity.user_locked = true;
        identity.confirmed = name.is_some();
        self.edits.push(SpeakerEdit::Name {
            revision,
            speaker,
            name,
            extra: BTreeMap::new(),
        });
        Ok(())
    }
    pub fn confirm(&mut self, speaker: u32) -> Result<()> {
        ensure!(self.identities.contains_key(&speaker), "说话人不存在");
        let revision = self.next_edit()?;
        self.identities.get_mut(&speaker).unwrap().confirmed = true;
        self.edits.push(SpeakerEdit::Confirm {
            revision,
            speaker,
            extra: BTreeMap::new(),
        });
        Ok(())
    }
    pub fn merge(&mut self, from: u32, into: u32) -> Result<()> {
        ensure!(
            from != into
                && self.identities.contains_key(&from)
                && self.identities.contains_key(&into),
            "请选择两位不同的说话人"
        );
        let turns = self.turns();
        ensure!(
            turns.iter().any(|turn| turn.speaker == from)
                && turns.iter().any(|turn| turn.speaker == into),
            "所选说话人已合并或没有活动区间"
        );
        let a = self
            .identities
            .get(&from)
            .and_then(|identity| identity.name.as_deref());
        let b = self
            .identities
            .get(&into)
            .and_then(|identity| identity.name.as_deref());
        ensure!(
            a.is_none() || b.is_none() || a == b,
            "两位说话人已有不同姓名；请先明确保留的姓名"
        );
        let inherited = a.map(str::to_owned);
        let target_empty = b.is_none();
        let revision = self.next_edit()?;
        if target_empty && inherited.is_some() {
            let target = self.identities.get_mut(&into).unwrap();
            target.name = inherited;
            target.user_locked = true;
        }
        self.edits.push(SpeakerEdit::Merge {
            revision,
            from,
            into,
            extra: BTreeMap::new(),
        });
        Ok(())
    }
    pub fn split(&mut self, speaker: u32, at_ms: u64) -> Result<u32> {
        ensure!(self.edits.len() < 512, "说话人修订记录过多");
        let turns = self.turns();
        ensure!(
            turns
                .iter()
                .any(|turn| turn.speaker == speaker && turn.start_ms < at_ms)
                && turns
                    .iter()
                    .any(|turn| turn.speaker == speaker && turn.end_ms > at_ms),
            "拆分点两侧都需要已有发言"
        );
        let new_speaker = self.allocate(at_ms)?;
        let revision = self.next_edit()?;
        self.edits.push(SpeakerEdit::Split {
            revision,
            speaker,
            new_speaker,
            at_ms,
            extra: BTreeMap::new(),
        });
        Ok(new_speaker)
    }

    pub fn turns(&self) -> Vec<SpeakerTurn> {
        let mut turns = Vec::new();
        for interval in &self.intervals {
            let Some(&id) = self.slots.get(&interval.slot) else {
                continue;
            };
            let mut pieces = vec![(interval.start_ms, interval.end_ms, id)];
            for edit in &self.edits {
                match edit {
                    SpeakerEdit::Merge { from, into, .. } => {
                        for (_, _, speaker) in &mut pieces {
                            if *speaker == *from {
                                *speaker = *into;
                            }
                        }
                    }
                    SpeakerEdit::Split {
                        speaker,
                        new_speaker,
                        at_ms,
                        ..
                    } => {
                        let mut next = Vec::with_capacity(pieces.len() + 1);
                        for (start, end, id) in pieces {
                            if id != *speaker || end <= *at_ms {
                                next.push((start, end, id));
                            } else if start >= *at_ms {
                                next.push((start, end, *new_speaker));
                            } else {
                                next.push((start, *at_ms, id));
                                next.push((*at_ms, end, *new_speaker));
                            }
                        }
                        pieces = next;
                    }
                    _ => (),
                }
            }
            for (start_ms, end_ms, speaker) in pieces {
                let mut extra = BTreeMap::new();
                if !interval.extra.is_empty() {
                    // Keep backend metadata under its original interval rather
                    // than claiming it describes each manually split piece.
                    extra.insert(
                        "online_source_interval".into(),
                        serde_json::json!({
                            "slot": interval.slot,
                            "start_ms": interval.start_ms,
                            "end_ms": interval.end_ms,
                            "extra": &interval.extra,
                        }),
                    );
                }
                extra.insert("online_session_id".into(), self.session_id.clone().into());
                extra.insert("backend_slot".into(), interval.slot.into());
                extra.insert("backend_revision".into(), self.backend_revision.into());
                let confirmed = self
                    .identities
                    .get(&speaker)
                    .is_some_and(|identity| identity.confirmed);
                extra.insert(
                    "identity_state".into(),
                    if confirmed { "confirmed" } else { "tentative" }.into(),
                );
                turns.push(SpeakerTurn {
                    start_ms,
                    end_ms,
                    speaker,
                    extra,
                });
            }
        }
        turns.sort_by_key(|turn| (turn.start_ms, turn.end_ms, turn.speaker));
        turns
    }

    pub fn apply_to_transcript(&self, transcript: &mut Transcript) -> Result<()> {
        let expected = self
            .run_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("说话人结果尚未绑定识别运行"))?;
        let run = transcript
            .run
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("逐字稿缺少识别运行身份"))?;
        ensure!(run.run_id == expected, "说话人结果与识别运行不匹配");
        if let Some(current) = transcript.extra.get("online_speakers") {
            ensure!(
                current.get("session_id").and_then(|value| value.as_str())
                    == Some(self.session_id.as_str()),
                "当前稿件属于其他说话人会话"
            );
            ensure!(
                current
                    .get("revision")
                    .and_then(|value| value.as_u64())
                    .is_some_and(|revision| revision <= self.revision),
                "当前稿件已有更新或无法核对的说话人版本"
            );
        } else {
            // An empty turn list does not establish ownership of names or
            // voiceprints left by another analysis. Reusing the same numeric
            // slot would silently assign an unrelated person's identity.
            ensure!(
                transcript.speaker_names.is_empty() && transcript.speaker_voiceprints.is_empty(),
                "当前稿件存在未绑定会话的姓名或声纹；在线结果单独保留，请先核对身份"
            );
        }
        ensure!(
            transcript.speaker_turns.iter().all(|turn| turn
                .extra
                .get("online_session_id")
                .and_then(|value| value.as_str())
                == Some(self.session_id.as_str())),
            "当前稿件含独立说话人版本，在线结果未覆盖"
        );
        transcript.speaker_turns = self.turns();
        for (id, identity) in &self.identities {
            if !identity.user_locked {
                continue;
            }
            if let Some(name) = &identity.name {
                let extra = transcript
                    .speaker_names
                    .get(id)
                    .map(|old| old.extra.clone())
                    .unwrap_or_default();
                transcript.speaker_names.insert(
                    *id,
                    SpeakerName {
                        extra,
                        name: name.clone(),
                        matched: false,
                        profile_id: None,
                    },
                );
            } else {
                transcript.speaker_names.remove(id);
            }
        }
        Ok(())
    }

    /// A checkpoint is separate from the ASR writer, so late speaker results
    /// cannot overwrite text revisions. Source data remains in the transcript.
    pub fn save(&self, base: &Path) -> Result<()> {
        let _activity = opencap_core::managed_meeting_activity(base)?;
        // Serialize checkpoint replacement across app instances, and never
        // let an older save of the same session overwrite a newer revision.
        let _guard = opencap_core::meeting_metadata_guard(base)?;
        if let Some(existing) = Self::load(base)? {
            if existing.session_id == self.session_id {
                ensure!(
                    existing.revision <= self.revision,
                    "在线说话人检查点已有更新版本，未覆盖"
                );
                if existing.revision == self.revision {
                    ensure!(
                        serde_json::to_value(&existing)? == serde_json::to_value(self)?,
                        "在线说话人检查点同版本内容冲突，未覆盖"
                    );
                }
            } else {
                use std::io::Read;
                ensure!(
                    existing.run_id.is_some()
                        && self.run_id.is_some()
                        && existing.run_id != self.run_id,
                    "同一识别运行已有其他在线说话人会话，未覆盖检查点"
                );
                let old_path = base.with_extension("speakers.online.json");
                let mut bytes = Vec::new();
                std::fs::File::open(&old_path)?
                    .take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 64 * 1024 * 1024,
                    "旧说话人检查点过大，未覆盖"
                );
                let copied: Self =
                    serde_json::from_slice(&bytes).context("旧说话人检查点在归档时无法读取")?;
                ensure!(
                    serde_json::to_value(&copied)? == serde_json::to_value(&existing)?,
                    "旧说话人检查点在归档时已变化，未覆盖"
                );
                let archive = base.with_extension(format!(
                    "speakers.online.archive-{}.json",
                    opencap_core::new_stable_id()
                ));
                opencap_core::atomic_create_bytes(&archive, &bytes)?;
                if let Some(name) = archive.file_name() {
                    opencap_core::register_meeting_asset(
                        base,
                        "online_speakers_history",
                        opencap_core::AssetOwnership::Managed,
                        Path::new(name),
                    )?;
                }
            }
        }
        let path = base.with_extension("speakers.online.json");
        crate::atomic_write(&path, &serde_json::to_vec_pretty(self)?)?;
        if let Some(name) = path.file_name() {
            opencap_core::register_meeting_asset(
                base,
                "online_speakers",
                opencap_core::AssetOwnership::Managed,
                Path::new(name),
            )?;
        }
        Ok(())
    }

    pub fn load(base: &Path) -> Result<Option<Self>> {
        use std::io::Read;
        const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
        let path = base.with_extension("speakers.online.json");
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_CHECKPOINT_BYTES,
            "在线说话人检查点过大或不可读"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_CHECKPOINT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CHECKPOINT_BYTES,
            "在线说话人检查点过大或不可读"
        );
        let registry: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            registry.schema_version == 1 && (1..=128).contains(&registry.capacity),
            "在线说话人检查点版本或容量无效"
        );
        ensure!(
            registry.edits.len() <= 512
                && registry.intervals.len() <= 100_000
                && registry.identities.len() <= 1024,
            "在线说话人检查点超出容量"
        );
        ensure!(
            registry.slots.iter().all(|(slot, id)| *slot > 0
                && *slot as usize <= registry.capacity
                && registry.identities.contains_key(id)),
            "在线说话人槽映射无效"
        );
        ensure!(
            registry.next_id > 0
                && registry
                    .identities
                    .iter()
                    .all(|(id, identity)| *id > 0 && identity.id == *id && *id < registry.next_id),
            "在线说话人身份编号无效；检查点保留"
        );
        ensure!(
            !registry.session_id.trim().is_empty()
                && registry
                    .run_id
                    .as_deref()
                    .is_none_or(|id| !id.trim().is_empty()),
            "在线说话人会话或运行身份无效；检查点保留"
        );
        let mut slot_ids = std::collections::BTreeSet::new();
        ensure!(
            registry.slots.values().all(|id| slot_ids.insert(*id)),
            "在线说话人槽映射重复；检查点保留"
        );
        let mut last_edit_revision = 0;
        for edit in &registry.edits {
            let (revision, valid) = match edit {
                SpeakerEdit::Name {
                    revision, speaker, ..
                }
                | SpeakerEdit::Confirm {
                    revision, speaker, ..
                } => (*revision, registry.identities.contains_key(speaker)),
                SpeakerEdit::Merge {
                    revision,
                    from,
                    into,
                    ..
                } => (
                    *revision,
                    from != into
                        && registry.identities.contains_key(from)
                        && registry.identities.contains_key(into),
                ),
                SpeakerEdit::Split {
                    revision,
                    speaker,
                    new_speaker,
                    at_ms,
                    ..
                } => (
                    *revision,
                    speaker != new_speaker
                        && registry.identities.contains_key(speaker)
                        && registry.identities.contains_key(new_speaker)
                        && *at_ms <= registry.input_end_ms.saturating_add(1000),
                ),
            };
            ensure!(
                valid && revision > last_edit_revision && revision <= registry.revision,
                "在线说话人修订顺序或身份无效；检查点保留"
            );
            last_edit_revision = revision;
        }
        ensure!(
            registry
                .intervals
                .iter()
                .all(|interval| interval.start_ms < interval.end_ms
                    && interval.end_ms <= registry.input_end_ms.saturating_add(1000)
                    && registry.slots.contains_key(&interval.slot)),
            "在线说话人区间无效"
        );
        Ok(Some(registry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(slot: u32, start_ms: u64, end_ms: u64) -> OnlineSpeakerInterval {
        OnlineSpeakerInterval {
            start_ms,
            end_ms,
            slot,
            extra: BTreeMap::new(),
        }
    }

    fn snapshot(
        revision: u64,
        input_end_ms: u64,
        intervals: Vec<OnlineSpeakerInterval>,
    ) -> OnlineSpeakerSnapshot {
        OnlineSpeakerSnapshot {
            revision,
            input_end_ms,
            finished: false,
            intervals,
        }
    }

    #[test]
    fn backend_slots_map_to_stable_session_ids_across_revisions() {
        let mut registry = OnlineSpeakerRegistry::new(8, None).unwrap();
        registry
            .apply_snapshot(snapshot(
                1,
                2_000,
                vec![interval(3, 0, 1_000), interval(5, 1_000, 2_000)],
            ))
            .unwrap();
        // Reordered and split intervals in a later revision must keep the same
        // identities bound to the backend slots.
        registry
            .apply_snapshot(snapshot(
                2,
                4_000,
                vec![
                    interval(5, 1_000, 2_000),
                    interval(3, 0, 1_000),
                    interval(3, 3_000, 4_000),
                ],
            ))
            .unwrap();
        let turns = registry.turns();
        let slot3: Vec<_> = turns
            .iter()
            .filter(|turn| turn.start_ms < 1_000 || turn.start_ms == 3_000)
            .collect();
        assert_eq!(slot3.len(), 2);
        assert_eq!(slot3[0].speaker, slot3[1].speaker, "同一后端槽不能重新编号");
        let slot5 = turns.iter().find(|turn| turn.start_ms == 1_000).unwrap();
        assert_ne!(slot5.speaker, slot3[0].speaker);
        assert_eq!(registry.identities.len(), 2);
    }

    #[test]
    fn stale_revision_and_time_regressions_are_rejected() {
        let mut registry = OnlineSpeakerRegistry::new(4, None).unwrap();
        assert!(
            registry
                .apply_snapshot(snapshot(2, 2_000, vec![interval(1, 0, 1_000)]))
                .unwrap()
        );
        assert!(
            !registry
                .apply_snapshot(snapshot(1, 2_500, vec![interval(1, 0, 2_000)]))
                .unwrap()
        );
        assert!(
            registry
                .apply_snapshot(snapshot(3, 1_500, vec![interval(1, 0, 1_000)]))
                .is_err()
        );
        assert_eq!(registry.input_end_ms, 2_000);
        assert_eq!(registry.interval_count(), 1);
    }

    #[test]
    fn slot_outside_capacity_is_rejected_instead_of_merged() {
        let mut registry = OnlineSpeakerRegistry::new(2, None).unwrap();
        assert!(
            registry
                .apply_snapshot(snapshot(1, 1_000, vec![interval(3, 0, 1_000)]))
                .is_err()
        );
        assert!(registry.identities.is_empty());
    }

    #[test]
    fn manual_name_and_confirmation_survive_later_snapshots() {
        let mut registry = OnlineSpeakerRegistry::new(4, None).unwrap();
        registry
            .apply_snapshot(snapshot(1, 2_000, vec![interval(1, 0, 1_000)]))
            .unwrap();
        let id = registry.turns()[0].speaker;
        registry.rename(id, "张总").unwrap();
        registry.confirm(id).unwrap();
        registry
            .apply_snapshot(snapshot(
                2,
                3_000,
                vec![interval(1, 0, 2_000), interval(2, 2_000, 3_000)],
            ))
            .unwrap();
        let identity = &registry.identities[&id];
        assert_eq!(identity.name.as_deref(), Some("张总"));
        assert!(identity.user_locked && identity.confirmed);
        let turns = registry.turns();
        assert!(
            turns
                .iter()
                .any(|turn| turn.speaker == id && turn.end_ms == 2_000)
        );
    }

    #[test]
    fn merge_requires_named_conflict_to_be_resolved_and_split_keeps_evidence() {
        let mut registry = OnlineSpeakerRegistry::new(4, None).unwrap();
        registry
            .apply_snapshot(snapshot(
                1,
                3_000,
                vec![interval(1, 0, 1_000), interval(2, 1_000, 3_000)],
            ))
            .unwrap();
        let turns = registry.turns();
        let first = turns
            .iter()
            .find(|turn| turn.start_ms == 0)
            .unwrap()
            .speaker;
        let second = turns
            .iter()
            .find(|turn| turn.start_ms == 1_000)
            .unwrap()
            .speaker;
        registry.rename(first, "甲").unwrap();
        registry.rename(second, "乙").unwrap();
        assert!(
            registry.merge(first, second).is_err(),
            "不同姓名合并必须要求用户明确"
        );
        let third = registry.split(second, 2_000).unwrap();
        let turns = registry.turns();
        assert!(
            turns
                .iter()
                .any(|turn| turn.speaker == third && turn.end_ms <= 3_000)
        );
        assert!(
            turns
                .iter()
                .any(|turn| turn.speaker == second && turn.start_ms < 2_000)
        );
    }
}
