mod audio;
mod capture;
mod controls;
mod indicator;
mod preview;
mod region;
mod shell;
pub use audio::*;
pub use capture::*;
pub use controls::*;
pub use indicator::*;
pub use preview::*;
pub use region::*;
pub use shell::*;

/// QPC and WGC/WASAPI timestamps use the same monotonic Windows clock.
pub fn qpc_us() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut value, mut frequency) = (0, 0);
    unsafe {
        let _ = QueryPerformanceCounter(&mut value);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    if frequency <= 0 {
        return 0;
    }
    ((value as i128 * 1_000_000) / frequency as i128) as i64
}
