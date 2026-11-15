//! Interim in-app audio/video review controls.
use super::*;
use opencap_media::{PlaybackCommand, PlaybackRange, PlaybackSession, PlaybackState};

#[derive(Clone)]
pub(super) enum PlayerRequest {
    Play,
    RetryCurrent,
    At(u64),
    SpeakerTurn {
        run_id: Option<String>,
        speaker: u32,
        start: u64,
        end: u64,
    },
}

pub(super) struct WaveformJob {
    pub base: PathBuf,
    media: PathBuf,
    audio_stream: Option<u64>,
    expected_fingerprint: Option<opencap_core::MediaFingerprint>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    result: crossbeam_channel::Receiver<anyhow::Result<opencap_asr::Waveform>>,
}

fn waveform_cache() -> PathBuf {
    #[cfg(feature = "ui-smoke")]
    if let Some(path) = smoke::isolated_dir() {
        return path.join("waveforms-v1");
    }
    config_path().with_file_name("waveforms-v1")
}

impl Recorder {
    pub(super) fn close_player_for_media_change(&mut self, base: &PathBuf) {
        if self
            .pending_player
            .as_ref()
            .is_some_and(|(pending_base, _, _)| pending_base == base)
        {
            self.pending_player = None;
        }
        if self.player_base.as_ref() == Some(base) {
            if let Some(player) = &self.player
                && let Err(error) = player.command(PlaybackCommand::Close)
            {
                self.transcription_status = format!("旧媒体播放器未能关闭，请手动关闭：{error:#}");
            }
            self.player_clear_image = true;
            self.player_selection_anchor = None;
            self.player_selection = None;
            self.player_selection_snapshot.clear();
            self.player_selection_run = None;
            self.cancel_waveforms_for(base);
            self.player_waveform = None;
            self.player_waveform_status.clear();
            self.player_drag_preview = None;
        }
    }
    fn player_matches_transcript_source(&self) -> bool {
        let (Some(player), Some(base), Some(media)) =
            (&self.player, &self.player_base, &self.player_media)
        else {
            return false;
        };
        if self.pending_player.is_some()
            || self.transcript_output.as_ref() != Some(base)
            || self.transcript.source.as_ref() != Some(media)
            || player.closing()
            || matches!(
                player.snapshot().state,
                PlaybackState::Failed | PlaybackState::Closed | PlaybackState::Closing
            )
        {
            return false;
        }
        player.expected_audio_stream() == self.transcript_audio_stream_for(base, media)
            && self
                .transcript_fingerprint_for(base, media)
                .is_ok_and(|fingerprint| player.expected_fingerprint() == fingerprint.as_ref())
    }
    fn transcript_fingerprint_for(
        &self,
        base: &std::path::Path,
        media: &std::path::Path,
    ) -> anyhow::Result<Option<opencap_core::MediaFingerprint>> {
        if self.transcript_output.as_deref() != Some(base)
            || self.transcript.source.as_deref() != Some(media)
        {
            return Ok(None);
        }
        self.transcript
            .run
            .as_ref()
            .and_then(|run| run.extra.get("input_media_fingerprint"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| anyhow::anyhow!("转写的媒体指纹无效，无法确认回听来源：{error}"))
    }
    fn transcript_audio_stream_for(
        &self,
        base: &std::path::Path,
        media: &std::path::Path,
    ) -> Option<u64> {
        if self.transcript_output.as_deref() != Some(base)
            || self.transcript.source.as_deref() != Some(media)
        {
            return None;
        }
        self.transcript
            .run
            .as_ref()?
            .extra
            .get("input_audio")?
            .get("audio_stream_index")?
            .as_u64()
    }
    pub(super) fn cancel_waveform_jobs(&mut self) {
        for job in &self.player_waveform_jobs {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(super) fn cancel_waveforms_for(&mut self, base: &std::path::Path) {
        for job in &self.player_waveform_jobs {
            if job.base.as_path() == base {
                job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    fn start_waveform(&mut self, base: PathBuf, media: PathBuf) {
        self.cancel_waveform_jobs();
        self.player_waveform = None;
        self.player_waveform_status = "正在读取音轨和生成波形…".into();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_media = media.clone();
        let worker_base = base.clone();
        let audio_stream = self
            .player
            .as_ref()
            .and_then(|player| player.expected_audio_stream());
        let expected_fingerprint = self
            .player
            .as_ref()
            .and_then(|player| player.expected_fingerprint().cloned());
        let (tx, result) = crossbeam_channel::bounded(1);
        self.player_waveform_jobs.push(WaveformJob {
            base,
            media,
            audio_stream,
            expected_fingerprint,
            cancel,
            result,
        });
        std::thread::spawn(move || {
            let result =
                opencap_core::managed_meeting_activity(&worker_base).and_then(|_activity| {
                    opencap_asr::load_or_build_waveform_for_stream(
                        &worker_media,
                        &waveform_cache(),
                        audio_stream,
                        &worker_cancel,
                    )
                });
            let _ = tx.send(result);
        });
    }
    fn poll_waveforms(&mut self) -> bool {
        let mut changed = false;
        let mut index = 0;
        while index < self.player_waveform_jobs.len() {
            let result = match self.player_waveform_jobs[index].result.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("波形线程退出")))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            };
            let Some(result) = result else {
                index += 1;
                continue;
            };
            let job = self.player_waveform_jobs.swap_remove(index);
            if job.cancel.load(std::sync::atomic::Ordering::Relaxed)
                || self.player_base.as_ref() != Some(&job.base)
                || self.player_media.as_ref() != Some(&job.media)
                || self.player.as_ref().is_none_or(|player| {
                    player.closing()
                        || matches!(
                            player.snapshot().state,
                            PlaybackState::Failed | PlaybackState::Closed
                        )
                        || player.expected_audio_stream() != job.audio_stream
                        || player.expected_fingerprint() != job.expected_fingerprint.as_ref()
                })
            {
                continue;
            }
            match result {
                Ok(waveform) => {
                    if job.expected_fingerprint.as_ref().is_some_and(|expected| {
                        expected.sha256 != waveform.source_sha256
                            || expected.size_bytes != waveform.source_bytes
                    }) {
                        self.player_waveform = None;
                        self.player_waveform_status = "波形媒体与转写指纹不一致，未显示。".into();
                    } else {
                        self.player_waveform_status =
                            waveform.cache_warning.clone().unwrap_or_default();
                        self.player_waveform = Some(waveform);
                    }
                }
                Err(error) => self.player_waveform_status = format!("波形不可用：{error:#}"),
            }
            changed = true;
        }
        changed
    }
    pub(super) fn restore_player_after_move(
        &mut self,
        base: PathBuf,
        media: PathBuf,
        previous: opencap_media::PlaybackSnapshot,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.player.is_none(), "另一个播放器已打开");
        anyhow::ensure!(media.is_file(), "原媒体当前不可访问");
        let was_playing = matches!(
            previous.state,
            PlaybackState::Playing | PlaybackState::Buffering
        );
        let expected = self.transcript_audio_stream_for(&base, &media);
        let fingerprint = self.transcript_fingerprint_for(&base, &media)?;
        let player = PlaybackSession::open_with_source(media.clone(), expected, fingerprint)?;
        player.command(PlaybackCommand::SetVolume(previous.volume))?;
        player.command(PlaybackCommand::SetSpeed(previous.speed))?;
        player.command(PlaybackCommand::SetRange(previous.range))?;
        player.command(PlaybackCommand::Seek(previous.position_ms))?;
        if was_playing && !self.active() {
            player.command(PlaybackCommand::Play)?;
            self.player_start_pending = true;
        } else if was_playing {
            self.message
                .push_str("；录制正在进行，播放已恢复为暂停状态。");
        }
        self.player_snapshot = Some(player.snapshot());
        self.player_base = Some(base);
        self.player_media = Some(media);
        self.player_drag_preview = None;
        self.player = Some(player);
        if let (Some(base), Some(media)) = (self.player_base.clone(), self.player_media.clone()) {
            self.start_waveform(base, media);
        }
        Ok(())
    }
    pub(super) fn player_slider_event(
        &mut self,
        event: &gpui::component::slider::SliderEvent,
        cx: &mut Context<Self>,
    ) {
        use gpui::component::slider::{SliderEvent, SliderValue};
        let (percent, released) = match event {
            SliderEvent::Change(SliderValue::Single(value)) => (*value, false),
            SliderEvent::Release(SliderValue::Single(value)) => (*value, true),
            _ => return,
        };
        if let Some(duration) = self
            .player_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.duration_ms)
        {
            let target = ((duration as f64) * f64::from(percent.clamp(0., 100.)) / 100.) as u64;
            if released {
                self.player_drag_preview = None;
                if !self.active() {
                    self.send_player(PlaybackCommand::Seek(target));
                }
            } else {
                self.player_drag_preview = Some(target);
            }
            cx.notify();
        }
    }
    pub(super) fn sync_player_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::component::slider::SliderValue;
        if self.player_drag_preview.is_some() {
            return;
        }
        let Some(snapshot) = &self.player_snapshot else {
            return;
        };
        let Some(duration) = snapshot.duration_ms.filter(|duration| *duration > 0) else {
            return;
        };
        let target = (snapshot.position_ms as f64 * 100. / duration as f64).clamp(0., 100.) as f32;
        let current = match self.player_slider.read(cx).value() {
            SliderValue::Single(value) => value,
            _ => 0.,
        };
        if (target - current).abs() > 0.01 {
            self.player_slider
                .update(cx, |slider, cx| slider.set_value(target, window, cx));
        }
    }
    pub(super) fn player_blocks_recording(&self) -> bool {
        self.pending_player.is_some()
            || self.player_start_pending
            || self.player.as_ref().is_some_and(|player| {
                let snapshot = player.snapshot();
                player.closing()
                    || snapshot.verifying_source
                    || (!player.finished() && snapshot.position_pending)
                    || matches!(
                        snapshot.state,
                        PlaybackState::Playing | PlaybackState::Buffering | PlaybackState::Closing
                    )
            })
    }
    pub(super) fn open_internal_player(&mut self, base: PathBuf, media: PathBuf) {
        self.open_internal_player_request(base, media, PlayerRequest::Play);
    }
    fn open_internal_player_at(&mut self, base: PathBuf, media: PathBuf, target: Option<u64>) {
        self.open_internal_player_request(
            base,
            media,
            target.map_or(PlayerRequest::Play, PlayerRequest::At),
        );
    }
    fn open_internal_player_request(
        &mut self,
        base: PathBuf,
        media: PathBuf,
        request: PlayerRequest,
    ) {
        if self.active() {
            self.message = "录制期间不能启动回放，以免系统声音被录入。".into();
            return;
        }
        let command = match &request {
            PlayerRequest::Play => PlaybackCommand::Play,
            PlayerRequest::RetryCurrent => {
                if self.transcript_output.as_ref() != Some(&base)
                    || self.transcript.source.as_ref() != Some(&media)
                {
                    self.transcription_status = "会议或媒体已变化，请重新选择播放。".into();
                    return;
                }
                PlaybackCommand::Play
            }
            PlayerRequest::At(target) => PlaybackCommand::PlayAt(*target),
            PlayerRequest::SpeakerTurn {
                run_id,
                speaker,
                start,
                end,
            } => {
                if self.transcript_output.as_ref() != Some(&base)
                    || self.transcript.source.as_ref() != Some(&media)
                    || self.transcript.run.as_ref().map(|run| run.run_id.as_str())
                        != run_id.as_deref()
                    || !self.transcript.speaker_turns.iter().any(|turn| {
                        turn.speaker == *speaker && turn.start_ms == *start && turn.end_ms == *end
                    })
                {
                    self.transcription_status = "说话人区间或会议已变化，请重新选择。".into();
                    return;
                }
                let Ok(range) = PlaybackRange::new(*start, *end, false) else {
                    self.transcription_status = "说话人区间无效，无法播放。".into();
                    return;
                };
                if self.transcript.gaps.iter().any(|(gap_start, gap_end)| {
                    gap_end > gap_start && *gap_start < *end && *gap_end > *start
                }) {
                    self.transcription_status = "该说话人区间包含转写缺口，请结合原音核对。".into();
                }
                PlaybackCommand::PlayRange(range)
            }
        };
        if !matches!(&request, PlayerRequest::Play)
            || self.player_base.as_ref() != Some(&base)
            || self.player_media.as_ref() != Some(&media)
        {
            self.player_clear_image = true;
        }
        if self.player_base.as_ref() != Some(&base) || self.player_media.as_ref() != Some(&media) {
            self.player_selection_anchor = None;
            self.player_selection = None;
            self.player_selection_snapshot.clear();
            self.player_selection_run = None;
        }
        let expected_audio_stream = self.transcript_audio_stream_for(&base, &media);
        let expected_fingerprint = match self.transcript_fingerprint_for(&base, &media) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                self.message = format!("无法核对原媒体：{error:#}");
                return;
            }
        };
        if let Some(player) = &self.player {
            if self.player_base.as_ref() == Some(&base)
                && self.player_media.as_ref() == Some(&media)
                && player.expected_audio_stream() == expected_audio_stream
                && player.expected_fingerprint() == expected_fingerprint.as_ref()
                && self.pending_player.is_none()
                && !matches!(
                    player.snapshot().state,
                    PlaybackState::Closing | PlaybackState::Closed | PlaybackState::Failed
                )
                && !player.finished()
            {
                if let Err(error) = player.command(command) {
                    self.message = format!("无法播放：{error:#}");
                } else {
                    self.player_start_pending = true;
                }
                return;
            }
            self.pending_player = Some((base, media, request));
            let _ = player.command(PlaybackCommand::Close);
            return;
        }
        match PlaybackSession::open_with_source(
            media.clone(),
            expected_audio_stream,
            expected_fingerprint,
        ) {
            Ok(player) => {
                if let Err(error) = player.command(command) {
                    self.message = format!("无法播放：{error:#}");
                    return;
                }
                self.player_snapshot = Some(player.snapshot());
                self.player_base = Some(base);
                self.player_media = Some(media);
                self.player_start_pending = true;
                self.player = Some(player);
                if let (Some(base), Some(media)) =
                    (self.player_base.clone(), self.player_media.clone())
                {
                    self.start_waveform(base, media);
                }
            }
            Err(error) => self.message = format!("无法打开媒体：{error:#}"),
        }
    }
    pub(super) fn poll_player(&mut self, cx: &mut Context<Self>) -> bool {
        let waveform_changed = self.poll_waveforms();
        let Some(player) = &self.player else {
            return waveform_changed;
        };
        let snapshot = player.snapshot();
        let finished = player.finished();
        let closing = player.closing();
        let expected_fingerprint_present = player.expected_fingerprint().is_some();
        if matches!(
            snapshot.state,
            PlaybackState::Failed | PlaybackState::Closed
        ) {
            for job in &self.player_waveform_jobs {
                if self.player_base.as_ref() == Some(&job.base) {
                    job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            self.player_waveform = None;
            self.player_waveform_status.clear();
        }
        let recorded_stream = self
            .player_base
            .as_deref()
            .zip(self.player_media.as_deref())
            .and_then(|(base, media)| self.transcript_audio_stream_for(base, media));
        let recorded_fingerprint = self
            .player_base
            .as_deref()
            .zip(self.player_media.as_deref())
            .map(|(base, media)| self.transcript_fingerprint_for(base, media))
            .transpose();
        let fingerprint_mismatch = match recorded_fingerprint {
            Ok(Some(Some(ref fingerprint))) => player.expected_fingerprint() != Some(fingerprint),
            Err(_) => true,
            _ => false,
        };
        if ((recorded_stream.is_some() && player.expected_audio_stream() != recorded_stream)
            || fingerprint_mismatch)
            && !finished
            && !closing
            && !matches!(
                snapshot.state,
                PlaybackState::Closing | PlaybackState::Closed
            )
        {
            let _ = player.command(PlaybackCommand::Close);
            if let Some(base) = self.player_base.clone() {
                self.cancel_waveforms_for(&base);
            }
            self.player_waveform = None;
            self.player_waveform_status.clear();
            if let Some(image) = self.player_image.take() {
                cx.drop_image(image, None);
            }
            self.transcription_status = "正在关闭来源不匹配的播放，请重新从当前逐字稿打开。".into();
            self.transcript_view
                .update(cx, |view, cx| view.set_playback(None, false, cx));
            return true;
        }
        // A very short clip may reach Ended, or a queued Pause may already
        // have run, before the UI observes Playing. The sequence check is the
        // acknowledgement; the visible state need not pass through Playing.
        if finished || (!snapshot.position_pending && !snapshot.verifying_source) {
            self.player_start_pending = false;
        }
        let mut changed = waveform_changed || self.player_snapshot.as_ref() != Some(&snapshot);
        let generation_changed = self
            .player_snapshot
            .as_ref()
            .is_some_and(|old| old.generation != snapshot.generation);
        if self.player_clear_image
            || closing
            || snapshot.position_pending
            || matches!(
                snapshot.state,
                PlaybackState::Closing | PlaybackState::Closed | PlaybackState::Failed
            )
            || (generation_changed && snapshot.state != PlaybackState::Paused)
        {
            if let Some(image) = self.player_image.take() {
                cx.drop_image(image, None);
                changed = true;
            }
            changed |= self.player_clear_image;
            self.player_clear_image = false;
        }
        if let Some(frame) = player.take_video_frame()
            && !closing
            && !snapshot.position_pending
            && !matches!(
                snapshot.state,
                PlaybackState::Closing | PlaybackState::Closed | PlaybackState::Failed
            )
            && frame.generation == snapshot.generation
            && let Some(buffer) = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)
        {
            let image = std::sync::Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            if let Some(old) = self.player_image.replace(image) {
                cx.drop_image(old, None);
            }
            changed = true;
        }
        let same_transcript_source = self.player_matches_transcript_source();
        let current_run = self.transcript.run.as_ref().map(|run| run.run_id.clone());
        if (!same_transcript_source
            || self.player_selection_run.as_ref() != Some(&current_run)
            || (self.player_selection_anchor.is_some() && !self.player_selection_is_current()))
            && self.player_selection_anchor.take().is_some()
        {
            self.player_selection_run = None;
            self.player_selection_snapshot.clear();
            self.transcription_status = "起始句段已变化，请重新选择范围。".into();
            changed = true;
        }
        if self.player_selection.is_some()
            && (!same_transcript_source || !self.player_selection_is_current())
        {
            self.player_selection = None;
            self.player_selection_snapshot.clear();
            self.player_selection_run = None;
            self.send_player(PlaybackCommand::SetRange(None));
            self.transcription_status = "所选文字或句段时间已变化，循环选区已清除。".into();
            changed = true;
        }
        self.player_snapshot = Some(snapshot.clone());
        let position = (same_transcript_source
            && !closing
            && !snapshot.verifying_source
            && !snapshot.position_pending
            && !matches!(
                snapshot.state,
                PlaybackState::Failed | PlaybackState::Closing | PlaybackState::Closed
            ))
        .then_some(snapshot.position_ms);
        self.transcript_view.update(cx, |view, cx| {
            view.set_playback(
                position,
                expected_fingerprint_present && snapshot.source_verified,
                cx,
            )
        });
        if finished && (self.pending_player.is_some() || snapshot.state == PlaybackState::Closed) {
            self.player = None;
            self.player_snapshot = None;
            self.player_base = None;
            self.player_media = None;
            self.cancel_waveform_jobs();
            self.player_waveform = None;
            if let Some(image) = self.player_image.take() {
                cx.drop_image(image, None);
            }
            if let Some((base, media, request)) = self.pending_player.take() {
                self.open_internal_player_request(base, media, request);
            }
            return true;
        }
        changed
    }
    pub(super) fn seek_player_from_segment(&mut self, milliseconds: u64, cx: &mut Context<Self>) {
        if self.active() {
            self.transcription_status = "录制期间不能启动回放，以免声音被录入。".into();
        } else if let (Some(base), Some(media)) = (
            self.transcript_output.clone(),
            self.transcript.source.clone(),
        ) {
            self.open_internal_player_at(base, media, Some(milliseconds));
        } else {
            self.transcription_status = "当前逐字稿没有可播放的原媒体。".into();
        }
        cx.notify();
    }
    pub(super) fn play_speaker_turn(
        &mut self,
        speaker: u32,
        start: u64,
        end: u64,
        cx: &mut Context<Self>,
    ) {
        if let (Some(base), Some(media)) = (
            self.transcript_output.clone(),
            self.transcript.source.clone(),
        ) {
            let request = PlayerRequest::SpeakerTurn {
                run_id: self.transcript.run.as_ref().map(|run| run.run_id.clone()),
                speaker,
                start,
                end,
            };
            self.open_internal_player_request(base, media, request);
        } else {
            self.transcription_status = "当前逐字稿没有可播放的原媒体。".into();
        }
        cx.notify();
    }
    fn playback_row_is_current(&self, selection: &super::transcript_view::PlaybackSeek) -> bool {
        let run_id = self.transcript.run.as_ref().map(|run| run.run_id.as_str());
        let mut matches = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.id == selection.segment_id);
        let row = matches.next();
        run_id == selection.run_id.as_deref()
            && matches.next().is_none()
            && row.is_some_and(|segment| {
                segment.revision == selection.revision
                    && segment.start_ms == selection.milliseconds
                    && segment.end_ms == selection.end_ms
                    && segment.end_ms > segment.start_ms
                    && segment.is_confirmed()
                    && segment.text.as_str() == selection.text.as_ref()
            })
    }
    pub(super) fn seek_player_from_row(
        &mut self,
        selection: &super::transcript_view::PlaybackSeek,
        cx: &mut Context<Self>,
    ) {
        if self.playback_row_is_current(selection) {
            self.seek_player_from_segment(selection.milliseconds, cx);
        } else {
            self.transcription_status = "识别运行或句段已变化，请重新选择后播放。".into();
            cx.notify();
        }
    }
    pub(super) fn select_player_segment_from_row(
        &mut self,
        selection: &super::transcript_view::PlaybackSeek,
        cx: &mut Context<Self>,
    ) {
        if self.playback_row_is_current(selection) {
            self.select_player_segment(selection.segment_id, cx);
        } else {
            self.transcription_status = "识别运行或句段已变化，请重新选择范围。".into();
            cx.notify();
        }
    }
    pub(super) fn seek_player_from_token(
        &mut self,
        selection: &super::transcript_view::PlaybackTokenSeek,
        cx: &mut Context<Self>,
    ) {
        let mut matching = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.id == selection.segment_id);
        let segment = matching.next();
        let valid = matching.next().is_none()
            && segment
                .and_then(|segment| {
                    opencap_asr::exact_token_starts(segment, self.transcript.run.as_ref())
                })
                .is_some_and(|mapping| {
                    mapping.source_run_id == selection.source_run_id
                        && mapping.text_revision == selection.text_revision
                        && mapping.text_sha256 == selection.text_sha256
                        && mapping.tokens.iter().any(|token| {
                            token.start_ms == selection.start_ms
                                && token.text_range == selection.text_range
                        })
                });
        let source_verified_at_transcription = self.transcript.run.as_ref().is_some_and(|run| {
            run.extra
                .get("input_integrity")
                .and_then(|value| value.as_str())
                == Some("hash_matched_before_and_after")
                && run
                    .extra
                    .get("input_media_fingerprint")
                    .cloned()
                    .and_then(|value| {
                        serde_json::from_value::<opencap_core::MediaFingerprint>(value).ok()
                    })
                    .is_some()
                && run
                    .extra
                    .get("input_audio")
                    .and_then(|value| value.get("audio_stream_index"))
                    .and_then(|value| value.as_u64())
                    .is_some()
        });
        if valid && source_verified_at_transcription {
            self.seek_player_from_segment(selection.start_ms, cx);
        } else if valid {
            self.transcription_status =
                "原媒体或音轨缺少完整核对记录，不能按 token 时间定位。".into();
            cx.notify();
        } else {
            self.transcription_status = "识别文字或 token 时间已变化，请重新展开后定位。".into();
            cx.notify();
        }
    }
    pub(super) fn play_aligned_word(
        &mut self,
        selection: &super::transcript_view::PlaybackWordRange,
        cx: &mut Context<Self>,
    ) {
        let mut matching = self
            .transcript
            .segments
            .iter()
            .filter(|segment| segment.id == selection.segment_id);
        let segment = matching.next();
        let valid = matching.next().is_none()
            && segment
                .and_then(|segment| {
                    opencap_asr::validated_word_alignment(segment, self.transcript.run.as_ref())
                })
                .is_some_and(|alignment| {
                    alignment.source_run_id == selection.source_run_id
                        && alignment.alignment_run_id == selection.alignment_run_id
                        && alignment.text_revision == selection.text_revision
                        && alignment.text_sha256 == selection.text_sha256
                        && alignment.words.contains(&selection.word)
                });
        if !valid {
            self.transcription_status = "词时间或逐字稿已变化，请重新展开时间详情。".into();
        } else if self.active() || !self.player_matches_transcript_source() {
            self.transcription_status = "请先停止录制并打开当前逐字稿的原媒体。".into();
        } else if let Some(player) = &self.player {
            let snapshot = player.snapshot();
            if player.expected_fingerprint().is_none()
                || snapshot.verifying_source
                || !snapshot.source_verified
            {
                self.transcription_status = "原媒体身份尚未核实，无法使用词级范围播放。".into();
            } else if let Ok(range) =
                PlaybackRange::new(selection.word.start_ms, selection.word.end_ms, false)
            {
                match player.command(PlaybackCommand::PlayRange(range)) {
                    Ok(()) => {
                        self.player_selection = None;
                        self.player_selection_anchor = None;
                        self.player_selection_snapshot.clear();
                        self.player_selection_run = None;
                        self.player_clear_image = true;
                        self.player_start_pending = true;
                    }
                    Err(error) => {
                        self.transcription_status = format!("无法播放词范围，请重试：{error:#}")
                    }
                }
            }
        }
        cx.notify();
    }
    pub(super) fn select_player_segment(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.player_matches_transcript_source() {
            self.transcription_status = "请先打开当前逐字稿的原媒体。".into();
        } else if let Some(first) = self.player_selection_anchor.take() {
            let current_run = self.transcript.run.as_ref().map(|run| run.run_id.clone());
            if self.player_selection_run.as_ref() != Some(&current_run)
                || !self.player_selection_is_current()
            {
                self.player_selection_run = None;
                self.player_selection_snapshot.clear();
                self.transcription_status = "识别运行已变化，请重新选择范围。".into();
                cx.notify();
                return;
            }
            self.select_player_range(first, id, cx);
            return;
        } else {
            let mut matching = self
                .transcript
                .segments
                .iter()
                .filter(|segment| segment.id == id);
            let Some(segment) = matching.next() else {
                self.transcription_status = "起始句段不存在，请重新选择。".into();
                cx.notify();
                return;
            };
            if matching.next().is_some()
                || !segment.is_confirmed()
                || segment.end_ms <= segment.start_ms
            {
                self.transcription_status = "起始句段不唯一或尚未定稿，请重新选择。".into();
                cx.notify();
                return;
            }
            let anchor = (
                segment.id,
                segment.revision,
                segment.start_ms,
                segment.end_ms,
                segment.text.clone(),
            );
            let had_range = self.player_selection.is_some()
                || self
                    .player_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.range.is_some());
            self.player_selection_anchor = Some(id);
            self.player_selection = None;
            self.player_selection_snapshot.clear();
            self.player_selection_snapshot.push(anchor);
            self.player_selection_run =
                Some(self.transcript.run.as_ref().map(|run| run.run_id.clone()));
            if had_range {
                self.send_player(PlaybackCommand::SetRange(None));
            }
            self.transcription_status = "已选择起始句段，再点另一句的“选段”确定范围。".into();
        }
        cx.notify();
    }
    pub(super) fn select_player_range_from_rows(
        &mut self,
        selection: &super::transcript_view::PlaybackRangeSelection,
        cx: &mut Context<Self>,
    ) {
        let run_id = self.transcript.run.as_ref().map(|run| run.run_id.as_str());
        let bounds = selection.rows.first().zip(selection.rows.last());
        let valid = run_id == selection.run_id.as_deref()
            && bounds.is_some_and(|(first, _)| {
                let Some(index) = self
                    .transcript
                    .segments
                    .iter()
                    .position(|segment| segment.id == first.id)
                else {
                    return false;
                };
                let Some(end) = index.checked_add(selection.rows.len()) else {
                    return false;
                };
                self.transcript
                    .segments
                    .get(index..end)
                    .is_some_and(|segments| {
                        segments.iter().zip(&selection.rows).all(|(segment, row)| {
                            segment.id == row.id
                                && segment.revision == row.revision
                                && segment.start_ms == row.start_ms
                                && segment.end_ms == row.end_ms
                                && segment.end_ms > segment.start_ms
                                && segment.is_confirmed()
                                && segment.text.as_str() == row.text.as_ref()
                        })
                    })
            });
        if valid {
            let (first, last) = bounds.expect("已校验非空选区");
            self.select_player_range(first.id, last.id, cx);
        } else {
            self.transcription_status = "所选文字或识别运行已变化，请重新拖选。".into();
            cx.notify();
        }
    }
    pub(super) fn select_player_range(&mut self, first: u64, last: u64, cx: &mut Context<Self>) {
        if !self.player_matches_transcript_source() {
            self.transcription_status = "请先打开当前逐字稿的原媒体。".into();
            cx.notify();
            return;
        }
        let had_range = self.player_selection.is_some()
            || self
                .player_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.range.is_some());
        self.player_selection_anchor = None;
        self.player_selection = None;
        self.player_selection_snapshot.clear();
        self.player_selection_run = None;
        if had_range {
            self.send_player(PlaybackCommand::SetRange(None));
        }
        let a = self
            .transcript
            .segments
            .iter()
            .position(|segment| segment.id == first);
        let b = self
            .transcript
            .segments
            .iter()
            .position(|segment| segment.id == last);
        if let (Some(a), Some(b)) = (a, b) {
            let rows = &self.transcript.segments[a.min(b)..=a.max(b)];
            let ids: std::collections::HashSet<_> = rows.iter().map(|segment| segment.id).collect();
            if ids.len() != rows.len()
                || self
                    .transcript
                    .segments
                    .iter()
                    .filter(|segment| ids.contains(&segment.id))
                    .count()
                    != ids.len()
            {
                self.transcription_status = "所选句段编号重复，无法可靠确定播放范围。".into();
                cx.notify();
                return;
            }
            if rows
                .iter()
                .any(|segment| !segment.is_confirmed() || segment.end_ms <= segment.start_ms)
            {
                self.transcription_status =
                    "选区包含尚未定稿、正在校正或时间无效的句段，请重新选择。".into();
                cx.notify();
                return;
            }
            let start = rows
                .iter()
                .map(|segment| segment.start_ms)
                .min()
                .unwrap_or(0);
            let end = rows
                .iter()
                .map(|segment| segment.end_ms)
                .max()
                .unwrap_or(start);
            if start < end {
                self.player_selection_anchor = None;
                self.player_selection = Some((start, end));
                self.player_selection_run =
                    Some(self.transcript.run.as_ref().map(|run| run.run_id.clone()));
                self.player_selection_snapshot = rows
                    .iter()
                    .map(|segment| {
                        (
                            segment.id,
                            segment.revision,
                            segment.start_ms,
                            segment.end_ms,
                            segment.text.clone(),
                        )
                    })
                    .collect();
                self.transcription_status = format!(
                    "已选择完整句段 {}–{}，可播放或循环；文字选择保持不变。{}",
                    opencap_asr::timestamp(start, '.'),
                    opencap_asr::timestamp(end, '.'),
                    if self
                        .transcript
                        .gaps
                        .iter()
                        .any(|(gap_start, gap_end)| gap_end > gap_start
                            && *gap_start < end
                            && *gap_end > start)
                    {
                        "该范围包含转写缺口，请结合原音核对。"
                    } else {
                        ""
                    }
                );
            } else {
                self.transcription_status = "所选句段没有有效的时间范围。".into();
            }
        } else {
            self.transcription_status = "所选句段已变化，请重新选择。".into();
        }
        cx.notify();
    }
    fn play_player_selection(&mut self, repeat: bool) {
        let Some((start, end)) = self.player_selection else {
            return;
        };
        if self.active() || !self.player_matches_transcript_source() {
            self.message = "请在未录制时播放当前会议的选段。".into();
            return;
        }
        if !self.player_selection_is_current() {
            self.player_selection = None;
            self.player_selection_snapshot.clear();
            self.player_selection_run = None;
            self.send_player(PlaybackCommand::SetRange(None));
            self.transcription_status = "所选文字或句段时间已变化，请重新选择。".into();
            return;
        }
        let Ok(range) = PlaybackRange::new(start, end, repeat) else {
            return;
        };
        let Some(player) = &self.player else {
            self.message = "播放器不可用，请重新打开媒体。".into();
            return;
        };
        if player.closing() || self.pending_player.is_some() {
            self.message = "播放器正在关闭或切换，请稍候。".into();
            return;
        }
        let snapshot = player.snapshot();
        if snapshot.verifying_source
            || (player.expected_fingerprint().is_some() && !snapshot.source_verified)
        {
            self.message = "正在核对原媒体，完成后才能播放所选范围。".into();
            return;
        }
        let queued = player.command(PlaybackCommand::PlayRange(range));
        match queued {
            Ok(()) => {
                self.player_clear_image = true;
                self.player_start_pending = true;
                if self.transcript.gaps.iter().any(|(gap_start, gap_end)| {
                    gap_end > gap_start && *gap_start < end && *gap_end > start
                }) {
                    self.transcription_status =
                        "所选范围包含转写缺口；正在播放原媒体，请结合原音核对。".into();
                }
            }
            Err(error) => self.message = format!("无法完整排入选段播放，请重试：{error:#}"),
        }
    }
    fn player_selection_is_current(&self) -> bool {
        if self.player_selection_run.as_ref()
            != Some(&self.transcript.run.as_ref().map(|run| run.run_id.clone()))
        {
            return false;
        }
        let Some(first) = self.player_selection_snapshot.first() else {
            return false;
        };
        let ids: std::collections::HashSet<_> = self
            .player_selection_snapshot
            .iter()
            .map(|row| row.0)
            .collect();
        if ids.len() != self.player_selection_snapshot.len()
            || self
                .transcript
                .segments
                .iter()
                .filter(|segment| ids.contains(&segment.id))
                .count()
                != ids.len()
        {
            return false;
        }
        let Some(index) = self
            .transcript
            .segments
            .iter()
            .position(|segment| segment.id == first.0)
        else {
            return false;
        };
        let end = index.saturating_add(self.player_selection_snapshot.len());
        self.transcript
            .segments
            .get(index..end)
            .is_some_and(|segments| {
                segments
                    .iter()
                    .zip(&self.player_selection_snapshot)
                    .all(|(segment, old)| {
                        segment.is_confirmed()
                            && segment.end_ms > segment.start_ms
                            && (
                                segment.id,
                                segment.revision,
                                segment.start_ms,
                                segment.end_ms,
                                segment.text.as_str(),
                            ) == (old.0, old.1, old.2, old.3, old.4.as_str())
                    })
            })
    }
    pub(super) fn send_player(&mut self, command: PlaybackCommand) {
        if !matches!(command, PlaybackCommand::Close)
            && self.player.as_ref().is_some_and(|player| player.closing())
        {
            self.message = "播放器正在关闭，请稍候。".into();
            return;
        }
        if self.pending_player.is_some() && !matches!(command, PlaybackCommand::Close) {
            self.message = "正在切换媒体，请等待当前播放器关闭。".into();
            return;
        }
        if self.active() && matches!(command, PlaybackCommand::Play) {
            self.message = "录制期间不能启动回放，以免系统声音被录入。".into();
            return;
        }
        if matches!(
            command,
            PlaybackCommand::Seek(_)
                | PlaybackCommand::SetRange(_)
                | PlaybackCommand::SetSpeed(_)
                | PlaybackCommand::Close
        ) {
            self.player_clear_image = true;
        }
        if let Some(player) = &self.player {
            match player.command(command) {
                Ok(()) if matches!(command, PlaybackCommand::Play) => {
                    self.player_start_pending = true
                }
                Ok(()) => (),
                Err(error) => self.message = format!("播放操作失败：{error:#}"),
            }
        }
    }
    pub(super) fn player_controls(&self, cx: &Context<Self>) -> Stateful<Div> {
        let Some(snapshot) = &self.player_snapshot else {
            return div().id("player-controls-empty");
        };
        let position = self.player_drag_preview.unwrap_or(snapshot.position_ms);
        let playing = self.player_start_pending
            || matches!(
                snapshot.state,
                PlaybackState::Playing | PlaybackState::Buffering
            );
        let duration = snapshot
            .duration_ms
            .map(|ms| opencap_asr::timestamp(ms, '.'))
            .unwrap_or_else(|| "--:--".into());
        let status = if snapshot.state == PlaybackState::Closing {
            "正在关闭"
        } else if snapshot.verifying_source {
            "正在核对原媒体"
        } else {
            match snapshot.state {
                PlaybackState::Paused => "已暂停",
                PlaybackState::Buffering => "缓冲中",
                PlaybackState::Playing => "播放中",
                PlaybackState::Ended => "已结束",
                PlaybackState::Failed => "播放失败",
                PlaybackState::Closing => "正在关闭",
                PlaybackState::Closed => "已关闭",
            }
        };
        let error = snapshot.error.clone().unwrap_or_default();
        let video_error = snapshot.video_error.clone().unwrap_or_default();
        let source_status = if self.player_base == self.transcript_output
            && self.player_media.as_deref() == self.transcript.source.as_deref()
        {
            self.player.as_ref().map(|player| {
                if matches!(
                    snapshot.state,
                    PlaybackState::Closing | PlaybackState::Closed
                ) {
                    "播放器正在关闭，已请求停止来源核对。"
                } else if snapshot.state == PlaybackState::Failed {
                    "来源或播放未就绪，请查看错误。"
                } else if snapshot.verifying_source {
                    "正在核对转写的原媒体指纹。"
                } else if snapshot.source_verified
                    && self.transcript.run.as_ref().is_none_or(|run| {
                        run.extra
                            .get("input_integrity")
                            .and_then(|value| value.as_str())
                            != Some("hash_matched_before_and_after")
                    })
                {
                    "当前文件匹配已记录的媒体指纹；原识别期间的媒体完整性未确认。"
                } else if snapshot.source_verified {
                    "播放前已核对媒体指纹；播放期间的外部修改仍需留意。"
                } else if player.expected_fingerprint().is_some() {
                    "媒体指纹未完成核对。"
                } else {
                    "历史运行未记录媒体指纹，无法确认当前文件内容身份。"
                }
            })
        } else {
            None
        };
        let volume = snapshot.volume;
        let retry_allowed = self
            .player_base
            .as_deref()
            .zip(self.player_media.as_deref())
            .is_some_and(|(base, media)| {
                (self.transcript_output.as_deref() == Some(base)
                    && self.transcript.source.as_deref() == Some(media))
                    || self.can_retry_library_player(base, media)
            });
        ui::card()
            .id("player-keyboard-scope")
            .track_focus(&self.player_keyboard_focus)
            .focus(|style| style.border_color(rgb(ACCENT)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !this.player_keyboard_focus.is_focused(window) || event.keystroke.modifiers.modified() || this.active() {
                    return;
                }
                let Some(snapshot) = this.player_snapshot.clone() else { return; };
                if matches!(snapshot.state, PlaybackState::Failed | PlaybackState::Closing | PlaybackState::Closed) { return; }
                let handled = match event.keystroke.key.as_str() {
                    "space" if !event.is_held => {
                        this.send_player(if this.player_start_pending || matches!(snapshot.state, PlaybackState::Playing | PlaybackState::Buffering) {
                            PlaybackCommand::Pause
                        } else { PlaybackCommand::Play });
                        true
                    }
                    "left" => { this.send_player(PlaybackCommand::Seek(snapshot.position_ms.saturating_sub(10_000))); true }
                    "right" => { this.send_player(PlaybackCommand::Seek(snapshot.position_ms.saturating_add(10_000))); true }
                    "up" => { this.send_player(PlaybackCommand::SetVolume((snapshot.volume + 0.1).min(1.))); true }
                    "down" => { this.send_player(PlaybackCommand::SetVolume((snapshot.volume - 0.1).max(0.))); true }
                    _ => false,
                };
                if handled { cx.stop_propagation(); cx.notify(); }
            }))
            .p_3()
            .gap_2()
            .child(label(format!(
                "内置回放 · {status} · {} / {duration}",
                opencap_asr::timestamp(position, '.')
            )))
            .when_some(source_status, |view, status| view.child(label(status)))
            .when(self.player_matches_transcript_source(), |d|
                d.child(label("选择两句“选段”可回听或循环连续范围。")))
            .child(button("focus-player-shortcuts", "键盘控制", false).ghost()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.player_keyboard_focus.focus(window, cx);
                    this.message = "播放器键盘控制：空格播放或暂停，左右跳转 10 秒，上下调整音量。按 Tab 返回其他控件。".into();
                    cx.notify();
                })))
            .child(
                gpui::component::slider::Slider::new(&self.player_slider)
                    .w_full()
                    .disabled(
                        self.active()
                            || snapshot.duration_ms.is_none()
                            || matches!(
                                snapshot.state,
                                PlaybackState::Failed
                                    | PlaybackState::Closing
                                    | PlaybackState::Closed
                            ),
                    ),
            )
            .when(snapshot.has_video, |d| {
                d.child(
                    div()
                        .h(px(200.))
                        .w_full()
                        .bg(rgb(0x090a0c))
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when_some(self.player_image.clone(), |d, image| {
                            d.child(img(image).size_full().object_fit(ObjectFit::Contain))
                        })
                        .when(self.player_image.is_none(), |d| {
                            d.child(label("等待视频画面…"))
                        }),
                )
            })
            .when(!snapshot.has_video, |d| {
                if let Some(waveform) = &self.player_waveform {
                    let count = waveform.peaks.len().min(100);
                    let timeline_ms = snapshot.duration_ms.filter(|duration| *duration > 0)
                        .unwrap_or(waveform.duration_ms.max(1));
                    let played = (position as f64 / timeline_ms as f64).clamp(0., 1.);
                    d.child(
                        div()
                            .h(px(58.))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(1.))
                            .overflow_hidden()
                            .children((0..count).map(|index| {
                                // Place every bar on the player's media clock,
                                // including silence after an early audio EOF.
                                let start_ms = (timeline_ms as u128 * index as u128 / count as u128) as u64;
                                let end_ms = (timeline_ms as u128 * (index + 1) as u128 / count as u128) as u64;
                                let begin = (start_ms / waveform.bucket_ms).min(waveform.peaks.len() as u64) as usize;
                                let end = end_ms.saturating_add(waveform.bucket_ms - 1)
                                    .checked_div(waveform.bucket_ms).unwrap_or(0)
                                    .min(waveform.peaks.len() as u64) as usize;
                                let peak = if begin < end {
                                    waveform.peaks[begin..end].iter().copied().fold(0., f32::max)
                                } else { 0. };
                                div()
                                    .flex_1()
                                    .min_w(px(1.))
                                    .h(px(3. + 48. * peak.sqrt()))
                                    .rounded_sm()
                                    .bg(rgb(if (index as f64 / count as f64) < played {
                                        ACCENT
                                    } else {
                                        BORDER
                                    }))
                            })),
                    )
                } else {
                    d.child(label(self.player_waveform_status.clone()))
                }
            })
            .when(self.player_waveform.is_some() && !self.player_waveform_status.is_empty(), |d| {
                d.child(label(self.player_waveform_status.clone()))
            })
            .when(!error.is_empty(), |d| d.child(label(error)))
            .when(snapshot.state == PlaybackState::Failed, |d| {
                d.child(button("retry-player", "重新打开媒体", false)
                    .disabled(self.active() || self.pending_player.is_some()
                        || !retry_allowed)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.active() && this.pending_player.is_none()
                            && let (Some(base), Some(media)) = (this.player_base.clone(), this.player_media.clone())
                            && this.player.as_ref().is_some_and(|player| player.snapshot().state == PlaybackState::Failed)
                        {
                            let request = if this.transcript_output.as_ref() == Some(&base)
                                && this.transcript.source.as_ref() == Some(&media) {
                                Some(PlayerRequest::RetryCurrent)
                            } else if this.can_retry_library_player(&base, &media) {
                                Some(PlayerRequest::Play)
                            } else {
                                None
                            };
                            if let Some(request) = request {
                                this.open_internal_player_request(base, media, request);
                            } else {
                                this.message = "会议或媒体已变化，请重新选择播放。".into();
                            }
                        }
                        cx.notify();
                    })))
            })
            .when(!video_error.is_empty(), |d| {
                d.child(label(format!("视频画面不可用：{video_error}")))
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "player-toggle",
                            if playing { "暂停" } else { "播放" },
                            false,
                        )
                        .disabled(
                            self.active()
                                || matches!(
                                    snapshot.state,
                                    PlaybackState::Failed
                                        | PlaybackState::Closing
                                        | PlaybackState::Closed
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_player(if playing {
                                PlaybackCommand::Pause
                            } else {
                                PlaybackCommand::Play
                            });
                            cx.notify();
                        })),
                    )
                    .child(
                        button("player-back", "后退 10 秒", false)
                            .disabled(self.active())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_player(PlaybackCommand::Seek(
                                    position.saturating_sub(10_000),
                                ));
                                cx.notify();
                            })),
                    )
                    .child(
                        button("player-forward", "前进 10 秒", false)
                            .disabled(self.active())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_player(PlaybackCommand::Seek(
                                    position.saturating_add(10_000),
                                ));
                                cx.notify();
                            })),
                    )
                    .child(
                        button("player-volume-down", "音量 −", false)
                            .disabled(volume <= 0.)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_player(PlaybackCommand::SetVolume(
                                    (volume - 0.1).max(0.),
                                ));
                                cx.notify();
                            })),
                    )
                    .child(label(format!("{:.0}%", volume * 100.)))
                    .child(
                        button("player-volume-up", "音量 +", false)
                            .disabled(volume >= 1.)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_player(PlaybackCommand::SetVolume(
                                    (volume + 0.1).min(1.),
                                ));
                                cx.notify();
                            })),
                    )
                    .children([0.75f32, 1.0, 1.25, 1.5, 2.0].into_iter().enumerate().map(
                        |(index, speed)| {
                            button(
                                ("player-speed", index),
                                format!("{speed}×"),
                                (snapshot.speed - speed).abs() < 0.001,
                            )
                            .disabled(self.active())
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.send_player(PlaybackCommand::SetSpeed(speed));
                                    cx.notify();
                                },
                            ))
                        },
                    ))
                    .when(
                        self.player_selection.is_some()
                            && self.player_matches_transcript_source(),
                        |d| {
                            d.child(
                                button("player-selection", "播放所选", false)
                                    .disabled(self.active())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.play_player_selection(false);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button("player-loop-selection", "循环所选", false)
                                    .disabled(self.active())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.play_player_selection(true);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button("player-clear-selection", "清除选段", false)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player_selection = None;
                                        this.player_selection_anchor = None;
                                        this.player_selection_snapshot.clear();
                                        this.player_selection_run = None;
                                        this.send_player(PlaybackCommand::SetRange(None));
                                        cx.notify();
                                    })),
                            )
                        },
                    )
                    .child(button("player-close", "关闭播放", false).ghost().on_click(
                        cx.listener(|this, _, _, cx| {
                            this.pending_player = None;
                            this.cancel_waveform_jobs();
                            this.player_selection = None;
                            this.player_selection_anchor = None;
                            this.player_selection_snapshot.clear();
                            this.player_selection_run = None;
                            this.send_player(PlaybackCommand::Close);
                            cx.notify();
                        }),
                    )),
            )
    }
}
