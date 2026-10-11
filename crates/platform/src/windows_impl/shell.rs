use anyhow::Result;
use std::{path::Path, time::Instant};
use windows::{
    Win32::{
        Foundation::{FILETIME, HWND},
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, OpenInputDesktop,
            },
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
        UI::{
            Shell::ShellExecuteW,
            WindowsAndMessaging::{
                SW_SHOWNORMAL, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
            },
        },
    },
    core::{PCWSTR, w},
};

pub fn exclude_from_capture(hwnd: isize) -> Result<()> {
    set_capture_exclusion(hwnd, true)
}
pub fn set_capture_exclusion(hwnd: isize, exclude: bool) -> Result<()> {
    unsafe {
        SetWindowDisplayAffinity(
            HWND(hwnd as _),
            if exclude {
                WDA_EXCLUDEFROMCAPTURE
            } else {
                WDA_NONE
            },
        )?;
    }
    Ok(())
}
pub fn open_path(path: &Path) -> Result<()> {
    let path: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    anyhow::ensure!(result.0 as isize > 32, "Unable to open path");
    Ok(())
}
pub fn desktop_available() -> bool {
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(d) => {
                let _ = CloseDesktop(d);
                true
            }
            Err(_) => false,
        }
    }
}
pub struct ProcessMeter {
    previous: u64,
    time: Instant,
    cores: usize,
}
impl Default for ProcessMeter {
    fn default() -> Self {
        Self {
            previous: 0,
            time: Instant::now(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }
}
impl ProcessMeter {
    pub fn sample(&mut self) -> (Option<f64>, Option<u64>) {
        unsafe {
            let (mut creation, mut exit, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            let cpu = if GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
            .is_ok()
            {
                let v = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
                let total = v(kernel) + v(user);
                let elapsed = self.time.elapsed().as_secs_f64();
                let result = if self.previous > 0 && elapsed > 0.0 {
                    Some(
                        (total.saturating_sub(self.previous)) as f64
                            / 10_000_000.0
                            / elapsed
                            / self.cores as f64
                            * 100.0,
                    )
                } else {
                    None
                };
                self.previous = total;
                self.time = Instant::now();
                result
            } else {
                None
            };
            let mut mem = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            let memory = if GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut mem,
                std::mem::size_of_val(&mem) as u32,
            )
            .is_ok()
            {
                Some(mem.PeakWorkingSetSize as u64)
            } else {
                None
            };
            (cpu, memory)
        }
    }
}
