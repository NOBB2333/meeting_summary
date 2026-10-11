use anyhow::Result;
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::*,
    UI::WindowsAndMessaging::IsIconic,
};

/// DWM composites the source HWND directly. No capture threads, readback or video encoding.
pub struct WindowPreview {
    thumbnail: isize,
    source: isize,
    last: Cell<Option<([i32; 4], [i32; 4])>>,
}
impl WindowPreview {
    pub fn new(destination: isize, source: isize) -> Result<Self> {
        let thumbnail = unsafe { DwmRegisterThumbnail(HWND(destination as _), HWND(source as _))? };
        Ok(Self {
            thumbnail,
            source,
            last: Cell::new(None),
        })
    }
    pub fn minimized(&self) -> bool {
        unsafe { IsIconic(HWND(self.source as _)).as_bool() }
    }
    pub fn hide(&self) {
        self.last.set(None);
        unsafe {
            let _ = DwmUpdateThumbnailProperties(
                self.thumbnail,
                &DWM_THUMBNAIL_PROPERTIES {
                    dwFlags: DWM_TNP_VISIBLE,
                    fVisible: false.into(),
                    ..Default::default()
                },
            );
        }
    }
    /// Destination and clip are physical client coordinates, including the scroll mask.
    pub fn place(&self, area: [i32; 4], clip: [i32; 4]) -> Result<()> {
        let size = unsafe { DwmQueryThumbnailSourceSize(self.thumbnail)? };
        let Some((dest, source)) = thumbnail_geometry([size.cx, size.cy], area, clip) else {
            self.hide();
            return Ok(());
        };
        if self.last.get() == Some((dest, source)) {
            return Ok(());
        }
        let rect = |r: [i32; 4]| RECT {
            left: r[0],
            top: r[1],
            right: r[2],
            bottom: r[3],
        };
        unsafe {
            DwmUpdateThumbnailProperties(
                self.thumbnail,
                &DWM_THUMBNAIL_PROPERTIES {
                    dwFlags: DWM_TNP_RECTDESTINATION
                        | DWM_TNP_RECTSOURCE
                        | DWM_TNP_VISIBLE
                        | DWM_TNP_OPACITY
                        | DWM_TNP_SOURCECLIENTAREAONLY,
                    rcDestination: rect(dest),
                    rcSource: rect(source),
                    opacity: 255,
                    fVisible: true.into(),
                    fSourceClientAreaOnly: false.into(),
                },
            )?;
        }
        self.last.set(Some((dest, source)));
        Ok(())
    }
}
impl Drop for WindowPreview {
    fn drop(&mut self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.thumbnail);
        }
    }
}
fn thumbnail_geometry(
    size: [i32; 2],
    area: [i32; 4],
    clip: [i32; 4],
) -> Option<([i32; 4], [i32; 4])> {
    let [sw, sh] = size;
    let aw = area[2] - area[0];
    let ah = area[3] - area[1];
    if sw <= 0 || sh <= 0 || aw <= 0 || ah <= 0 {
        return None;
    }
    let scale = (aw as f64 / sw as f64).min(ah as f64 / sh as f64);
    let w = (sw as f64 * scale).round().max(1.0) as i32;
    let h = (sh as f64 * scale).round().max(1.0) as i32;
    let x = area[0] + (aw - w) / 2;
    let y = area[1] + (ah - h) / 2;
    let dest = [
        x.max(clip[0]),
        y.max(clip[1]),
        (x + w).min(clip[2]),
        (y + h).min(clip[3]),
    ];
    if dest[0] >= dest[2] || dest[1] >= dest[3] {
        return None;
    }
    let source = [
        ((dest[0] - x) as f64 / w as f64 * sw as f64).round() as i32,
        ((dest[1] - y) as f64 / h as f64 * sh as f64).round() as i32,
        ((dest[2] - x) as f64 / w as f64 * sw as f64).round() as i32,
        ((dest[3] - y) as f64 / h as f64 * sh as f64).round() as i32,
    ];
    Some((dest, source))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thumbnails_preserve_aspect_ratio_and_clip_source_when_scrolling() {
        assert_eq!(
            thumbnail_geometry([1920, 1080], [0, 0, 200, 200], [0, 0, 200, 200]),
            Some(([0, 43, 200, 156], [0, 0, 1920, 1080]))
        );
        let (d, s) = thumbnail_geometry([400, 400], [10, -50, 210, 150], [0, 0, 300, 100]).unwrap();
        assert_eq!(d, [10, 0, 210, 100]);
        assert_eq!(s, [0, 100, 400, 300]);
        assert!(thumbnail_geometry([400, 400], [0, -300, 200, -100], [0, 0, 300, 100]).is_none());
    }
}
