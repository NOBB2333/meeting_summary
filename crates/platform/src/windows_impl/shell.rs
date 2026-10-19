use anyhow::Result;
use std::{
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::Path,
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::{FILETIME, HWND},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, OpenInputDesktop,
            },
            Threading::{CREATE_NO_WINDOW, GetCurrentProcess, GetProcessTimes},
        },
        UI::{
            Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW},
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
    // ShellExecute may load third-party association/preview handlers into its
    // caller and dispatch reentrant UI messages. Keep those outside OpenCap.
    // No cmd.exe or interpolated shell command: a filename is one literal arg.
    let path = path.canonicalize()?;
    let helper = std::env::current_exe()?;
    let mut child = std::process::Command::new(helper)
        .arg("--open-path-helper")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()?;
    std::thread::spawn(move || {
        if let Ok(status) = child.wait()
            && !status.success()
        {
            eprintln!("OpenCap: external file handler exited with {status}");
        }
    });
    Ok(())
}
/// Entry point used only by a short-lived child, before GPUI/COM initialisation.
pub fn open_path_in_helper(path: &Path) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    // Shell handlers expect ordinary drive/UNC paths, not Rust's canonical
    // verbatim prefix. Preserve UTF-16, including non-ASCII filenames.
    let mut file = if raw.starts_with(&[92, 92, 63, 92, 85, 78, 67, 92]) {
        [vec![92, 92], raw[8..].to_vec()].concat()
    } else if raw.starts_with(&[92, 92, 63, 92]) {
        raw[4..].to_vec()
    } else {
        raw
    };
    file.push(0);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info)?;
    }
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
