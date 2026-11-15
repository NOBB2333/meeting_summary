//! Optional Nemotron streaming ASR experiment against a pinned C ABI.
use crate::{StreamingAsrBackend, StreamingTextEvent, nemo_poc::hash_file};
use anyhow::{Context, Result, ensure};
use opencap_core::PcmAudio;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::Path,
    ptr,
};

pub const NEMOTRON_ASR_SHA256: &str =
    "a5c435f294eea8f88ce68dd27b8c3bfea7f777cb2fbba04fcd30eaa555f429ae";
pub const NEMOTRON_ASR_BYTES: u64 = 741_548_352;

#[repr(C)]
struct BackendConfig {
    size: usize,
    gpu: i32,
}
#[repr(C)]
struct ModelConfig {
    size: usize,
    path: *const c_char,
    name: *const c_char,
}
#[repr(C)]
struct RecognizerConfig {
    size: usize,
    backend: *const BackendConfig,
    model: *const ModelConfig,
    streaming: *const c_void,
    decoder: *const c_void,
    vad: *const c_void,
    endpointing: *const c_void,
    postproc: *const c_void,
    diar: *const c_void,
    batching: *const c_void,
    salm: *const c_void,
}
#[repr(C)]
struct RecognitionOptions {
    size: usize,
    request_id: *const c_char,
    language_code: *const c_char,
    interim_results: bool,
    enable_word_time_offsets: bool,
    enable_automatic_punctuation: bool,
    verbatim_transcripts: bool,
    profanity_filter: bool,
    stop_history_eou_ms: i32,
    speech_contexts: *const c_void,
    speech_context_count: usize,
    max_alternatives: i32,
    enable_speaker_diarization: bool,
    max_speaker_count: i32,
}
type Create = unsafe extern "C" fn(*const RecognizerConfig, *mut *mut c_void) -> i32;
type Destroy = unsafe extern "C" fn(*mut c_void);
type OptionsDefault = unsafe extern "C" fn() -> RecognitionOptions;
type Open = unsafe extern "C" fn(*mut c_void, *const RecognitionOptions, *mut *mut c_void) -> i32;
type Push = unsafe extern "C" fn(*mut c_void, *const f32, usize, i32) -> i32;
type Finish = unsafe extern "C" fn(*mut c_void) -> i32;
type Next = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32;
type IsFinal = unsafe extern "C" fn(*const c_void) -> bool;
type Processed = unsafe extern "C" fn(*const c_void) -> f32;
type Alternatives = unsafe extern "C" fn(*const c_void) -> usize;
type Text = unsafe extern "C" fn(*const c_void, usize) -> *const c_char;
type WordCount = unsafe extern "C" fn(*const c_void, usize) -> usize;
type WordText = unsafe extern "C" fn(*const c_void, usize, usize) -> *const c_char;
type WordTime = unsafe extern "C" fn(*const c_void, usize, usize) -> i32;
type LastError = unsafe extern "C" fn() -> *const c_char;

/// Owns recognizer and stream on one worker. Does not infer word timing or
/// speaker identity from the cumulative hypothesis.
pub struct NemoStreamingAsr {
    _library: libloading::Library,
    _model_path: CString,
    recognizer: *mut c_void,
    stream: *mut c_void,
    destroy: Destroy,
    close: Destroy,
    push: Push,
    finish: Finish,
    next: Next,
    is_final: IsFinal,
    processed: Processed,
    alternatives: Alternatives,
    text: Text,
    word_count: WordCount,
    word_text: WordText,
    word_start: WordTime,
    word_end: WordTime,
    result_destroy: Destroy,
    last_error: LastError,
    rate: Option<u32>,
    origin_us: Option<i64>,
    expected_us: Option<i64>,
    received: u64,
    last_final_word_start: Option<i32>,
    finished: bool,
    pub library_sha256: String,
}
impl NemoStreamingAsr {
    /// # Safety
    /// The library must be trusted and implement the pinned NeMo-Speech C ABI.
    #[allow(clippy::type_complexity)]
    pub unsafe fn open(library: &Path, model: &Path) -> Result<Self> {
        let (size, hash) = hash_file(model)?;
        ensure!(
            size == NEMOTRON_ASR_BYTES && hash == NEMOTRON_ASR_SHA256,
            "ASR 模型大小或 SHA-256 不匹配"
        );
        let (_, library_sha256) = hash_file(library)?;
        let model_path = CString::new(model.to_str().context("模型路径不是有效 UTF-8")?)?;
        let library =
            unsafe { libloading::Library::new(library) }.context("无法加载 NeMo 原生库")?;
        unsafe fn symbol<T: Copy>(library: &libloading::Library, name: &[u8]) -> Result<T> {
            Ok(*unsafe { library.get::<T>(name) }.context("NeMo 库缺少所需 ASR C ABI 符号")?)
        }
        let (
            create,
            destroy,
            defaults,
            open,
            push,
            finish,
            next,
            close,
            is_final,
            processed,
            alternatives,
            text,
            word_count,
            word_text,
            word_start,
            word_end,
            result_destroy,
            last_error,
        ): (
            Create,
            Destroy,
            OptionsDefault,
            Open,
            Push,
            Finish,
            Next,
            Destroy,
            IsFinal,
            Processed,
            Alternatives,
            Text,
            WordCount,
            WordText,
            WordTime,
            WordTime,
            Destroy,
            LastError,
        ) = unsafe {
            (
                symbol(&library, b"nemo_speech_asr_create\0")?,
                symbol(&library, b"nemo_speech_asr_destroy\0")?,
                symbol(&library, b"nemo_speech_asr_recognition_options_default\0")?,
                symbol(&library, b"nemo_speech_asr_streaming_recognize\0")?,
                symbol(&library, b"nemo_speech_asr_stream_push_f32\0")?,
                symbol(&library, b"nemo_speech_asr_stream_finish\0")?,
                symbol(&library, b"nemo_speech_asr_stream_next\0")?,
                symbol(&library, b"nemo_speech_asr_stream_close\0")?,
                symbol(&library, b"nemo_speech_asr_result_is_final\0")?,
                symbol(&library, b"nemo_speech_asr_result_audio_processed\0")?,
                symbol(&library, b"nemo_speech_asr_result_alternative_count\0")?,
                symbol(&library, b"nemo_speech_asr_result_transcript\0")?,
                symbol(&library, b"nemo_speech_asr_result_word_count\0")?,
                symbol(&library, b"nemo_speech_asr_result_word_text\0")?,
                symbol(&library, b"nemo_speech_asr_result_word_start_time\0")?,
                symbol(&library, b"nemo_speech_asr_result_word_end_time\0")?,
                symbol(&library, b"nemo_speech_asr_result_destroy\0")?,
                symbol(&library, b"nemo_speech_asr_last_error\0")?,
            )
        };
        let backend = BackendConfig {
            size: std::mem::size_of::<BackendConfig>(),
            gpu: -1,
        };
        let model = ModelConfig {
            size: std::mem::size_of::<ModelConfig>(),
            path: model_path.as_ptr(),
            name: ptr::null(),
        };
        let config = RecognizerConfig {
            size: std::mem::size_of::<RecognizerConfig>(),
            backend: &backend,
            model: &model,
            streaming: ptr::null(),
            decoder: ptr::null(),
            vad: ptr::null(),
            endpointing: ptr::null(),
            postproc: ptr::null(),
            diar: ptr::null(),
            batching: ptr::null(),
            salm: ptr::null(),
        };
        let error_message = || unsafe {
            let p = last_error();
            if p.is_null() {
                "未知错误".to_owned()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let mut recognizer = ptr::null_mut();
        let status = unsafe { create(&config, &mut recognizer) };
        if status != 0 || recognizer.is_null() {
            let error = error_message();
            if !recognizer.is_null() {
                unsafe { destroy(recognizer) };
            }
            anyhow::bail!("NeMo ASR 初始化失败 ({status})：{error}");
        }
        let mut options = unsafe { defaults() };
        options.interim_results = true;
        options.enable_word_time_offsets = true;
        options.enable_speaker_diarization = false;
        let mut stream = ptr::null_mut();
        let status = unsafe { open(recognizer, &options, &mut stream) };
        if status != 0 || stream.is_null() {
            let error = error_message();
            if !stream.is_null() {
                unsafe { close(stream) };
            }
            unsafe { destroy(recognizer) };
            anyhow::bail!("NeMo ASR 流创建失败 ({status})：{error}");
        }
        Ok(Self {
            _library: library,
            _model_path: model_path,
            recognizer,
            stream,
            destroy,
            close,
            push,
            finish,
            next,
            is_final,
            processed,
            alternatives,
            text,
            word_count,
            word_text,
            word_start,
            word_end,
            result_destroy,
            last_error,
            rate: None,
            origin_us: None,
            expected_us: None,
            received: 0,
            last_final_word_start: None,
            finished: false,
            library_sha256,
        })
    }
    fn check(&self, status: i32, action: &str) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let message = unsafe {
            let p = (self.last_error)();
            if p.is_null() {
                "未知错误".to_owned()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        anyhow::bail!("{action}失败 ({status})：{message}")
    }
    fn drain(&mut self) -> Result<Vec<StreamingTextEvent>> {
        struct ResultHandle {
            ptr: *mut c_void,
            destroy: Destroy,
        }
        impl Drop for ResultHandle {
            fn drop(&mut self) {
                unsafe { (self.destroy)(self.ptr) };
            }
        }
        let mut events = Vec::new();
        for _ in 0..1024 {
            let mut raw = ptr::null_mut();
            let status = unsafe { (self.next)(self.stream, &mut raw) };
            if status != 0 {
                if !raw.is_null() {
                    unsafe { (self.result_destroy)(raw) };
                }
                self.check(status, "获取 NeMo ASR 结果")?;
            }
            if raw.is_null() {
                return Ok(events);
            }
            let guard = ResultHandle {
                ptr: raw,
                destroy: self.result_destroy,
            };
            let (finalized, processed, alternatives) = unsafe {
                (
                    (self.is_final)(guard.ptr),
                    (self.processed)(guard.ptr),
                    (self.alternatives)(guard.ptr),
                )
            };
            ensure!(
                alternatives <= 100 && processed.is_finite() && processed >= 0.,
                "NeMo ASR 返回无效结果"
            );
            let received_ms = self
                .rate
                .map(|rate| self.received.saturating_mul(1000) / u64::from(rate))
                .unwrap_or(0);
            ensure!(
                (processed as f64 * 1000.) <= received_ms.saturating_add(200) as f64,
                "NeMo ASR 返回的处理时间超出已输入音频"
            );
            let text = if alternatives == 0 {
                String::new()
            } else {
                let value = unsafe { (self.text)(guard.ptr, 0) };
                if value.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(value) }
                        .to_string_lossy()
                        .into_owned()
                }
            };
            let origin = self.origin_us.unwrap_or(0).max(0) as u64 / 1000;
            let mut words = Vec::new();
            let mut timing_error = None;
            if alternatives > 0 {
                let count = unsafe { (self.word_count)(guard.ptr, 0) };
                if count > 10_000 {
                    timing_error = Some("backend_word_count_exceeds_limit".into());
                } else {
                    let mut previous_start = self.last_final_word_start;
                    for index in 0..count {
                        let (word, start, end) = unsafe {
                            (
                                (self.word_text)(guard.ptr, 0, index),
                                (self.word_start)(guard.ptr, 0, index),
                                (self.word_end)(guard.ptr, 0, index),
                            )
                        };
                        if word.is_null()
                            || start < 0
                            || end < start
                            || end as u64 > received_ms.saturating_add(80)
                            || previous_start.is_some_and(|previous| start < previous)
                        {
                            words.clear();
                            timing_error = Some("backend_word_timing_invalid".into());
                            break;
                        }
                        let text = unsafe { CStr::from_ptr(word) }
                            .to_string_lossy()
                            .into_owned();
                        if text.is_empty() || text.len() > 4096 {
                            words.clear();
                            timing_error = Some("backend_word_text_invalid".into());
                            break;
                        }
                        words.push(crate::StreamingWordTiming {
                            text,
                            start_ms: origin.saturating_add(start as u64),
                            end_ms: origin.saturating_add(end as u64),
                            method: "nemo_decoder_emission_frames_not_forced_alignment".into(),
                        });
                        previous_start = Some(start);
                    }
                    if finalized && timing_error.is_none() {
                        self.last_final_word_start = previous_start;
                    }
                }
            }
            events.push(StreamingTextEvent {
                kind: "snapshot".into(),
                segment_id: None,
                revision: None,
                text,
                finalized,
                processed_ms: Some(origin.saturating_add((processed as f64 * 1000.) as u64)),
                words,
                raw_token_timing: None,
                timing_error,
                run_provenance: None,
            });
        }
        anyhow::bail!("NeMo ASR 在一次读取中返回过多结果")
    }
}
impl StreamingAsrBackend for NemoStreamingAsr {
    fn push(&mut self, audio: &PcmAudio) -> Result<Vec<StreamingTextEvent>> {
        ensure!(
            !self.finished
                && audio.pts_us >= 0
                && (8_000..=96_000).contains(&audio.sample_rate)
                && audio.channels > 0,
            "无效音频块或 ASR 流已结束"
        );
        ensure!(
            audio
                .samples
                .len()
                .is_multiple_of(usize::from(audio.channels)),
            "不完整的 PCM 帧"
        );
        if let Some(rate) = self.rate {
            ensure!(rate == audio.sample_rate, "ASR 流不能更换采样率");
        }
        if let Some(expected) = self.expected_us {
            ensure!(
                audio.pts_us.abs_diff(expected) <= 1_000,
                "音频时间出现缺口，不能压缩时间轴"
            );
        }
        if audio.samples.is_empty() {
            return Ok(Vec::new());
        }
        self.rate = Some(audio.sample_rate);
        self.origin_us.get_or_insert(audio.pts_us);
        let mono: Vec<f32> = audio
            .samples
            .chunks_exact(usize::from(audio.channels))
            .map(|frame| {
                frame
                    .iter()
                    .map(|v| if v.is_finite() { *v } else { 0. })
                    .sum::<f32>()
                    / f32::from(audio.channels)
            })
            .collect();
        self.check(
            unsafe {
                (self.push)(
                    self.stream,
                    mono.as_ptr(),
                    mono.len(),
                    audio.sample_rate as i32,
                )
            },
            "写入 NeMo ASR",
        )?;
        self.received = self.received.saturating_add(mono.len() as u64);
        self.expected_us = Some(
            self.origin_us.unwrap_or(0).saturating_add(
                (self.received.saturating_mul(1_000_000) / u64::from(audio.sample_rate))
                    .min(i64::MAX as u64) as i64,
            ),
        );
        self.drain()
    }
    fn finish(&mut self) -> Result<Vec<StreamingTextEvent>> {
        ensure!(!self.finished, "NeMo ASR 流已结束");
        self.check(unsafe { (self.finish)(self.stream) }, "结束 NeMo ASR 流")?;
        self.finished = true;
        let events = self.drain()?;
        ensure!(
            events.iter().any(|event| event.finalized),
            "NeMo ASR 未返回最终结果，实验标记为未完成"
        );
        Ok(events)
    }
}
impl Drop for NemoStreamingAsr {
    fn drop(&mut self) {
        unsafe {
            if !self.stream.is_null() {
                (self.close)(self.stream);
            }
            if !self.recognizer.is_null() {
                (self.destroy)(self.recognizer);
            }
        }
    }
}
