#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::*;
#[cfg(target_os = "macos")]
mod macos_impl;
#[cfg(target_os = "macos")]
pub use macos_impl::*;
#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("OpenCap supports Windows and macOS. Linux needs a PipeWire backend.");
