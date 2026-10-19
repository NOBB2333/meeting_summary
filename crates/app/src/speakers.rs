//! Editable meeting names and explicitly enrolled local voice profiles.
use super::*;
use gpui::component::input::Input;

pub(super) enum SpeakerEvent {
    Name(u32, String, bool),
    Forget(String),
}
#[derive(PartialEq)]
struct SpeakerRow {
    id: u32,
    name: String,
    matched: bool,
    can_remember: bool,
}
pub(super) struct SpeakersView {
    signature: Vec<SpeakerRow>,
    inputs: Vec<(u32, Entity<InputState>)>,
    profiles: Vec<opencap_asr::VoiceProfile>,
    status: String,
    busy: bool,
}
fn root() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(dir) = smoke::isolated_dir() {
        return dir.join("models");
    }
    opencap_asr::model_root()
}
impl SpeakersView {
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
            profiles: Vec::new(),
            status: String::new(),
            busy: false,
        }
    }
    pub fn replace(
        &mut self,
        t: &opencap_asr::Transcript,
        new_document: bool,
        busy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = t
            .speaker_turns
            .iter()
            .map(|t| t.speaker)
            .collect::<std::collections::BTreeSet<_>>();
        let signature: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let name = t.speaker_names.get(&id);
                SpeakerRow {
                    id,
                    name: name.map(|n| n.name.clone()).unwrap_or_default(),
                    matched: name.is_some_and(|n| n.matched),
                    can_remember: t.speaker_voiceprints.contains_key(&id),
                }
            })
            .collect();
        if new_document || signature != self.signature {
            self.inputs = signature
                .iter()
                .map(|row| {
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
            self.signature = signature;
            cx.notify();
        }
        if self.busy != busy {
            self.busy = busy;
            cx.notify();
        }
    }
}
impl EventEmitter<SpeakerEvent> for SpeakersView {}
impl Render for SpeakersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut view = div().w_full().flex().flex_col().gap_3();
        for (id, input) in &self.inputs {
            let id = *id;
            let input1 = input.clone();
            let input2 = input.clone();
            let info = self.signature.iter().find(|s| s.id == id).unwrap();
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .child(label(format!(
                        "说话人 {id}{}",
                        if info.matched {
                            " · 声纹匹配，请核对"
                        } else {
                            ""
                        }
                    )))
                    .child(Input::new(input).disabled(self.busy))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                button(("name-speaker", id), "仅本场保存", false)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(SpeakerEvent::Name(
                                            id,
                                            input1.read(cx).value().to_string(),
                                            false,
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
                                        ))
                                    })),
                            ),
                    )
                    .when(!info.can_remember, |d| {
                        d.child(label("记住声音需重新分析，且至少有 3 秒独立发言"))
                    }),
            );
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
        if self.active() || self.transcription_busy() {
            return;
        }
        let base = self.transcript_output.clone();
        let mut transcript = self.transcript.clone();
        let operation = match event {
            SpeakerEvent::Name(id, name, remember) => Some((*id, name.clone(), *remember)),
            SpeakerEvent::Forget(_) => None,
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
                            transcript.sync_outputs(base);
                        }
                    }
                    Ok::<_, anyhow::Error>((
                        base,
                        transcript,
                        opencap_asr::voice_profiles(&root())?,
                    ))
                })
                .await;
            let _ = entity.update(cx, |this, cx| {
                this.voice_busy = false;
                match result {
                    Ok((base, transcript, profiles)) => {
                        if this.transcript_output == base {
                            this.transcript = transcript;
                            this.transcript_dirty = true;
                            this.remember_meeting();
                        }
                        this.speakers_view.update(cx, |view, cx| {
                            view.profiles = profiles;
                            view.status.clear();
                            cx.notify();
                        });
                        this.transcription_status = if forgetting {
                            "已忘记本机保存的声音；既有会议中的姓名保留。".into()
                        } else if this.transcript.output_warnings.is_empty() {
                            "姓名已保存，逐字稿文件已同步。已有 AI 纪要可在对话中要求更新。".into()
                        } else {
                            "姓名已更新；部分逐字稿文件未同步，可重试同步文件。".into()
                        };
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
