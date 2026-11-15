//! Explicit, opt-in NeMo-Speech.cpp C ABI experiment. Not a default backend.
//!
//! ABI and model are pinned to NeMo-Speech.cpp e790e463b2913cff9f989a3df224c587f14b4c5c.
//! A caller must supply a trusted native library and the verified model file.
use crate::{OnlineDiarizer, OnlineSpeakerInterval, OnlineSpeakerSnapshot};
use anyhow::{Context, Result, ensure};
use opencap_core::PcmAudio;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    io::Read,
    path::Path,
    ptr,
};

pub const NEMO_REFERENCE_REVISION: &str = "e790e463b2913cff9f989a3df224c587f14b4c5c";
pub const NEMOTRON_DIAR_SHA256: &str =
    "08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1";
pub const NEMOTRON_DIAR_BYTES: u64 = 107_012_128;

#[repr(C)]
struct ModelConfig {
    size: usize,
    model_path: *const c_char,
    gpu: i32,
    preset: *const c_char,
    chunk_frames: i32,
    right_context_frames: i32,
    left_context_frames: i32,
    fifo_frames: i32,
    spkcache_frames: i32,
    update_period_frames: i32,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct NativeSegment {
    start_time: f64,
    end_time: f64,
    speaker: i32,
}
type Create = unsafe extern "C" fn(*const ModelConfig, *mut *mut c_void) -> i32;
type Destroy = unsafe extern "C" fn(*mut c_void);
type Capacity = unsafe extern "C" fn(*const c_void) -> i32;
type Open = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32;
type Push = unsafe extern "C" fn(*mut c_void, *const f32, usize, i32) -> i32;
type Finish = unsafe extern "C" fn(*mut c_void) -> i32;
type Segments = unsafe extern "C" fn(
    *const c_void,
    *const c_void,
    *mut NativeSegment,
    usize,
    *mut usize,
) -> i32;
type LastError = unsafe extern "C" fn() -> *const c_char;

pub(crate) fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = opencap_core::open_regular_file_for_read(path)
        .with_context(|| format!("无法读取 {}", path.display()))?;
    let before = file.metadata()?;
    let identity = same_file::Handle::from_file(file.try_clone()?)?;
    let mut sha = Sha256::new();
    let mut count = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).context("文件大小超出范围")?;
        sha.update(&buf[..n]);
    }
    let after = file.metadata()?;
    let current = opencap_core::open_regular_file_for_read(path)?;
    ensure!(
        before.len() == count
            && after.len() == count
            && before.modified()? == after.modified()?
            && same_file::Handle::from_file(current)? == identity,
        "读取期间文件发生变化：{}",
        path.display()
    );
    Ok((count, format!("{:x}", sha.finalize())))
}

/// Record the actual file bytes used by a local model comparison.
pub fn fingerprint_poc_file(path: &Path) -> Result<(u64, String)> {
    hash_file(path)
}

/// Owns both opaque handles and the loaded library. Instances are deliberately
/// not shared between workers: the upstream stream is single-threaded.
pub struct NemoDiarizer {
    _library: libloading::Library,
    _model_path: CString,
    model: *mut c_void,
    stream: *mut c_void,
    destroy: Destroy,
    close: Destroy,
    push: Push,
    finish: Finish,
    segments: Segments,
    last_error: LastError,
    capacity: usize,
    rate: Option<u32>,
    origin_us: Option<i64>,
    expected_us: Option<i64>,
    samples_received: u64,
    revision: u64,
    finished: bool,
    pub library_sha256: String,
}

impl NemoDiarizer {
    /// Loading a native library runs its initialization code; only pass a
    /// library built from the pinned, trusted source. The library hash is
    /// recorded, but its source revision cannot be inferred from the binary.
    ///
    /// # Safety
    /// The supplied library must implement the pinned C ABI and be trusted code.
    pub unsafe fn open(library: &Path, model: &Path) -> Result<Self> {
        let (size, hash) = hash_file(model)?;
        ensure!(
            size == NEMOTRON_DIAR_BYTES && hash == NEMOTRON_DIAR_SHA256,
            "说话人模型大小或 SHA-256 不匹配"
        );
        let (_, library_sha256) = hash_file(library)?;
        let model_path = CString::new(model.to_str().context("模型路径不是有效 UTF-8")?)?;
        let library =
            unsafe { libloading::Library::new(library) }.context("无法加载 NeMo 原生库")?;
        unsafe fn symbol<T: Copy>(library: &libloading::Library, name: &[u8]) -> Result<T> {
            Ok(*unsafe { library.get::<T>(name) }.context("NeMo 库缺少所需 C ABI 符号")?)
        }
        let (create, destroy, capacity, open, push, finish, close, segments, last_error): (
            Create,
            Destroy,
            Capacity,
            Open,
            Push,
            Finish,
            Destroy,
            Segments,
            LastError,
        ) = unsafe {
            (
                symbol(&library, b"nemo_speech_diar_create\0")?,
                symbol(&library, b"nemo_speech_diar_destroy\0")?,
                symbol(&library, b"nemo_speech_diar_num_speakers\0")?,
                symbol(&library, b"nemo_speech_diar_stream_open\0")?,
                symbol(&library, b"nemo_speech_diar_stream_push_f32\0")?,
                symbol(&library, b"nemo_speech_diar_stream_finish\0")?,
                symbol(&library, b"nemo_speech_diar_stream_close\0")?,
                symbol(&library, b"nemo_speech_diar_segments\0")?,
                symbol(&library, b"nemo_speech_asr_last_error\0")?,
            )
        };
        let config = ModelConfig {
            size: std::mem::size_of::<ModelConfig>(),
            model_path: model_path.as_ptr(),
            gpu: -1,
            preset: ptr::null(),
            chunk_frames: 0,
            right_context_frames: 0,
            left_context_frames: -1,
            fifo_frames: 0,
            spkcache_frames: 0,
            update_period_frames: 0,
        };
        let message = || unsafe {
            let text = last_error();
            if text.is_null() {
                "未知错误".into()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        };
        let mut model_handle = ptr::null_mut();
        let status = unsafe { create(&config, &mut model_handle) };
        if status != 0 || model_handle.is_null() {
            let error = message();
            if !model_handle.is_null() {
                unsafe { destroy(model_handle) };
            }
            anyhow::bail!("NeMo 模型初始化失败 ({status})：{error}");
        }
        let capacity_value = unsafe { capacity(model_handle) };
        if capacity_value != 8 {
            unsafe { destroy(model_handle) };
            anyhow::bail!("模型报告 {capacity_value} 个 speaker slot；本实验固定检查 8 个 slot");
        }
        let mut stream = ptr::null_mut();
        let status = unsafe { open(model_handle, &mut stream) };
        if status != 0 || stream.is_null() {
            let error = message();
            if !stream.is_null() {
                unsafe { close(stream) };
            }
            unsafe { destroy(model_handle) };
            anyhow::bail!("NeMo 流创建失败 ({status})：{error}");
        }
        Ok(Self {
            _library: library,
            _model_path: model_path,
            model: model_handle,
            stream,
            destroy,
            close,
            push,
            finish,
            segments,
            last_error,
            capacity: capacity_value as usize,
            rate: None,
            origin_us: None,
            expected_us: None,
            samples_received: 0,
            revision: 0,
            finished: false,
            library_sha256,
        })
    }
    fn check(&self, status: i32, action: &str) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let error = unsafe {
            let text = (self.last_error)();
            if text.is_null() {
                "未知错误".into()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        };
        anyhow::bail!("{action}失败 ({status})：{error}")
    }
}

impl OnlineDiarizer for NemoDiarizer {
    fn push(&mut self, audio: &PcmAudio) -> Result<()> {
        ensure!(!self.finished, "说话人流已结束");
        ensure!(
            (8_000..=96_000).contains(&audio.sample_rate) && audio.channels > 0,
            "无效音频格式"
        );
        ensure!(
            audio
                .samples
                .len()
                .is_multiple_of(usize::from(audio.channels)),
            "不完整的 PCM 帧"
        );
        ensure!(audio.pts_us >= 0, "音频时间必须使用非负媒体坐标");
        if let Some(rate) = self.rate {
            ensure!(rate == audio.sample_rate, "单个说话人流不能更换采样率");
        }
        if let Some(expected) = self.expected_us {
            ensure!(
                audio.pts_us.abs_diff(expected) <= 1_000,
                "音频时间不连续；需要标记缺口并重建流，不能压缩时间轴"
            );
        }
        if audio.samples.is_empty() {
            return Ok(());
        }
        self.rate = Some(audio.sample_rate);
        self.origin_us.get_or_insert(audio.pts_us);
        let mono: Vec<f32>;
        let samples: &[f32] =
            if audio.channels == 1 && audio.samples.iter().all(|sample| sample.is_finite()) {
                &audio.samples
            } else {
                mono = audio
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
                &mono
            };
        let status = unsafe {
            (self.push)(
                self.stream,
                samples.as_ptr(),
                samples.len(),
                audio.sample_rate as i32,
            )
        };
        self.check(status, "NeMo 流输入")?;
        self.samples_received = self.samples_received.saturating_add(samples.len() as u64);
        self.expected_us = Some(
            self.origin_us.unwrap_or(0).saturating_add(
                (self.samples_received.saturating_mul(1_000_000) / u64::from(audio.sample_rate))
                    .min(i64::MAX as u64) as i64,
            ),
        );
        Ok(())
    }
    fn snapshot(&mut self) -> Result<OnlineSpeakerSnapshot> {
        let mut count = 0usize;
        let status =
            unsafe { (self.segments)(self.stream, ptr::null(), ptr::null_mut(), 0, &mut count) };
        self.check(status, "读取说话人区间数量")?;
        ensure!(count <= 1_000_000, "说话人区间数量异常");
        let mut segments = vec![NativeSegment::default(); count];
        if count > 0 {
            let status = unsafe {
                (self.segments)(
                    self.stream,
                    ptr::null(),
                    segments.as_mut_ptr(),
                    segments.len(),
                    &mut count,
                )
            };
            self.check(status, "读取说话人区间")?;
            ensure!(count <= segments.len(), "说话人区间数量超出缓冲");
            segments.truncate(count);
        }
        let origin = self.origin_us.unwrap_or(0).max(0) as u64 / 1000;
        let mut intervals = Vec::with_capacity(segments.len());
        for segment in segments {
            ensure!(
                segment.start_time.is_finite()
                    && segment.end_time.is_finite()
                    && segment.start_time >= 0.
                    && segment.end_time >= segment.start_time
                    && segment.speaker > 0
                    && segment.speaker as usize <= self.capacity,
                "NeMo 返回无效说话人区间"
            );
            intervals.push(OnlineSpeakerInterval {
                start_ms: origin.saturating_add((segment.start_time * 1000.) as u64),
                end_ms: origin.saturating_add((segment.end_time * 1000.) as u64),
                slot: segment.speaker as u32,
                extra: Default::default(),
            });
        }
        self.revision = self.revision.saturating_add(1);
        Ok(OnlineSpeakerSnapshot {
            revision: self.revision,
            input_end_ms: self.expected_us.unwrap_or(0).max(0) as u64 / 1000,
            finished: self.finished,
            intervals,
        })
    }
    fn finish(&mut self) -> Result<OnlineSpeakerSnapshot> {
        if !self.finished {
            let status = unsafe { (self.finish)(self.stream) };
            self.check(status, "结束 NeMo 说话人流")?;
            self.finished = true;
        }
        self.snapshot()
    }
    fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Replay an authorized file at media speed and write inspectable snapshots.
/// This experimental runner does not provide DER/CER or prove product readiness.
pub fn replay_nemo_diarization(
    input: &Path,
    library: &Path,
    model: &Path,
    output: &Path,
) -> Result<()> {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    ensure!(input.is_file(), "音频文件不存在");
    let fingerprint = opencap_core::fingerprint_media(input, None)?;
    let loading = Instant::now();
    // The CLI requires an explicit library path and warns that native code is trusted.
    let mut backend = unsafe { NemoDiarizer::open(library, model)? };
    let load_ms = loading.elapsed().as_millis();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .context("无法新建 POC 输出；不会覆盖已有文件")?;
    writeln!(
        output,
        "{}",
        serde_json::json!({
            "type": "metadata", "reference_revision": NEMO_REFERENCE_REVISION,
            "binary_revision": "unverified", "library_sha256": backend.library_sha256.as_str(),
            "model_sha256": NEMOTRON_DIAR_SHA256, "provider": "cpu", "capacity": backend.capacity(),
            "model_load_ms": load_ms, "input": input, "input_rate_hz": 16000,
            "input_fingerprint": fingerprint,
            "scoring": "not evaluated", "timestamps": "backend speaker segments, not word alignment"
        })
    )?;
    let started = Instant::now();
    let mut origin: Option<i64> = None;
    let mut next_snapshot = 1_000_000i64;
    let decoded = crate::audio::decode_file_with_info(input, |audio, _duration| {
        let first = *origin.get_or_insert(audio.pts_us);
        let end =
            audio.pts_us + (audio.samples.len() as i64 * 1_000_000 / i64::from(audio.sample_rate));
        let elapsed_media = end.saturating_sub(first).max(0);
        let delay = Duration::from_micros(elapsed_media as u64).saturating_sub(started.elapsed());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        backend.push(&audio)?;
        if elapsed_media >= next_snapshot {
            let snapshot = backend.snapshot()?;
            writeln!(
                output,
                "{}",
                serde_json::json!({
                    "type": "snapshot", "wall_elapsed_ms": started.elapsed().as_millis(),
                    "snapshot": snapshot
                })
            )?;
            next_snapshot = elapsed_media + 1_000_000;
        }
        Ok(())
    })?;
    let snapshot = backend.finish()?;
    let replay_elapsed_ms = started.elapsed().as_millis();
    ensure!(
        opencap_core::fingerprint_media(input, None)? == fingerprint,
        "实验期间输入媒体发生变化，输出不能作为同样本对照"
    );
    ensure!(
        fingerprint_poc_file(model)? == (NEMOTRON_DIAR_BYTES, NEMOTRON_DIAR_SHA256.to_owned()),
        "实验期间说话人模型发生变化"
    );
    ensure!(
        fingerprint_poc_file(library)?.1 == backend.library_sha256,
        "实验期间说话人动态库发生变化"
    );
    writeln!(
        output,
        "{}",
        serde_json::json!({
            "type": "final", "wall_elapsed_ms": replay_elapsed_ms, "snapshot": snapshot,
            "input_fingerprint_verified": fingerprint, "model_files_verified_after_replay": true, "decoded_audio": decoded
        })
    )?;
    output.sync_all()?;
    Ok(())
}

impl Drop for NemoDiarizer {
    fn drop(&mut self) {
        unsafe {
            if !self.stream.is_null() {
                (self.close)(self.stream);
            }
            if !self.model.is_null() {
                (self.destroy)(self.model);
            }
        }
    }
}
