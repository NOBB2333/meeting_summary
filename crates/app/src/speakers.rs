//! Editable meeting names and explicitly enrolled local voice profiles.
use super::*;
use gpui::component::input::Input;

pub(super) enum SpeakerEvent {
    Name(u32, String, bool, u64),
    Forget(String),
    #[cfg(feature = "internal-player")]
    PlayTurn(u32, u64, u64, u64),
    #[cfg(feature = "online-speaker")]
    Confirm(u32, u64),
    #[cfg(feature = "online-speaker")]
    Merge(u32, u32, u64),
    #[cfg(feature = "online-speaker")]
    Split(u32, u64, u64),
}
#[derive(PartialEq)]
struct SpeakerRow {
    id: u32,
    name: String,
    matched: bool,
    can_remember: bool,
    tentative: bool,
}
pub(super) struct SpeakersView {
    signature: Vec<SpeakerRow>,
    inputs: Vec<(u32, Entity<InputState>)>,
    orphan_inputs: Vec<(u32, Entity<InputState>)>,
    detached_drafts: Vec<(u32, Entity<InputState>)>,
    draft_generation: u64,
    draft_run: Option<(String, String)>,
    #[allow(clippy::type_complexity)]
    drafts_by_run: std::collections::BTreeMap<(String, String), Vec<(u32, Entity<InputState>)>>,
    profiles: Vec<opencap_asr::VoiceProfile>,
    status: String,
    busy: bool,
    online_editable: bool,
    split_position: Option<u64>,
    #[cfg(feature = "internal-player")]
    #[allow(clippy::type_complexity)]
    turns: std::collections::BTreeMap<u32, Vec<(u64, u64, bool, bool, String)>>,
    #[cfg(feature = "internal-player")]
    turn_pages: std::collections::BTreeMap<u32, usize>,
}
fn root() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(dir) = smoke::isolated_dir() {
        return dir.join("models");
    }
    opencap_asr::model_root()
}
impl SpeakersView {
    pub(super) fn has_unsaved_drafts(&self, cx: &App) -> bool {
        !self.orphan_inputs.is_empty()
            || !self.detached_drafts.is_empty()
            || !self.drafts_by_run.is_empty()
            || self.inputs.iter().any(|(id, input)| {
                let original = self
                    .signature
                    .iter()
                    .find(|row| row.id == *id)
                    .map(|row| row.name.as_str())
                    .unwrap_or("");
                input.read(cx).value().trim() != original
            })
    }
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.spawn(async |entity, cx| {
            let profiles = cx
                .background_executor()
                .spawn(async { opencap_asr::voice_profiles(&root()) })
                .await;
            let _ = entity.update(cx, |this, cx| {
                match profiles {
                    Ok(p) => this.profiles = p,
                    Err(e) => this.status = format!("声纹记忆读取失败：{e:#}"),
                };
                cx.notify();
            });
        })
        .detach();
        Self {
            signature: Vec::new(),
            inputs: Vec::new(),
            orphan_inputs: Vec::new(),
            detached_drafts: Vec::new(),
            draft_generation: 0,
            draft_run: None,
            drafts_by_run: Default::default(),
            profiles: Vec::new(),
            status: String::new(),
            busy: false,
            online_editable: false,
            split_position: None,
            #[cfg(feature = "internal-player")]
            turns: Default::default(),
            #[cfg(feature = "internal-player")]
            turn_pages: Default::default(),
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replace(
        &mut self,
        t: &opencap_asr::Transcript,
        new_document: bool,
        data_changed: bool,
        busy: bool,
        online_editable: bool,
        split_position: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let incoming_run = t.run.as_ref().and_then(|run| {
            run.meeting_id
                .as_ref()
                .filter(|meeting| !meeting.is_empty() && !run.run_id.is_empty())
                .map(|meeting| (meeting.clone(), run.run_id.clone()))
        });
        // A new run is a distinct identity even if its caller did not set the
        // document-switch flag. Do not reuse same-numbered speaker inputs.
        let new_document =
            new_document || (self.draft_run.is_some() && self.draft_run != incoming_run);
        if !new_document && self.draft_run.is_none() {
            self.draft_run = incoming_run.clone();
        }
        #[cfg(feature = "internal-player")]
        if data_changed || new_document {
            let mut ordered: Vec<_> = t
                .speaker_turns
                .iter()
                .filter(|turn| turn.end_ms > turn.start_ms)
                .collect();
            ordered.sort_by_key(|turn| (turn.start_ms, turn.end_ms, turn.speaker));
            let mut overlaps = vec![false; ordered.len()];
            // Track the greatest prior end per speaker, then the earliest
            // later start per speaker. Two passes mark both sides of an
            // overlap without enumerating all pairs in a long meeting.
            let mut ends = std::collections::BTreeSet::<(u64, u32)>::new();
            let mut by_speaker = std::collections::BTreeMap::<u32, u64>::new();
            for (index, turn) in ordered.iter().enumerate() {
                overlaps[index] = ends
                    .iter()
                    .rev()
                    .find(|(_, speaker)| *speaker != turn.speaker)
                    .is_some_and(|(end, _)| *end > turn.start_ms);
                let old = by_speaker.get(&turn.speaker).copied();
                let end = old.map_or(turn.end_ms, |value| value.max(turn.end_ms));
                if let Some(old) = old {
                    ends.remove(&(old, turn.speaker));
                }
                by_speaker.insert(turn.speaker, end);
                ends.insert((end, turn.speaker));
            }
            let mut starts = std::collections::BTreeSet::<(u64, u32)>::new();
            by_speaker.clear();
            for (index, turn) in ordered.iter().enumerate().rev() {
                overlaps[index] |= starts
                    .iter()
                    .find(|(_, speaker)| *speaker != turn.speaker)
                    .is_some_and(|(start, _)| *start < turn.end_ms);
                let old = by_speaker.get(&turn.speaker).copied();
                let start = old.map_or(turn.start_ms, |value| value.min(turn.start_ms));
                if let Some(old) = old {
                    starts.remove(&(old, turn.speaker));
                }
                by_speaker.insert(turn.speaker, start);
                starts.insert((start, turn.speaker));
            }
            let mut segments: Vec<_> = t
                .segments
                .iter()
                .filter(|segment| {
                    segment.is_confirmed()
                        && segment.end_ms > segment.start_ms
                        && !segment.text.trim().is_empty()
                })
                .collect();
            segments.sort_by_key(|segment| segment.start_ms);
            let mut prefix_end = Vec::with_capacity(segments.len());
            let mut latest_end = 0u64;
            for segment in &segments {
                latest_end = latest_end.max(segment.end_ms);
                prefix_end.push(latest_end);
            }
            let mut turns =
                std::collections::BTreeMap::<u32, Vec<(u64, u64, bool, bool, String)>>::new();
            for (turn, overlap) in ordered.into_iter().zip(overlaps) {
                let tentative = turn
                    .extra
                    .get("identity_state")
                    .and_then(|value| value.as_str())
                    == Some("tentative");
                let upper = segments.partition_point(|segment| segment.start_ms < turn.end_ms);
                let lower = prefix_end[..upper].partition_point(|end| *end <= turn.start_ms);
                let excerpt = segments[lower..upper]
                    .iter()
                    .find(|segment| segment.end_ms > turn.start_ms)
                    .map(|segment| {
                        let mut chars = segment.text.chars();
                        let mut preview: String = chars
                            .by_ref()
                            .take(80)
                            .map(|ch| if ch.is_control() { ' ' } else { ch })
                            .collect();
                        if chars.next().is_some() {
                            preview.push('…');
                        }
                        preview
                    })
                    .unwrap_or_default();
                turns.entry(turn.speaker).or_default().push((
                    turn.start_ms,
                    turn.end_ms,
                    tentative,
                    overlap,
                    excerpt,
                ));
            }
            for ranges in turns.values_mut() {
                ranges.sort_unstable();
            }
            if new_document {
                self.turn_pages.clear();
            }
            if self.turns != turns || new_document {
                self.turns = turns;
                self.draft_generation = self.draft_generation.wrapping_add(1);
                cx.notify();
            }
        }
        if data_changed || new_document {
            let mut ids = std::collections::BTreeMap::<u32, bool>::new();
            for turn in &t.speaker_turns {
                let tentative = turn
                    .extra
                    .get("identity_state")
                    .and_then(|value| value.as_str())
                    == Some("tentative");
                ids.entry(turn.speaker)
                    .and_modify(|value| *value |= tentative)
                    .or_insert(tentative);
            }
            let signature: Vec<_> = ids
                .into_iter()
                .map(|(id, tentative)| {
                    let name = t.speaker_names.get(&id);
                    SpeakerRow {
                        id,
                        name: name.map(|n| n.name.clone()).unwrap_or_default(),
                        matched: name.is_some_and(|n| n.matched),
                        can_remember: t.speaker_voiceprints.contains_key(&id),
                        tentative,
                    }
                })
                .collect();
            if new_document || signature != self.signature {
                let next_run = incoming_run;
                let mut prior_orphans: std::collections::BTreeSet<u32> =
                    self.orphan_inputs.iter().map(|(id, _)| *id).collect();
                let mut previous = if new_document {
                    let mut drafts = std::mem::take(&mut self.orphan_inputs);
                    drafts.extend(std::mem::take(&mut self.inputs).into_iter().filter(
                        |(id, input)| {
                            let original = self
                                .signature
                                .iter()
                                .find(|row| row.id == *id)
                                .map(|row| row.name.as_str())
                                .unwrap_or("");
                            input.read(cx).value().trim() != original
                        },
                    ));
                    if let Some(key) = self.draft_run.take() {
                        if !drafts.is_empty() {
                            self.drafts_by_run.insert(key, drafts);
                        }
                    } else {
                        // No stable identity means we cannot safely associate
                        // these edits with any later document or speaker ID.
                        self.detached_drafts.extend(drafts);
                    }
                    self.orphan_inputs.clear();
                    let drafts = next_run
                        .as_ref()
                        .and_then(|key| self.drafts_by_run.remove(key))
                        .unwrap_or_default();
                    prior_orphans = drafts.iter().map(|(id, _)| *id).collect();
                    self.draft_run = next_run;
                    drafts
                } else {
                    std::mem::take(&mut self.inputs)
                };
                if !new_document {
                    previous.append(&mut self.orphan_inputs);
                }
                self.inputs = signature
                    .iter()
                    .map(|row| {
                        if let Some((_, input)) = previous.iter().find(|(id, _)| *id == row.id) {
                            let input = input.clone();
                            if !new_document
                                && let Some(old) =
                                    self.signature.iter().find(|old| old.id == row.id)
                                && old.name != row.name
                                && (input.read(cx).value().as_str() == old.name
                                    || input.read(cx).value().trim() == row.name)
                            {
                                input.update(cx, |state, cx| {
                                    state.set_value(row.name.clone(), window, cx)
                                });
                            }
                            return (row.id, input);
                        }
                        (
                            row.id,
                            cx.new(|cx| {
                                InputState::new(window, cx)
                                    .placeholder("填写姓名")
                                    .default_value(row.name.clone())
                            }),
                        )
                    })
                    .collect();
                self.orphan_inputs = previous
                    .into_iter()
                    .filter(|(id, input)| {
                        if signature.iter().any(|row| row.id == *id) {
                            return false;
                        }
                        let current = input.read(cx).value().to_string();
                        let original = self
                            .signature
                            .iter()
                            .find(|row| row.id == *id)
                            .map(|row| row.name.as_str())
                            .unwrap_or("");
                        prior_orphans.contains(id) || current.trim() != original
                    })
                    .collect();
                self.signature = signature;
                self.draft_generation = self.draft_generation.wrapping_add(1);
                cx.notify();
            }
        }
        if self.busy != busy
            || self.online_editable != online_editable
            || self.split_position != split_position
        {
            self.busy = busy;
            self.online_editable = online_editable;
            self.split_position = split_position;
            cx.notify();
        }
    }
}
impl EventEmitter<SpeakerEvent> for SpeakersView {}
impl Render for SpeakersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut view = div().w_full().flex().flex_col().gap_3();
        for (_index, (id, input)) in self.inputs.iter().enumerate() {
            let id = *id;
            let generation = self.draft_generation;
            let input1 = input.clone();
            let input2 = input.clone();
            let input3 = input.clone();
            let info = self.signature.iter().find(|s| s.id == id).unwrap();
            let original_name = info.name.clone();
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .child(label(format!(
                        "说话人 {id}{}{}",
                        if info.matched {
                            " · 声纹匹配，请核对"
                        } else {
                            ""
                        },
                        if info.tentative {
                            " · 含暂定区间"
                        } else {
                            ""
                        }
                    )))
                    .child(Input::new(input).disabled(self.busy && !self.online_editable))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                button(("name-speaker", id), "仅本场保存", false)
                                    .disabled(self.busy && !self.online_editable)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(SpeakerEvent::Name(
                                            id,
                                            input1.read(cx).value().to_string(),
                                            false,
                                            generation,
                                        ))
                                    })),
                            )
                            .child(
                                button(("remember-speaker", id), "保存并记住声音", false)
                                    .disabled(self.busy || !info.can_remember)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(SpeakerEvent::Name(
                                            id,
                                            input2.read(cx).value().to_string(),
                                            true,
                                            generation,
                                        ))
                                    })),
                            )
                            .child(
                                button(("reset-speaker-name", id), "撤销输入", false)
                                    .ghost()
                                    .disabled(self.busy && !self.online_editable)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if this
                                            .signature
                                            .iter()
                                            .any(|row| row.id == id && row.name == original_name)
                                        {
                                            input3.update(cx, |state, cx| {
                                                state.set_value(original_name.clone(), window, cx)
                                            });
                                        }
                                    })),
                            ),
                    )
                    .when(cfg!(feature = "internal-player"), |d| {
                        #[cfg(feature = "internal-player")]
                        let d = {
                            let turns = self.turns.get(&id).map(Vec::as_slice).unwrap_or_default();
                            let pages = turns.len().div_ceil(5).max(1);
                            let page = self
                                .turn_pages
                                .get(&id)
                                .copied()
                                .unwrap_or(0)
                                .min(pages - 1);
                            let heading = if turns.is_empty() {
                                "没有可回听的有效时间区间".to_string()
                            } else {
                                format!("发言区间 · 第 {} / {} 页（点击回听本段）", page + 1, pages)
                            };
                            let mut panel = div().flex().flex_col().gap_1().child(label(heading));
                            for (offset, (start, end, tentative, overlap, excerpt)) in
                                turns.iter().skip(page * 5).take(5).cloned().enumerate()
                            {
                                panel = panel.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            button(
                                                format!("speaker-turn-{id}-{offset}"),
                                                format!(
                                                    "{} – {}{}{}",
                                                    opencap_asr::timestamp(start, '.'),
                                                    opencap_asr::timestamp(end, '.'),
                                                    if tentative { " · 暂定" } else { "" },
                                                    if overlap { " · 多人重叠" } else { "" }
                                                ),
                                                false,
                                            )
                                            .ghost()
                                            .on_click(
                                                cx.listener(move |_, _, _, cx| {
                                                    cx.emit(SpeakerEvent::PlayTurn(
                                                        id, start, end, generation,
                                                    ))
                                                }),
                                            ),
                                        )
                                        .when(!excerpt.is_empty(), |row| {
                                            row.child(label(format!("附近文字：{excerpt}")))
                                        }),
                                );
                            }
                            panel = panel.when(turns.len() > 5, |panel| {
                                panel.child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            button(("speaker-turn-prev", id), "上一页", false)
                                                .ghost()
                                                .disabled(page == 0)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.turn_pages
                                                        .insert(id, page.saturating_sub(1));
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            button(("speaker-turn-next", id), "下一页", false)
                                                .ghost()
                                                .disabled(page + 1 >= pages)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.turn_pages
                                                        .insert(id, (page + 1).min(pages - 1));
                                                    cx.notify();
                                                })),
                                        ),
                                )
                            });
                            d.child(panel)
                        };
                        d
                    })
                    .when(!info.can_remember, |d| {
                        d.child(label("记住声音需重新分析，且至少有 3 秒独立发言"))
                    })
                    .when(self.online_editable, |d| {
                        #[cfg(feature = "online-speaker")]
                        let d = {
                            let previous = _index
                                .checked_sub(1)
                                .map(|previous| self.inputs[previous].0);
                            let split = self.split_position;
                            d.child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        button(("confirm-online-speaker", id), "确认编号", false)
                                            .ghost()
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.emit(SpeakerEvent::Confirm(id, generation))
                                            })),
                                    )
                                    .when_some(previous, |d, target| {
                                        d.child(
                                            button(
                                                ("merge-online-speaker", id),
                                                format!("合并到说话人 {target}"),
                                                false,
                                            )
                                            .ghost()
                                            .on_click(
                                                cx.listener(move |_, _, _, cx| {
                                                    cx.emit(SpeakerEvent::Merge(
                                                        id, target, generation,
                                                    ))
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        button(
                                            ("split-online-speaker", id),
                                            "从播放位置拆分",
                                            false,
                                        )
                                        .ghost()
                                        .disabled(split.is_none())
                                        .on_click(
                                            cx.listener(move |_, _, _, cx| {
                                                if let Some(position) = split {
                                                    cx.emit(SpeakerEvent::Split(
                                                        id, position, generation,
                                                    ));
                                                }
                                            }),
                                        ),
                                    ),
                            )
                        };
                        d
                    }),
            );
        }
        if !self.orphan_inputs.is_empty() {
            view = view.child(label(
                "以下姓名草稿的说话人编号已变化，暂不能保存；可复制或等待编号恢复。",
            ));
            let generation = self.draft_generation;
            for (index, (id, input)) in self.orphan_inputs.iter().enumerate() {
                view = view.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label(format!("说话人 {id} · 未保存草稿")))
                        .child(Input::new(input))
                        .child(
                            button(("discard-orphan-speaker-draft", index), "移除草稿", false)
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.draft_generation == generation
                                        && index < this.orphan_inputs.len()
                                    {
                                        this.orphan_inputs.remove(index);
                                        this.draft_generation =
                                            this.draft_generation.wrapping_add(1);
                                        cx.notify();
                                    }
                                })),
                        ),
                );
            }
        }
        if !self.detached_drafts.is_empty() {
            view = view.child(label(
                "以下姓名草稿来自无法确认身份的先前文档，仅供复制，不会自动关联当前会议。",
            ));
            let generation = self.draft_generation;
            for (index, (id, input)) in self.detached_drafts.iter().enumerate() {
                view = view.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label(format!("原说话人 {id} · 未绑定草稿")))
                        .child(Input::new(input))
                        .child(
                            button(("discard-detached-speaker-draft", index), "移除草稿", false)
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.draft_generation == generation
                                        && index < this.detached_drafts.len()
                                    {
                                        this.detached_drafts.remove(index);
                                        this.draft_generation =
                                            this.draft_generation.wrapping_add(1);
                                        cx.notify();
                                    }
                                })),
                        ),
                );
            }
        }
        if !self.drafts_by_run.is_empty() {
            view = view.child(label(
                "其他会议的姓名草稿仅在本次运行中保留；可复制或切回原会议保存。",
            ));
            let generation = self.draft_generation;
            for (group, (key, drafts)) in self.drafts_by_run.iter().enumerate() {
                let (meeting, run) = key;
                view = view.child(label(format!("会议 {} · 运行 {}", meeting, run)));
                for (index, (id, input)) in drafts.iter().enumerate() {
                    let key = key.clone();
                    view = view.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label(format!("说话人 {id} · 未保存草稿")))
                            .child(Input::new(input))
                            .child(
                                button(
                                    format!("discard-other-speaker-draft-{group}-{index}"),
                                    "移除草稿",
                                    false,
                                )
                                .ghost()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if this.draft_generation != generation {
                                            return;
                                        }
                                        let empty = if let Some(entries) =
                                            this.drafts_by_run.get_mut(&key)
                                        {
                                            if index >= entries.len() {
                                                return;
                                            }
                                            entries.remove(index);
                                            entries.is_empty()
                                        } else {
                                            return;
                                        };
                                        if empty {
                                            this.drafts_by_run.remove(&key);
                                        }
                                        this.draft_generation =
                                            this.draft_generation.wrapping_add(1);
                                        cx.notify();
                                    },
                                )),
                            ),
                    );
                }
            }
        }
        if !self.profiles.is_empty() {
            view = view.child(label("本机已记住的声音"));
            for (i, profile) in self.profiles.iter().enumerate() {
                let id = profile.id.clone();
                view = view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(label(profile.name.clone()).flex_1())
                        .child(
                            button(("forget-voice", i), "忘记", false)
                                .ghost()
                                .disabled(self.busy)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SpeakerEvent::Forget(id.clone()))
                                })),
                        ),
                );
            }
        }
        view.child(label(self.status.clone()))
    }
}
impl Recorder {
    pub(super) fn edit_speaker(&mut self, event: &SpeakerEvent, cx: &mut Context<Self>) {
        if let SpeakerEvent::Name(_, _, _, generation) = event
            && (self.transcript_dirty
                || self.transcript_new_document
                || self.speakers_view.read(cx).draft_generation != *generation)
        {
            self.transcription_status = "说话人列表已变化，请重新查看后保存姓名。".into();
            cx.notify();
            return;
        }
        #[cfg(feature = "online-speaker")]
        if let SpeakerEvent::Confirm(_, generation)
        | SpeakerEvent::Merge(_, _, generation)
        | SpeakerEvent::Split(_, _, generation) = event
            && (self.transcript_dirty
                || self.transcript_new_document
                || self.speakers_view.read(cx).draft_generation != *generation)
        {
            self.transcription_status = "说话人列表已变化，请重新查看后操作。".into();
            cx.notify();
            return;
        }
        #[cfg(feature = "online-speaker")]
        if let SpeakerEvent::Split(_, at, _) = event
            && self.speakers_view.read(cx).split_position != Some(*at)
        {
            self.transcription_status = "播放位置已变化，请重新选择拆分位置。".into();
            cx.notify();
            return;
        }
        #[cfg(feature = "internal-player")]
        if let SpeakerEvent::PlayTurn(id, start, end, generation) = event {
            if !self.transcript_dirty
                && !self.transcript_new_document
                && self.speakers_view.read(cx).draft_generation == *generation
                && self.transcript.speaker_turns.iter().any(|turn| {
                    turn.speaker == *id
                        && turn.start_ms == *start
                        && turn.end_ms == *end
                        && end > start
                })
            {
                self.play_speaker_turn(*id, *start, *end, cx);
            } else {
                self.transcription_status = "说话人区间已变化，请重新选择。".into();
                cx.notify();
            }
            return;
        }
        if self.transcript_sync.is_some()
            || self.voice_busy
            || self.speaker_task.is_some()
            || self.ai.correction_task.is_some()
        {
            self.transcription_status = "逐字稿或说话人任务正在处理，请稍后再修改说话人。".into();
            cx.notify();
            return;
        }
        #[cfg(feature = "online-speaker")]
        if let Some(mut controller) = self.online_speakers.take() {
            let relevant = controller.applies_to(self.transcript_output.as_ref(), &self.transcript);
            let result = if relevant {
                match event {
                    SpeakerEvent::Name(id, name, false, _) => Some(controller.edit_name(*id, name)),
                    SpeakerEvent::Confirm(id, _) => Some(controller.confirm(*id)),
                    SpeakerEvent::Merge(from, into, _) => Some(controller.merge(*from, *into)),
                    SpeakerEvent::Split(id, at, _) => Some(controller.split(*id, *at)),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(result) = result {
                match result.and_then(|()| controller.apply(&mut self.transcript)) {
                    Ok(()) => {
                        self.transcript_dirty = true;
                        self.transcript.primary_save_pending = true;
                        controller.status = "说话人修订已记录；后台结果不会覆盖人工修改。".into();
                    }
                    Err(error) => controller.status = format!("说话人修订未保存：{error:#}"),
                }
                self.online_speaker_status = controller.status.clone();
                self.online_speakers = Some(controller);
                cx.notify();
                return;
            }
            self.online_speakers = Some(controller);
        }
        #[cfg(feature = "online-speaker")]
        if matches!(
            event,
            SpeakerEvent::Confirm(_, _)
                | SpeakerEvent::Merge(_, _, _)
                | SpeakerEvent::Split(_, _, _)
        ) {
            self.online_speaker_status = "当前会议没有可编辑的在线说话人会话。".into();
            cx.notify();
            return;
        }
        if self.active() || self.transcript_write_busy() {
            return;
        }
        let base = self.transcript_output.clone();
        let expected = base.as_ref().and_then(|base| {
            self.transcript_disk_baseline
                .as_ref()
                .filter(|(saved_base, _)| saved_base == base)
                .map(|(_, digest)| digest.clone())
        });
        let mut transcript = self.transcript.clone();
        let operation = match event {
            SpeakerEvent::Name(id, name, remember, _) => Some((*id, name.clone(), *remember)),
            SpeakerEvent::Forget(_) => None,
            #[allow(unreachable_patterns)]
            _ => None,
        };
        let forget = match event {
            SpeakerEvent::Forget(id) => Some(id.clone()),
            _ => None,
        };
        let forgetting = forget.is_some();
        self.voice_busy = true;
        cx.spawn(async move |entity, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(id) = forget {
                        opencap_asr::forget_voice(&root(), &id)?;
                    }
                    if let Some((id, name, remember)) = operation {
                        opencap_asr::name_speaker(&mut transcript, &root(), id, &name, remember)?;
                        if let Some(base) = &base {
                            let guard = opencap_core::meeting_metadata_guard(base);
                            let checked = guard
                                .as_ref()
                                .map_err(|error| anyhow::anyhow!("无法锁定会议元数据：{error:#}"))
                                .and_then(|_| {
                                    crate::transcription::verify_transcript_retry(
                                        base,
                                        &transcript,
                                        expected.as_deref(),
                                    )
                                });
                            if let Err(error) = checked {
                                transcript.primary_save_pending = true;
                                transcript
                                    .output_warnings
                                    .retain(|warning| !warning.starts_with("逐字稿并发核对失败："));
                                transcript
                                    .output_warnings
                                    .push(format!("逐字稿并发核对失败：{error:#}"));
                            } else {
                                transcript.sync_outputs(base);
                            }
                        } else {
                            transcript.primary_save_pending = true;
                        }
                    }
                    Ok::<_, anyhow::Error>((base, transcript, opencap_asr::voice_profiles(&root())))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.voice_busy = false;
                match result {
                    Ok((base, transcript, profiles)) => {
                        if !forgetting && this.transcript_output == base {
                            if let (Some(base), Some(digest)) = (
                                base.as_ref(),
                                crate::transcription::saved_transcript_digest(&transcript),
                            ) {
                                this.transcript_disk_baseline = Some((base.clone(), digest));
                            }
                            this.transcript = transcript;
                            this.transcript_dirty = true;
                            this.remember_meeting();
                        }
                        let profile_warning = profiles
                            .as_ref()
                            .err()
                            .map(|error| format!("声纹列表未刷新：{error:#}"));
                        this.speakers_view.update(cx, |view, cx| {
                            match profiles {
                                Ok(profiles) => {
                                    view.profiles = profiles;
                                    view.status.clear();
                                }
                                Err(error) => view.status = format!("声纹列表未刷新：{error:#}"),
                            }
                            cx.notify();
                        });
                        this.transcription_status = if forgetting {
                            "已忘记本机保存的声音；既有会议中的姓名保留。".into()
                        } else if this.transcript.primary_save_pending {
                            "姓名已更新但主稿未保存；请核对冲突或另存恢复副本。".into()
                        } else if this.transcript.output_warnings.is_empty() {
                            "姓名已保存，逐字稿文件已同步。已有 AI 纪要可在对话中要求更新。".into()
                        } else {
                            "姓名已更新；部分逐字稿文件未同步，可重试同步文件。".into()
                        };
                        if let Some(warning) = profile_warning {
                            this.transcription_status.push_str(&format!("；{warning}"));
                        }
                    }
                    Err(e) => this.transcription_status = format!("姓名 / 声纹记忆未保存：{e:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
