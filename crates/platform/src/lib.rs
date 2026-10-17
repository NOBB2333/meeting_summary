#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::*;
#[cfg(target_os = "macos")]
mod macos_impl;
#[cfg(target_os = "macos")]
pub use macos_impl::*;
#[cfg(target_os = "linux")]
mod linux_impl;
#[cfg(target_os = "linux")]
pub use linux_impl::*;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!("OpenCap supports Windows, macOS and Linux.");
