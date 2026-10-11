use anyhow::Result;
use opencap_core::CaptureTarget;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

/// A non-activating, click-through boundary. It is never included in recordings.
pub struct CaptureIndicator {
    hwnd: isize,
    last: Option<([i32; 4], bool)>,
    target: Option<CaptureTarget>,
}
impl CaptureIndicator {
    pub fn new() -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(indicator_proc),
                hInstance: instance.into(),
                lpszClassName: w!("OpenCapBoundary"),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                w!("OpenCapBoundary"),
                w!("OpenCap 录制范围"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let result = (|| -> Result<()> {
                SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_COLORKEY)?;
                SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
            Ok(Self {
                hwnd: hwnd.0 as isize,
                last: None,
                target: None,
            })
        }
    }
    pub fn update(&mut self, target: &CaptureTarget, paused: bool) -> Result<()> {
        if !matches!(target, CaptureTarget::Window(_))
            && self.target.as_ref() == Some(target)
            && self
                .last
                .is_some_and(|(_, last_paused)| last_paused == paused)
        {
            return Ok(());
        }
        let rect = match *target {
            CaptureTarget::Monitor(index) | CaptureTarget::Region { monitor: index, .. } => {
                let (x, y, w, h) = super::monitor_bounds(index)?;
                if let CaptureTarget::Region { rect, .. } = target {
                    [
                        x + rect.x as i32,
                        y + rect.y as i32,
                        rect.width as i32,
                        rect.height as i32,
                    ]
                } else {
                    [x, y, w as i32, h as i32]
                }
            }
            CaptureTarget::Window(id) => unsafe {
                let mut cloaked = 0u32;
                let _ = DwmGetWindowAttribute(
                    HWND(id as _),
                    DWMWA_CLOAKED,
                    (&mut cloaked as *mut u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                );
                if !IsWindow(Some(HWND(id as _))).as_bool()
                    || IsIconic(HWND(id as _)).as_bool()
                    || !IsWindowVisible(HWND(id as _)).as_bool()
                    || cloaked != 0
                {
                    self.hide();
                    return Ok(());
                }
                let mut rect = RECT::default();
                DwmGetWindowAttribute(
                    HWND(id as _),
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&mut rect as *mut RECT).cast(),
                    std::mem::size_of::<RECT>() as u32,
                )?;
                [
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                ]
            },
        };
        if self.last == Some((rect, paused)) {
            return Ok(());
        }
        unsafe {
            let hwnd = HWND(self.hwnd as _);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, isize::from(paused));
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect[0],
                rect[1],
                rect[2],
                rect[3],
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        self.last = Some((rect, paused));
        self.target = Some(target.clone());
        Ok(())
    }
    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(HWND(self.hwnd as _), SW_HIDE);
        }
        self.last = None;
        self.target = None;
    }
    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(HWND(self.hwnd as _)).as_bool() }
    }
}
impl Drop for CaptureIndicator {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(HWND(self.hwnd as _));
        }
    }
}
unsafe extern "system" fn indicator_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(dc, &rect, HBRUSH(GetStockObject(BLACK_BRUSH).0));
                let color = if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0 {
                    COLORREF(0x005bcdf5)
                } else {
                    COLORREF(0x005d5dee)
                };
                let pen = CreatePen(PS_SOLID, 2, color);
                let old_pen = SelectObject(dc, HGDIOBJ(pen.0));
                let old_brush = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
                let _ = Rectangle(dc, 1, 1, rect.right - 1, rect.bottom - 1);
                SelectObject(dc, old_pen);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                let corner = CreatePen(PS_SOLID, 5, color);
                let old_pen = SelectObject(dc, HGDIOBJ(corner.0));
                for (x, y, dx, dy) in [
                    (3, 3, 1, 1),
                    (rect.right - 4, 3, -1, 1),
                    (3, rect.bottom - 4, 1, -1),
                    (rect.right - 4, rect.bottom - 4, -1, -1),
                ] {
                    let len = 20.min(rect.right / 3).min(rect.bottom / 3);
                    let _ = MoveToEx(dc, x + dx * len, y, None);
                    let _ = LineTo(dc, x, y);
                    let _ = LineTo(dc, x, y + dy * len);
                }
                SelectObject(dc, old_pen);
                SelectObject(dc, old_brush);
                let _ = DeleteObject(HGDIOBJ(corner.0));
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            _ => (),
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn native_boundary_is_click_through_excluded_and_released() {
        let mut boundary = CaptureIndicator::new().unwrap();
        let hwnd = HWND(boundary.hwnd as _);
        let target = CaptureTarget::Region {
            monitor: 0,
            rect: opencap_core::Rect {
                x: 10,
                y: 10,
                width: 320,
                height: 180,
            },
        };
        boundary.update(&target, false).unwrap();
        unsafe {
            assert!(IsWindowVisible(hwnd).as_bool());
            let mut affinity = 0;
            GetWindowDisplayAffinity(hwnd, &mut affinity).unwrap();
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            assert_eq!(
                SendMessageW(hwnd, WM_NCHITTEST, Some(WPARAM(0)), Some(LPARAM(0))).0,
                HTTRANSPARENT as isize
            );
            assert_ne!(
                GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0,
                0
            );
        }
        boundary.update(&target, true).unwrap();
        unsafe {
            assert_eq!(GetWindowLongPtrW(hwnd, GWLP_USERDATA), 1);
        }
        boundary.hide();
        unsafe {
            assert!(!IsWindowVisible(hwnd).as_bool());
        }
        drop(boundary);
        unsafe {
            assert!(!IsWindow(Some(hwnd)).as_bool());
        }
    }
}
