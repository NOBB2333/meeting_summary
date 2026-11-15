//! Rebuildable local waveform summaries. The cache never contains audio samples.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waveform {
    pub version: u32,
    pub source_sha256: String,
    pub source_bytes: u64,
    #[serde(default)]
    pub audio_stream_index: Option<u64>,
    pub bucket_ms: u64,
    pub duration_ms: u64,
    pub peaks: Vec<f32>,
    /// Runtime-only warning; the generated waveform can still be displayed.
    #[serde(skip)]
    pub cache_warning: Option<String>,
}

fn file_hash(path: &Path, cancelled: &AtomicBool) -> Result<(u64, String)> {
    let fingerprint =
        opencap_core::fingerprint_media(path, Some(cancelled)).context("无法核对波形来源")?;
    Ok((fingerprint.size_bytes, fingerprint.sha256))
}

fn cached_waveform(path: &Path) -> Result<Waveform> {
    const LIMIT: u64 = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "波形缓存过大");
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn load_or_build_waveform(
    source: &Path,
    cache_dir: &Path,
    cancelled: &AtomicBool,
) -> Result<Waveform> {
    load_or_build_waveform_for_stream(source, cache_dir, None, cancelled)
}

pub fn load_or_build_waveform_for_stream(
    source: &Path,
    cache_dir: &Path,
    audio_stream_index: Option<u64>,
    cancelled: &AtomicBool,
) -> Result<Waveform> {
    let identity = same_file::Handle::from_path(source).context("无法确认波形媒体身份")?;
    let before = fs::metadata(source).context("原媒体不可访问")?;
    ensure!(before.is_file(), "原媒体不是文件");
    let (size, sha) = file_hash(source, cancelled)?;
    let after_hash = fs::metadata(source)?;
    ensure!(
        before.len() == after_hash.len()
            && before.modified().ok() == after_hash.modified().ok()
            && identity == same_file::Handle::from_path(source)?,
        "原媒体在读取时发生变化"
    );
    ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
    let actual_stream = u64::try_from(crate::audio::select_audio_stream_index(
        source,
        audio_stream_index,
    )?)?;
    ensure!(
        identity == same_file::Handle::from_path(source)?,
        "音轨探测期间媒体已被替换"
    );
    let cache = cache_dir.join(format!("{sha}-{actual_stream}.json"));
    if let Ok(waveform) = cached_waveform(&cache)
        && waveform.version == 2
        && waveform.source_sha256 == sha
        && waveform.source_bytes == size
        && waveform.audio_stream_index == Some(actual_stream)
        && waveform.bucket_ms >= 100
        && waveform.bucket_ms % 100 == 0
        && (waveform.bucket_ms / 100).is_power_of_two()
        && !waveform.peaks.is_empty()
        && waveform.peaks.len() <= 100_000
        && waveform.duration_ms
            >= (waveform.peaks.len().saturating_sub(1) as u64).saturating_mul(waveform.bucket_ms)
        && waveform.duration_ms
            <= (waveform.peaks.len() as u64)
                .saturating_mul(waveform.bucket_ms)
                .saturating_add(1)
        && waveform
            .peaks
            .iter()
            .all(|peak| peak.is_finite() && (0.0..=1.0).contains(peak))
    {
        ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
        let current = fs::metadata(source).context("原媒体不可访问")?;
        ensure!(
            identity == same_file::Handle::from_path(source)?
                && current.len() == before.len()
                && current.modified().ok() == before.modified().ok(),
            "读取波形缓存期间媒体发生变化"
        );
        return Ok(waveform);
    }
    let mut bucket_ms = 100u64;
    let mut peaks = Vec::<f32>::new();
    let mut duration_ms = 0u64;
    crate::audio::decode_file_with_expected_stream(source, Some(actual_stream), |packet, _| {
        ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
        ensure!(
            packet.sample_rate > 0 && packet.channels == 1 && packet.pts_us >= 0,
            "波形音频时间无效"
        );
        let mut offset = 0usize;
        while offset < packet.samples.len() {
            let time_us = (packet.pts_us as u64).saturating_add(
                (offset as u64).saturating_mul(1_000_000) / u64::from(packet.sample_rate),
            );
            while time_us / (bucket_ms * 1000) >= 100_000 {
                peaks = peaks
                    .chunks(2)
                    .map(|chunk| chunk.iter().copied().fold(0., f32::max))
                    .collect();
                bucket_ms = bucket_ms.checked_mul(2).context("音频时长超出范围")?;
            }
            let index = (time_us / (bucket_ms * 1000)) as usize;
            let until = ((index as u64 + 1) * bucket_ms * 1000).saturating_sub(time_us);
            let count = (((until as u128 * u128::from(packet.sample_rate)) / 1_000_000) as usize)
                .max(1)
                .min(packet.samples.len() - offset);
            let peak = packet.samples[offset..offset + count]
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .map(|value| value.abs().min(1.))
                .fold(0., f32::max);
            if peaks.len() <= index {
                peaks.resize(index + 1, 0.);
            }
            peaks[index] = peaks[index].max(peak);
            offset += count;
        }
        let end_us = (packet.pts_us as u64).saturating_add(
            (packet.samples.len() as u64).saturating_mul(1_000_000) / u64::from(packet.sample_rate),
        );
        duration_ms = duration_ms.max(end_us / 1000);
        Ok(())
    })?;
    ensure!(!peaks.is_empty(), "音轨没有可用采样");
    ensure!(
        file_hash(source, cancelled)? == (size, sha.clone())
            && identity == same_file::Handle::from_path(source)?,
        "原媒体在生成波形期间发生变化"
    );
    let mut waveform = Waveform {
        version: 2,
        source_sha256: sha,
        source_bytes: size,
        audio_stream_index: Some(actual_stream),
        bucket_ms,
        duration_ms,
        peaks,
        cache_warning: None,
    };
    let temp = cache_dir.join(format!(".waveform-{}.tmp", opencap_core::new_stable_id()));
    let result = (|| -> Result<()> {
        ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
        fs::create_dir_all(cache_dir)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(&waveform)?)?;
        file.sync_all()?;
        ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
        let current = fs::metadata(source).context("原媒体不可访问")?;
        ensure!(
            identity == same_file::Handle::from_path(source)?
                && current.len() == before.len()
                && current.modified().ok() == before.modified().ok(),
            "发布波形缓存前媒体发生变化"
        );
        fs::rename(&temp, &cache)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    ensure!(!cancelled.load(Ordering::Relaxed), "波形任务已停止");
    let current = fs::metadata(source).context("原媒体不可访问")?;
    ensure!(
        identity == same_file::Handle::from_path(source)?
            && current.len() == before.len()
            && current.modified().ok() == before.modified().ok(),
        "媒体在波形生成或缓存发布期间发生变化"
    );
    if let Err(error) = result {
        waveform.cache_warning = Some(format!("波形已生成，本地缓存未保存：{error:#}"));
    }
    Ok(waveform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "requires OPENCAP_WAVEFORM_TEST_MEDIA (authorized long media)"]
    fn real_long_media_builds_waveform_then_reuses_the_cache() {
        let source = std::env::var_os("OPENCAP_WAVEFORM_TEST_MEDIA")
            .map(std::path::PathBuf::from)
            .expect("set OPENCAP_WAVEFORM_TEST_MEDIA");
        let cache = std::env::temp_dir().join(format!(
            "opencap-waveform-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cancelled = AtomicBool::new(false);
        let cold = Instant::now();
        let first = load_or_build_waveform(&source, &cache, &cancelled).unwrap();
        let cold_time = cold.elapsed();
        assert!(first.duration_ms >= 60_000, "unexpected duration");
        assert!(!first.peaks.is_empty());
        assert!(first.peaks.iter().all(|peak| (0.0..=1.0).contains(peak)));
        let warm = Instant::now();
        let second = load_or_build_waveform(&source, &cache, &cancelled).unwrap();
        let warm_time = warm.elapsed();
        assert_eq!(second.source_sha256, first.source_sha256);
        assert_eq!(second.peaks.len(), first.peaks.len());
        assert!(
            warm_time <= cold_time,
            "cache read should not be slower than building: cold={cold_time:?} warm={warm_time:?}"
        );
        std::fs::remove_dir_all(cache).unwrap();
    }
}
