//! Common streaming experiment interface. A snapshot is not a word timestamp.
use anyhow::{Result, ensure};
use opencap_core::PcmAudio;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingTextEvent {
    /// `segment` is a revisable segment. NeMo `snapshot` is a hypothesis for
    /// the current utterance; final snapshots must be accumulated in order.
    pub kind: String,
    pub segment_id: Option<u64>,
    pub revision: Option<u64>,
    pub text: String,
    pub finalized: bool,
    /// Backend-reported processed time; not a word alignment or display latency.
    pub processed_ms: Option<u64>,
    /// Backend observations only; these are not automatically mapped to a
    /// displayed text revision or treated as forced alignment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<StreamingWordTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_token_timing: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_error: Option<String>,
    /// Actual worker configuration when available; never inferred from request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_provenance: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingWordTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub method: String,
}

pub trait StreamingAsrBackend {
    fn push(&mut self, audio: &PcmAudio) -> Result<Vec<StreamingTextEvent>>;
    fn finish(&mut self) -> Result<Vec<StreamingTextEvent>>;
}

/// Adapter for the existing Zipformer live worker. It does not alter the
/// default product backend and uses the same PCM interface as the NeMo POC.
pub struct SherpaStreamingAsr {
    session: crate::AsrSession,
    input: Option<crossbeam_channel::Sender<PcmAudio>>,
    processed_ms: Option<u64>,
    input_end_ms: u64,
}
impl SherpaStreamingAsr {
    pub fn new(root: PathBuf) -> Result<Self> {
        let session = crate::AsrSession::live_with_revision(root, String::new(), false)?;
        let input = Some(session.audio_sender());
        Ok(Self {
            session,
            input,
            processed_ms: None,
            input_end_ms: 0,
        })
    }
    fn collect(
        &mut self,
        event: crate::AsrEvent,
        output: &mut Vec<StreamingTextEvent>,
    ) -> Result<bool> {
        match event {
            crate::AsrEvent::Segment(segment) => output.push(StreamingTextEvent {
                kind: "segment".into(),
                segment_id: Some(segment.id),
                revision: Some(segment.revision),
                text: segment.text,
                finalized: segment.finalized,
                processed_ms: self.processed_ms,
                words: Vec::new(),
                raw_token_timing: segment.extra.get("raw_token_timing").cloned(),
                timing_error: None,
                run_provenance: None,
            }),
            crate::AsrEvent::Processed(ms) => self.processed_ms = Some(ms),
            crate::AsrEvent::Gap(start, end) => {
                anyhow::bail!("实时识别出现缺口 {start}–{end}ms，实验不能当作完整输入")
            }
            crate::AsrEvent::Error(error) => anyhow::bail!("实时识别失败：{error}"),
            crate::AsrEvent::Finished(transcript) => {
                ensure!(transcript.is_complete(), "基线识别未完整结束");
                output.push(StreamingTextEvent {
                    kind: "final_snapshot".into(),
                    segment_id: None,
                    revision: None,
                    text: transcript
                        .segments
                        .iter()
                        .filter(|s| s.is_confirmed())
                        .map(|s| s.text.as_str())
                        .collect::<Vec<_>>()
                        .join(""),
                    finalized: true,
                    processed_ms: self.processed_ms,
                    words: Vec::new(),
                    raw_token_timing: None,
                    timing_error: None,
                    run_provenance: transcript.run.as_ref().map(|run| serde_json::json!({
                        "model_id":run.model_id, "model_sha256":run.model_sha256,
                        "model_fingerprint":run.extra.get("model_fingerprint"),
                        "punctuation_requested":run.punctuation_requested, "punctuation_effective":run.punctuation_effective,
                        "punctuation_fingerprint":run.extra.get("punctuation_fingerprint"),
                        "punctuation_fingerprint_verification":run.extra.get("punctuation_fingerprint_verification"),
                        "model_fingerprint_verification":run.extra.get("model_fingerprint_verification"),
                        "provider":run.provider, "threads":run.threads
                    })),
                });
                return Ok(true);
            }
            _ => (),
        }
        Ok(false)
    }
    fn drain(&mut self) -> Result<Vec<StreamingTextEvent>> {
        let mut output = Vec::new();
        while let Ok(event) = self.session.events.try_recv() {
            self.collect(event, &mut output)?;
        }
        Ok(output)
    }
}
impl StreamingAsrBackend for SherpaStreamingAsr {
    fn push(&mut self, audio: &PcmAudio) -> Result<Vec<StreamingTextEvent>> {
        ensure!(
            audio.channels > 0 && audio.sample_rate > 0 && audio.pts_us >= 0,
            "无效音频块"
        );
        let frames = audio.samples.len() / usize::from(audio.channels);
        self.input_end_ms = (audio.pts_us as u64 / 1000)
            .saturating_add((frames as u64).saturating_mul(1000) / u64::from(audio.sample_rate));
        self.input
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("实时识别流已结束"))?
            .send_timeout(audio.clone(), Duration::from_secs(1))
            .map_err(|_| anyhow::anyhow!("实时识别输入队列积压或已关闭"))?;
        self.drain()
    }
    fn finish(&mut self) -> Result<Vec<StreamingTextEvent>> {
        self.input.take();
        self.session.finish_at(self.input_end_ms);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut output = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "实时识别收尾超时，不能报告完整结果");
            match self.session.events.recv_timeout(remaining) {
                Ok(event) => {
                    if self.collect(event, &mut output)? {
                        return Ok(output);
                    }
                }
                Err(_) => anyhow::bail!("实时识别结束前线程退出或超时"),
            }
        }
    }
}

/// Feed either backend the same decoded blocks on the media clock and write
/// observations immediately. This is an experiment, not an accuracy score.
pub fn replay_streaming_asr(
    input: &std::path::Path,
    output: &std::path::Path,
    backend: &mut dyn StreamingAsrBackend,
    metadata: serde_json::Value,
    verify_models: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use std::io::Write;
    ensure!(input.is_file(), "音频文件不存在");
    let fingerprint = opencap_core::fingerprint_media(input, None)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| anyhow::anyhow!("无法新建实验输出，不会覆盖已有文件：{e}"))?;
    writeln!(
        output,
        "{}",
        serde_json::json!({ "type": "metadata", "input": input, "input_fingerprint": fingerprint, "details": metadata, "scoring": "not evaluated" })
    )?;
    let start = Instant::now();
    let mut origin = None;
    let mut last_end = 0i64;
    let decoded = crate::audio::decode_file_with_info(input, |audio, _| {
        let first = *origin.get_or_insert(audio.pts_us);
        let frames = audio.samples.len() / usize::from(audio.channels.max(1));
        let end = audio.pts_us.saturating_add(
            (frames as i64).saturating_mul(1_000_000) / i64::from(audio.sample_rate.max(1)),
        );
        let elapsed = end.saturating_sub(first).max(0);
        last_end = end;
        let delay = Duration::from_micros(elapsed as u64).saturating_sub(start.elapsed());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        for event in backend.push(&audio)? {
            writeln!(
                output,
                "{}",
                serde_json::json!({ "type": "asr", "wall_elapsed_ms": start.elapsed().as_millis(), "input_end_ms": end.max(0) / 1000, "event": event })
            )?;
        }
        Ok(())
    })?;
    for event in backend.finish()? {
        writeln!(
            output,
            "{}",
            serde_json::json!({ "type": "asr", "wall_elapsed_ms": start.elapsed().as_millis(), "input_end_ms": last_end.max(0) / 1000, "event": event })
        )?;
    }
    let replay_elapsed_ms = start.elapsed().as_millis();
    ensure!(
        opencap_core::fingerprint_media(input, None)? == fingerprint,
        "实验期间输入媒体发生变化，输出不能作为同样本对照"
    );
    verify_models()?;
    writeln!(
        output,
        "{}",
        serde_json::json!({ "type": "complete", "wall_elapsed_ms": replay_elapsed_ms,
            "input_fingerprint_verified": fingerprint, "model_files_verified_after_replay": true, "decoded_audio": decoded })
    )?;
    output.sync_all()?;
    Ok(())
}
