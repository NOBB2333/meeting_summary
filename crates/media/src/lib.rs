mod encoder;
mod mixer;
mod mux;
mod recording;
mod reference;
pub use encoder::{FfmpegVideoEncoder, probe_encoders};
pub use mux::{MediaWriter, export_audio, inspect_output, recover_file};
pub use recording::*;
pub use reference::*;
pub fn initialize() -> anyhow::Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Error);
    Ok(())
}
mod preview;
pub use preview::RecordingPreview;
