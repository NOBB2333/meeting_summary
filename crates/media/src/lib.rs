mod encoder;
mod mixer;
mod mux;
mod playback_timeline;
#[cfg(feature = "internal-player")]
mod player;
mod recording;
mod reference;
mod timing;
pub use encoder::{FfmpegVideoEncoder, probe_encoders};
pub use mux::{MediaWriter, export_audio, export_audio_cancellable, inspect_output, recover_file};
pub use playback_timeline::*;
#[cfg(feature = "internal-player")]
pub use player::*;
pub use recording::*;
pub use reference::*;
pub fn initialize() -> anyhow::Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Error);
    Ok(())
}

/// Verify the filters used by pitch-preserving playback without an audio device.
pub fn verify_playback_filters() -> anyhow::Result<()> {
    initialize()?;
    for name in ["abuffer", "abuffersink", "atempo"] {
        anyhow::ensure!(
            ffmpeg_next::filter::find(name).is_some(),
            "FFmpeg playback filter is unavailable: {name}"
        );
    }
    Ok(())
}
mod preview;
pub use preview::{RecordingPreview, make_preview};
