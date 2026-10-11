#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::*;
#[cfg(not(windows))]
compile_error!(
    "OpenCap v0.1 implements Windows only. Add a platform backend before enabling this target."
);
