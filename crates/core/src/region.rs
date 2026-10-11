//! Platform-independent region editing in physical monitor coordinates.
use crate::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionHandle {
    New,
    Move,
    Resize {
        left: bool,
        top: bool,
        right: bool,
        bottom: bool,
    },
}

pub struct RegionEditor {
    pub rect: Option<Rect>,
    width: i32,
    height: i32,
    drag: Option<(RegionHandle, (i32, i32), Option<Rect>)>,
}
impl RegionEditor {
    pub fn new(width: u32, height: u32, initial: Option<Rect>) -> Self {
        let rect = initial.filter(|r| {
            r.width >= 2
                && r.height >= 2
                && r.x.checked_add(r.width).is_some_and(|v| v <= width)
                && r.y.checked_add(r.height).is_some_and(|v| v <= height)
        });
        Self {
            rect,
            width: width as i32,
            height: height as i32,
            drag: None,
        }
    }
    pub fn hit_test(&self, x: i32, y: i32) -> RegionHandle {
        let Some(r) = self.rect else {
            return RegionHandle::New;
        };
        let (l, t, rr, b) = (
            r.x as i32,
            r.y as i32,
            (r.x + r.width) as i32,
            (r.y + r.height) as i32,
        );
        if x < l - 8 || x > rr + 8 || y < t - 8 || y > b + 8 {
            return RegionHandle::New;
        }
        let (left, top, right, bottom) = (
            (x - l).abs() <= 8,
            (y - t).abs() <= 8,
            (x - rr).abs() <= 8,
            (y - b).abs() <= 8,
        );
        if left || top || right || bottom {
            RegionHandle::Resize {
                left,
                top,
                right,
                bottom,
            }
        } else {
            RegionHandle::Move
        }
    }
    pub fn begin(&mut self, x: i32, y: i32) {
        self.drag = Some((self.hit_test(x, y), (x, y), self.rect));
    }
    pub fn begin_move(&mut self, x: i32, y: i32) {
        self.drag = Some((RegionHandle::Move, (x, y), self.rect));
    }
    pub fn update(&mut self, x: i32, y: i32) {
        let Some((handle, (sx, sy), original)) = self.drag else {
            return;
        };
        let (x, y) = (x.clamp(0, self.width), y.clamp(0, self.height));
        let (l, t, r, b) = match (handle, original) {
            (RegionHandle::Move, Some(r)) => {
                let l = (r.x as i32 + x - sx).clamp(0, self.width - r.width as i32);
                let t = (r.y as i32 + y - sy).clamp(0, self.height - r.height as i32);
                (l, t, l + r.width as i32, t + r.height as i32)
            }
            (
                RegionHandle::Resize {
                    left,
                    top,
                    right,
                    bottom,
                },
                Some(r),
            ) => {
                let (mut l, mut t, mut rr, mut b) = (
                    r.x as i32,
                    r.y as i32,
                    (r.x + r.width) as i32,
                    (r.y + r.height) as i32,
                );
                if left {
                    l = x.min(rr - 2);
                }
                if right {
                    rr = x.max(l + 2);
                }
                if top {
                    t = y.min(b - 2);
                }
                if bottom {
                    b = y.max(t + 2);
                }
                (l, t, rr, b)
            }
            _ => (sx.min(x), sy.min(y), sx.max(x), sy.max(y)),
        };
        let (width, height) = ((r - l) as u32 & !1, (b - t) as u32 & !1);
        if width >= 2 && height >= 2 {
            self.rect = Some(Rect {
                x: l as u32,
                y: t as u32,
                width,
                height,
            });
        }
    }
    pub fn end(&mut self) {
        self.drag = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn select_move_resize_and_clamp() {
        let mut e = RegionEditor::new(1920, 1080, None);
        e.begin(600, 400);
        e.update(100, 100);
        e.end();
        assert_eq!(
            e.rect,
            Some(Rect {
                x: 100,
                y: 100,
                width: 500,
                height: 300
            })
        );
        e.begin(300, 200);
        e.update(2200, 1400);
        e.end();
        assert_eq!(
            e.rect,
            Some(Rect {
                x: 1420,
                y: 780,
                width: 500,
                height: 300
            })
        );
        e.begin(1420, 780);
        e.update(1300, 700);
        e.end();
        assert_eq!(
            e.rect,
            Some(Rect {
                x: 1300,
                y: 700,
                width: 620,
                height: 380
            })
        );
        e.begin(1920, 1080);
        e.update(1501, 901);
        e.end();
        assert_eq!(
            e.rect,
            Some(Rect {
                x: 1300,
                y: 700,
                width: 200,
                height: 200
            })
        );
    }
    #[test]
    fn reopen_and_keep_valid_rectangle() {
        let r = Rect {
            x: 10,
            y: 10,
            width: 640,
            height: 360,
        };
        let mut e = RegionEditor::new(1920, 1080, Some(r));
        assert_eq!(
            e.hit_test(10, 10),
            RegionHandle::Resize {
                left: true,
                top: true,
                right: false,
                bottom: false
            }
        );
        e.begin(10, 10);
        e.update(900, 900);
        e.end();
        assert_eq!(e.rect.unwrap().width, 2);
        assert_eq!(e.rect.unwrap().height, 2);
        assert!(RegionEditor::new(100, 100, Some(r)).rect.is_none());
    }
    #[test]
    fn selected_region_remains_editable_after_mouse_release() {
        let mut e = RegionEditor::new(1920, 1080, None);
        e.begin(100, 120);
        e.update(700, 520);
        e.end();

        // A second drag inside the completed selection moves it; no Enter key is needed.
        e.begin(400, 300);
        e.update(520, 380);
        e.end();
        assert_eq!(
            e.rect,
            Some(Rect {
                x: 220,
                y: 200,
                width: 600,
                height: 400,
            })
        );

        // A third drag on an edge resizes the completed selection.
        e.begin(820, 400);
        e.update(920, 400);
        e.end();
        assert_eq!(e.rect.unwrap().width, 700);
    }
}
