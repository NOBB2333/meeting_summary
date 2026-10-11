use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::atomic::{AtomicI8, Ordering};
use std::thread::{self, JoinHandle};
use windows::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, RemoteDesktop::*},
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

const IDLE_ICON_ID: usize = 101;
const RECORDING_ICON_ID: usize = 102;

#[derive(Debug, Clone)]
pub enum ShellEvent {
    Show,
    Record,
    Pause,
    OpenFolder,
    Quit,
    Locked,
    Warning(String),
}
pub struct DesktopControls {
    hwnd: isize,
    pub events: Receiver<ShellEvent>,
    worker: Option<JoinHandle<()>>,
    /// Cached tray state (0 idle, 1 recording); avoids re-sending NIM_MODIFY every 200 ms poll.
    tray_state: AtomicI8,
}
impl Drop for DesktopControls {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl DesktopControls {
    /// The tray changes quietly between the two embedded icons. The tooltip stays neutral.
    pub fn set_recording_state(&self, recording: bool) {
        let next = i8::from(recording);
        if self.tray_state.swap(next, Ordering::Relaxed) == next {
            return;
        }
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as _)),
                TRAY_ICON_MESSAGE,
                WPARAM(usize::from(recording)),
                LPARAM(0),
            );
        }
    }
}

pub fn set_app_icon(hwnd: isize) {
    unsafe {
        if hwnd == 0 {
            return;
        }
        if let Ok(icon) = app_icon(false) {
            let _ = SendMessageW(
                HWND(hwnd as _),
                WM_SETICON,
                Some(WPARAM(0)),
                Some(LPARAM(icon.0 as isize)),
            );
            let _ = SendMessageW(
                HWND(hwnd as _),
                WM_SETICON,
                Some(WPARAM(1)),
                Some(LPARAM(icon.0 as isize)),
            );
        }
    }
}
struct ShellState {
    sender: Sender<ShellEvent>,
}
const TRAY_MESSAGE: u32 = WM_APP + 1;
const TRAY_ICON_MESSAGE: u32 = WM_APP + 2;

unsafe fn app_icon(recording: bool) -> Result<HICON> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let resource = if recording {
        RECORDING_ICON_ID
    } else {
        IDLE_ICON_ID
    };
    unsafe { LoadIconW(Some(instance.into()), PCWSTR(resource as *const u16)) }
        .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
        .map_err(Into::into)
}

unsafe fn update_tray_icon(hwnd: HWND, recording: bool) {
    if let Ok(icon) = unsafe { app_icon(recording) } {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON,
            hIcon: icon,
            ..Default::default()
        };
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }
}
pub fn desktop_controls(record: &str, pause: &str) -> Result<DesktopControls> {
    let (tx, rx) = bounded(32);
    let (ready_tx, ready_rx) = bounded(1);
    let keys = [parse_hotkey(record)?, parse_hotkey(pause)?];
    anyhow::ensure!(keys[0] != keys[1], "录制与暂停快捷键不能相同");
    let worker = thread::spawn(move || {
        let result = unsafe { controls_loop(tx, ready_tx.clone(), keys) };
        if let Err(e) = result {
            let _ = ready_tx.try_send(Err(format!("{e:#}")));
        }
    });
    let hwnd = ready_rx
        .recv()
        .context("Desktop controls thread exited")?
        .map_err(anyhow::Error::msg)?;
    Ok(DesktopControls {
        hwnd,
        events: rx,
        worker: Some(worker),
        // controls_loop already added the tray icon with the idle icon.
        tray_state: AtomicI8::new(0),
    })
}
fn parse_hotkey(text: &str) -> Result<(u32, u32)> {
    let mut mods = MOD_NOREPEAT.0;
    let mut key = None;
    for token in text.split('+') {
        match token.trim().to_ascii_lowercase().as_str() {
            "ctrl" => mods |= MOD_CONTROL.0,
            "alt" => mods |= MOD_ALT.0,
            "shift" => mods |= MOD_SHIFT.0,
            "win" => mods |= MOD_WIN.0,
            s if s.starts_with('f') && s.len() > 1 => {
                let n: u32 = s[1..].parse()?;
                anyhow::ensure!((1..=24).contains(&n), "Invalid function key");
                key = Some(VK_F1.0 as u32 + n - 1);
            }
            s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => {
                key = Some(s.as_bytes()[0].to_ascii_uppercase() as u32)
            }
            _ => anyhow::bail!("Invalid hotkey: {text}"),
        }
    }
    Ok((mods, key.context("Hotkey has no key")?))
}
unsafe fn controls_loop(
    sender: Sender<ShellEvent>,
    ready: Sender<std::result::Result<isize, String>>,
    keys: [(u32, u32); 2],
) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("OpenCapControls"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let state = Box::new(ShellState { sender });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("OpenCapControls"),
            w!("OpenCap"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some((&*state as *const ShellState).cast()),
        )?;
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: app_icon(false)?,
            ..Default::default()
        };
        let tip: Vec<u16> = "OpenCap".encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            let _ = state
                .sender
                .try_send(ShellEvent::Warning("系统托盘图标创建失败".into()));
        }
        for (i, (mods, key)) in keys.into_iter().enumerate() {
            if RegisterHotKey(Some(hwnd), i as i32 + 1, HOT_KEY_MODIFIERS(mods), key).is_err() {
                let _ = state.sender.try_send(ShellEvent::Warning(format!(
                    "快捷键 {} 被其他程序占用，请修改设置",
                    i + 1
                )));
            }
        }
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        ready.send(Ok(hwnd.0 as isize))?;
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = WTSUnRegisterSessionNotification(hwnd);
        let _ = UnregisterHotKey(Some(hwnd), 1);
        let _ = UnregisterHotKey(Some(hwnd), 2);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        drop(state);
        Ok(())
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ShellState;
        if !state.is_null() {
            let send = |event| {
                let _ = (*state).sender.try_send(event);
            };
            match message {
                WM_HOTKEY => {
                    send(if wparam.0 == 1 {
                        ShellEvent::Record
                    } else {
                        ShellEvent::Pause
                    });
                    return LRESULT(0);
                }
                WM_WTSSESSION_CHANGE if wparam.0 as u32 == WTS_SESSION_LOCK => {
                    send(ShellEvent::Locked);
                    return LRESULT(0);
                }
                TRAY_ICON_MESSAGE => {
                    update_tray_icon(hwnd, wparam.0 != 0);
                    return LRESULT(0);
                }
                TRAY_MESSAGE => {
                    if lparam.0 as u32 == WM_LBUTTONDBLCLK {
                        send(ShellEvent::Show);
                    }
                    if lparam.0 as u32 == WM_RBUTTONUP
                        && let Ok(menu) = CreatePopupMenu()
                    {
                        for (id, label) in [
                            (1, "打开 OpenCap"),
                            (2, "开始 / 停止录制"),
                            (3, "暂停 / 继续"),
                            (4, "打开录制目录"),
                            (5, "退出"),
                        ] {
                            let label: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                id,
                                windows::core::PCWSTR(label.as_ptr()),
                            );
                        }
                        let mut point = POINT::default();
                        let _ = GetCursorPos(&mut point);
                        let _ = SetForegroundWindow(hwnd);
                        let selected = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_RIGHTBUTTON,
                            point.x,
                            point.y,
                            Some(0),
                            hwnd,
                            None,
                        )
                        .0;
                        match selected {
                            1 => send(ShellEvent::Show),
                            2 => send(ShellEvent::Record),
                            3 => send(ShellEvent::Pause),
                            4 => send(ShellEvent::OpenFolder),
                            5 => send(ShellEvent::Quit),
                            _ => (),
                        }
                        let _ = DestroyMenu(menu);
                    }
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                _ => (),
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
pub fn show_native_window(hwnd: isize, show: bool) {
    unsafe {
        let h = HWND(hwnd as _);
        let _ = ShowWindow(h, if show { SW_RESTORE } else { SW_HIDE });
        if show {
            let _ = SetForegroundWindow(h);
        }
    }
}
pub fn show_controls_without_activation(hwnd: isize, show: bool) {
    unsafe {
        let _ = ShowWindow(
            HWND(hwnd as _),
            if show { SW_SHOWNOACTIVATE } else { SW_HIDE },
        );
    }
}
/// GPUI defers placement for show:false windows. Native ShowWindow bypasses that
/// deferred placement, so establish the compact toolbar bounds before first show.
pub fn configure_floating_controls(hwnd: isize, owner: isize) -> Result<()> {
    use windows::Win32::{
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        UI::HiDpi::GetDpiForWindow,
    };
    unsafe {
        let hwnd = HWND(hwnd as _);
        let owner = HWND(owner as _);
        let scale = GetDpiForWindow(owner).max(96) as f64 / 96.;
        let (width, height) = ((660. * scale).round() as i32, (66. * scale).round() as i32);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(
            MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
            &mut info,
        )
        .ok()?;
        let work = info.rcWork;
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ((ex & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) as isize,
        );
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            work.left + ((work.right - work.left - width) / 2).max(0),
            work.bottom - height - 20,
            width,
            height,
            SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
    }
    Ok(())
}
pub fn native_window_visible(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(HWND(hwnd as _)).as_bool() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hotkeys_validate() {
        assert_eq!(parse_hotkey("Ctrl+Alt+F9").unwrap().1, VK_F9.0 as u32);
        assert!(parse_hotkey("Ctrl+F40").is_err());
        assert!(parse_hotkey("Ctrl").is_err());
    }
}
