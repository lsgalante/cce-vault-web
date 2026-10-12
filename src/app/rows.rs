//! A scrolling list of tappable rows: the file browser, search results,
//! backlinks, tasks and the action sheets are all one of these. A finger
//! scrolls it through the toolkit's touch path (a drag arrives as a pixel
//! wheel delta, a tap as a press and release), so it needs no touch code.

use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::PaintCtx;
use cce_ui::widget::markdown::srgb_u8;
use cce_ui::widget::{Bounds, MouseScrollDelta, ScrollMotion};

pub const ROW_H: f32 = 52.0;
const SUB_ROW_H: f32 = 64.0;
const HEAD_ROW_H: f32 = 34.0;
pub const PAD: f32 = 16.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Folder,
    Note,
    Attachment,
    /// A task: its checkbox state.
    Task(bool),
    /// A section heading; not tappable.
    Heading,
    /// A command row (an action sheet's entries).
    Action,
    /// A command that destroys something: drawn in the warning colour.
    Danger,
    /// Explanatory text; not tappable.
    Note2,
}

#[derive(Clone, Debug)]
pub struct Row<T> {
    pub title: String,
    pub subtitle: Option<String>,
    pub kind: Kind,
    pub target: T,
}

impl<T> Row<T> {
    pub fn new(title: impl Into<String>, kind: Kind, target: T) -> Row<T> {
        Row { title: title.into(), subtitle: None, kind, target }
    }

    pub fn with_sub(mut self, subtitle: impl Into<String>) -> Row<T> {
        let s = subtitle.into();
        self.subtitle = (!s.is_empty()).then_some(s);
        self
    }

    fn height(&self) -> f32 {
        match (&self.kind, &self.subtitle) {
            (Kind::Heading, _) => HEAD_ROW_H,
            (_, Some(_)) => SUB_ROW_H,
            _ => ROW_H,
        }
    }

    fn tappable(&self) -> bool {
        !matches!(self.kind, Kind::Heading | Kind::Note2)
    }
}

pub struct Rows<T> {
    pub rows: Vec<Row<T>>,
    pub scroll: f32,
    motion: ScrollMotion,
    /// The row under a press that has not been released yet.
    pub pressed: Option<usize>,
    /// What an empty list says.
    pub empty: String,
}

impl<T> Default for Rows<T> {
    fn default() -> Self {
        Rows { rows: Vec::new(), scroll: 0.0, motion: ScrollMotion::new(), pressed: None, empty: String::new() }
    }
}

pub struct Fonts {
    pub family: String,
    pub size: f32,
    pub small: f32,
}

impl<T> Rows<T> {
    /// Replace the rows; `keep_scroll` holds the offset (a refresh of the
    /// same list), otherwise it starts at the top.
    pub fn set(&mut self, rows: Vec<Row<T>>, keep_scroll: bool) {
        self.rows = rows;
        self.pressed = None;
        if !keep_scroll {
            self.scroll = 0.0;
            self.motion = ScrollMotion::new();
        }
    }

    fn content_height(&self) -> f32 {
        self.rows.iter().map(Row::height).sum::<f32>() + 8.0
    }

    pub fn max_scroll(&self, view: Rect) -> f32 {
        (self.content_height() - view.height).max(0.0)
    }

    pub fn clamp(&mut self, view: Rect) {
        let max = self.max_scroll(view);
        if self.scroll > max {
            self.scroll = max;
            self.motion.y.jump_to(max);
        }
    }

    /// The tappable row at (x, y).
    pub fn hit(&self, view: Rect, x: f32, y: f32) -> Option<usize> {
        if !view.contains(x, y) {
            return None;
        }
        let mut top = view.y + 4.0 - self.scroll;
        for (i, r) in self.rows.iter().enumerate() {
            let h = r.height();
            if y >= top && y < top + h {
                return r.tappable().then_some(i);
            }
            top += h;
        }
        None
    }

    pub fn wheel(&mut self, delta: &MouseScrollDelta, view: Rect) -> bool {
        let max = self.max_scroll(view);
        self.motion.reconcile(0.0, self.scroll);
        if self.motion.apply(delta, (ROW_H, ROW_H * 3.0), Bounds::max(0.0), Bounds::max(max)) {
            self.scroll = self.motion.y.pos();
            // A scroll is not a tap: drop the press highlight.
            self.pressed = None;
            return true;
        }
        false
    }

    pub fn tick(&mut self, dt: f32, view: Rect) -> bool {
        if !self.motion.is_animating() {
            return false;
        }
        let max = self.max_scroll(view);
        self.motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
        self.scroll = self.motion.y.pos();
        true
    }

    pub fn paint(&self, pc: &mut PaintCtx, view: Rect, f: &Fonts) {
        let fg = srgb_u8(cce_ui::color::TEXT_FG);
        let dim = srgb_u8(cce_ui::color::TEXT_DIM);
        if self.rows.is_empty() && !self.empty.is_empty() {
            pc.text_with(self.empty.clone(), view.x + PAD, view.y + PAD, f.size, dim, Some(f.family.clone()), None);
            return;
        }
        let bounds = [view.x, view.y, view.x + view.width, view.y + view.height];
        pc.clip(view, |pc| {
            let mut top = view.y + 4.0 - self.scroll;
            for (i, r) in self.rows.iter().enumerate() {
                let h = r.height();
                if top + h < view.y || top > view.y + view.height {
                    top += h;
                    continue;
                }
                let row = Rect { x: view.x + 6.0, y: top, width: view.width - 12.0, height: h };
                if self.pressed == Some(i) {
                    pc.rounded_rect(row, 8.0, (true, true, true, true), cce_ui::color::PANEL_MENU_HOVER);
                }
                let mut x = view.x + PAD;
                match r.kind {
                    Kind::Heading => {
                        let y = top + h - f.small * 1.4;
                        pc.text_with(r.title.to_uppercase(), x, y, f.small, dim, Some(f.family.clone()), Some(bounds));
                        top += h;
                        continue;
                    }
                    Kind::Task(done) => {
                        let cy = top + h / 2.0;
                        let r_ = 9.0;
                        pc.circle(x + r_, cy, r_, if done { cce_ui::color::text_link_color() } else { cce_ui::color::TEXT_DIM });
                        if !done {
                            pc.circle(x + r_, cy, r_ - 2.0, cce_ui::color::PANEL_MENU_BG);
                        }
                        x += 2.0 * r_ + 12.0;
                    }
                    Kind::Folder => {
                        // A folder reads as a tab-topped box, drawn small and dim.
                        let (bx, by) = (x, top + h / 2.0 - 7.0);
                        pc.rounded_rect(Rect { x: bx, y: by, width: 8.0, height: 4.0 }, 1.5, (true, true, false, false), cce_ui::color::TEXT_DIM);
                        pc.rounded_rect(Rect { x: bx, y: by + 3.0, width: 18.0, height: 12.0 }, 2.5, (true, true, true, true), cce_ui::color::TEXT_DIM);
                        x += 30.0;
                    }
                    _ => {}
                }
                let color = match r.kind {
                    Kind::Danger => [0xe0, 0x6c, 0x6c],
                    Kind::Attachment | Kind::Note2 => dim,
                    Kind::Task(true) => dim,
                    _ => fg,
                };
                let title_y = match &r.subtitle {
                    Some(_) => top + 11.0,
                    None => cce_ui::layout::align_text_y(top, h, f.size, 0.0),
                };
                let right = [view.x, view.y, view.x + view.width - PAD, view.y + view.height];
                pc.text_with(r.title.clone(), x, title_y, f.size, color, Some(f.family.clone()), Some(right));
                if let Some(sub) = &r.subtitle {
                    pc.text_with(sub.clone(), x, top + 13.0 + f.size * 1.3, f.small, dim, Some(f.family.clone()), Some(right));
                }
                if matches!(r.kind, Kind::Folder) {
                    // A chevron: this row drills down.
                    let (cx, cy) = (view.x + view.width - PAD - 4.0, top + h / 2.0);
                    let c = cce_ui::color::TEXT_DIM;
                    pc.vector(cx - 5.0, cy - 6.0, cx, cy, 2.0, c, cce_ui::scene::paint::Cap::Round);
                    pc.vector(cx, cy, cx - 5.0, cy + 6.0, 2.0, c, cce_ui::scene::paint::Cap::Round);
                }
                // A hairline between rows.
                if i + 1 < self.rows.len() && !matches!(self.rows[i + 1].kind, Kind::Heading) {
                    pc.quad(Rect { x: view.x + PAD, y: top + h - 0.5, width: view.width - 2.0 * PAD, height: 0.5 }, [1.0, 1.0, 1.0, 0.06]);
                }
                top += h;
            }
        });
    }
}
