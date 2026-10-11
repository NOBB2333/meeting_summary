use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use opencap_core::{Rect, RegionEditor, RegionHandle};
use std::thread::JoinHandle;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::w,
};

#[derive(Debug)]
pub enum RegionEvent {
    Changed(Rect),
    Start(Rect),
    Closed,
}

/// Modeless editor. Only its border and toolbar receive input after the first drag.
pub struct RegionSelection {
    pub events: Receiver<RegionEvent>,
    hwnd: isize,
    worker: Option<JoinHandle<()>>,
}
impl Drop for RegionSelection {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn edit_region(monitor: usize, initial: Option<Rect>) -> Result<RegionSelection> {
    let (tx, events) = unbounded();
    let (ready_tx, ready_rx) = bounded(1);
    let worker = std::thread::spawn(move || {
        if let Err(error) = run_editor(monitor, initial, tx, &ready_tx) {
            let _ = ready_tx.try_send(Err(format!("{error:#}")));
        }
    });
    let hwnd = ready_rx
        .recv()
        .context("Region editor exited")?
        .map_err(anyhow::Error::msg)?;
    Ok(RegionSelection {
        events,
        hwnd,
        worker: Some(worker),
    })
}
struct Selection {
    editor: RegionEditor,
    sender: Sender<RegionEvent>,
    width: i32,
    height: i32,
    editing: bool,
    dragging: bool,
}
impl Selection {
    fn toolbar(&self) -> RECT {
        let (x, y) = self
            .editor
            .rect
            .map(|r| {
                let x = (r.x as i32).clamp(0, (self.width - 520).max(0));
                let bottom = (r.y + r.height) as i32;
                let y = if bottom + 60 < self.height {
                    bottom + 12
                } else if r.y >= 60 {
                    r.y as i32 - 58
                } else {
                    12
                };
                (x, y)
            })
            .unwrap_or((20, 20));
        RECT {
            left: x,
            top: y,
            right: x + 520,
            bottom: y + 46,
        }
    }
}
fn run_editor(
    monitor: usize,
    initial: Option<Rect>,
    sender: Sender<RegionEvent>,
    ready: &Sender<Result<isize, String>>,
) -> Result<()> {
    let (x, y, width, height) = super::monitor_bounds(monitor)?;
    unsafe {
        let old_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let result = (|| -> Result<()> {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(selection_proc),
                hInstance: instance.into(),
                lpszClassName: w!("OpenCapRegion"),
                hCursor: LoadCursorW(None, IDC_CROSS)?,
                ..Default::default()
            };
            RegisterClassW(&class);
            let editor = RegionEditor::new(width, height, initial);
            let editing = editor.rect.is_some();
            let mut state = Box::new(Selection {
                editor,
                sender,
                width: width as i32,
                height: height as i32,
                editing,
                dragging: false,
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                w!("OpenCapRegion"),
                w!("OpenCap 区域编辑"),
                WS_POPUP,
                x,
                y,
                width as i32,
                height as i32,
                None,
                None,
                Some(instance.into()),
                Some((&mut *state as *mut Selection).cast()),
            )?;
            SetLayeredWindowAttributes(
                hwnd,
                COLORREF(0),
                if editing { 255 } else { 75 },
                LWA_ALPHA,
            )?;
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            shape(hwnd, &state);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            ready.send(Ok(hwnd.0 as isize))?;
            if let Some(rect) = state.editor.rect {
                let _ = state.sender.send(RegionEvent::Changed(rect));
            }
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            Ok(())
        })();
        SetThreadDpiAwarenessContext(old_dpi);
        result
    }
}
unsafe fn shape(hwnd: HWND, s: &Selection) {
    unsafe {
        if !s.editing {
            return;
        }
        if let Some(r) = s.editor.rect {
            let (l, t, rr, b) = (
                r.x as i32,
                r.y as i32,
                (r.x + r.width) as i32,
                (r.y + r.height) as i32,
            );
            let region = CreateRectRgn(l - 8, t - 8, rr + 9, b + 9);
            let inside = CreateRectRgn(l + 8, t + 8, (rr - 8).max(l + 8), (b - 8).max(t + 8));
            CombineRgn(Some(region), Some(region), Some(inside), RGN_DIFF);
            let bar = s.toolbar();
            let toolbar = CreateRectRgn(bar.left, bar.top, bar.right, bar.bottom);
            CombineRgn(Some(region), Some(region), Some(toolbar), RGN_OR);
            let _ = DeleteObject(HGDIOBJ(inside.0));
            let _ = DeleteObject(HGDIOBJ(toolbar.0));
            if SetWindowRgn(hwnd, Some(region), false) == 0 {
                let _ = DeleteObject(HGDIOBJ(region.0));
            }
        }
    }
}
unsafe fn commit_drag(hwnd: HWND, s: &mut Selection) {
    unsafe {
        s.editor.end();
        s.dragging = false;
        let _ = ReleaseCapture();
        if let Some(rect) = s.editor.rect {
            s.editing = true;
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            shape(hwnd, s);
            let _ = s.sender.send(RegionEvent::Changed(rect));
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}
unsafe extern "system" fn selection_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Selection;
        if !state.is_null() {
            let s = &mut *state;
            let x = (lp.0 as u16 as i16 as i32).clamp(0, s.width);
            let y = ((lp.0 >> 16) as u16 as i16 as i32).clamp(0, s.height);
            match message {
                WM_ERASEBKGND => return LRESULT(1),
                WM_LBUTTONDOWN => {
                    let bar = s.toolbar();
                    if s.editing && x >= bar.left && x < bar.right && y >= bar.top && y < bar.bottom
                    {
                        let offset = x - bar.left;
                        if offset >= 430 {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        if offset >= 300 {
                            if let Some(rect) = s.editor.rect {
                                let _ = s.sender.send(RegionEvent::Start(rect));
                            }
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        s.editor.begin_move(x, y);
                    } else {
                        s.editor.begin(x, y);
                    }
                    s.dragging = true;
                    SetCapture(hwnd);
                    return LRESULT(0);
                }
                WM_MOUSEMOVE => {
                    let old = s.editor.rect;
                    if s.dragging {
                        s.editor.update(x, y);
                    }
                    if s.editor.rect != old {
                        shape(hwnd, s);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let cursor = match s.editor.hit_test(x, y) {
                        RegionHandle::New if !s.editing => IDC_CROSS,
                        RegionHandle::Resize {
                            left,
                            top,
                            right,
                            bottom,
                        } if (left && top) || (right && bottom) => IDC_SIZENWSE,
                        RegionHandle::Resize {
                            left,
                            top,
                            right,
                            bottom,
                        } if (right && top) || (left && bottom) => IDC_SIZENESW,
                        RegionHandle::Resize { left, right, .. } if left || right => IDC_SIZEWE,
                        RegionHandle::Resize { .. } => IDC_SIZENS,
                        _ => IDC_SIZEALL,
                    };
                    SetCursor(LoadCursorW(None, cursor).ok());
                    return LRESULT(0);
                }
                WM_LBUTTONUP if s.dragging => {
                    s.editor.update(x, y);
                    commit_drag(hwnd, s);
                    return LRESULT(0);
                }
                WM_KEYDOWN if wp.0 as u16 == VK_RETURN.0 => {
                    commit_drag(hwnd, s);
                    return LRESULT(0);
                }
                WM_CAPTURECHANGED => {
                    s.dragging = false;
                    s.editor.end();
                    return LRESULT(0);
                }
                WM_RBUTTONDOWN | WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_PAINT => {
                    paint(hwnd, s);
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    let _ = s.sender.send(RegionEvent::Closed);
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                _ => (),
            }
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
}
unsafe fn paint(hwnd: HWND, s: &Selection) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let screen = BeginPaint(hwnd, &mut ps);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, s.width, s.height);
        let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
        let bounds = RECT {
            left: 0,
            top: 0,
            right: s.width,
            bottom: s.height,
        };
        FillRect(dc, &bounds, HBRUSH(GetStockObject(BLACK_BRUSH).0));
        SetTextColor(dc, COLORREF(0xffffff));
        SetBkMode(dc, TRANSPARENT);
        let font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        if let Some(r) = s.editor.rect {
            let pen = CreatePen(PS_SOLID, 3, COLORREF(0x62c8ff));
            let old_pen = SelectObject(dc, HGDIOBJ(pen.0));
            let old_brush = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
            let (l, t, rr, b) = (
                r.x as i32,
                r.y as i32,
                (r.x + r.width) as i32,
                (r.y + r.height) as i32,
            );
            let _ = Rectangle(dc, l, t, rr, b);
            for (x, y) in [
                (l, t),
                (rr, t),
                (l, b),
                (rr, b),
                ((l + rr) / 2, t),
                ((l + rr) / 2, b),
                (l, (t + b) / 2),
                (rr, (t + b) / 2),
            ] {
                let _ = Rectangle(dc, x - 5, y - 5, x + 6, y + 6);
            }
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
        if s.editing {
            let bar = s.toolbar();
            let brush = CreateSolidBrush(COLORREF(0x363029));
            FillRect(dc, &bar, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let r = s.editor.rect.unwrap();
            let text = format!("↔ 移动区域  {} × {}  |  拖边角缩放", r.width, r.height)
                .encode_utf16()
                .collect::<Vec<_>>();
            let _ = TextOutW(dc, bar.left + 10, bar.top + 15, &text);
            let start = RECT {
                left: bar.left + 300,
                top: bar.top,
                right: bar.left + 430,
                bottom: bar.bottom,
            };
            let brush = CreateSolidBrush(COLORREF(0x6b7650));
            FillRect(dc, &start, brush);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let _ = TextOutW(
                dc,
                start.left + 26,
                start.top + 15,
                &"开始录制".encode_utf16().collect::<Vec<_>>(),
            );
            let _ = TextOutW(
                dc,
                bar.left + 451,
                bar.top + 15,
                &"收起".encode_utf16().collect::<Vec<_>>(),
            );
        } else {
            let _ = TextOutW(
                dc,
                30,
                30,
                &"拖动框选 · 松开后可继续调整并开始录制 · Esc 取消"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            );
        }
        let _ = BitBlt(screen, 0, 0, s.width, s.height, Some(dc), 0, 0, SRCCOPY);
        SelectObject(dc, font);
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
        let _ = EndPaint(hwnd, &ps);
    }
}
