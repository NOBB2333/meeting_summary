//! Linux X11 + PulseAudio backend. Wayland capture requires a separate portal session.
mod audio;
mod capture;
mod shell;
pub use audio::*;
pub use capture::*;
pub use shell::*;
pub const CAPTURE_PATH: &str = "X11 GetImage/XFixes -> CPU BGRA -> FFmpeg; PulseAudio";
