//! The app: a phone-first, one-pane navigator over the vault. Every screen
//! fills the window, under a header (back, title, the screen's own
//! buttons) and over a toolbar (Files, Search, Today, Tasks, New), the way
//! a phone app is laid out; on a wide window the same screens just have
//! more room. Notes open in the reading view (cce-ui's `MarkdownView`
//! layout) and edit in live preview (`DocEditor`).
//!
//! It is the same `Application` in the browser and natively (the native
//! build is for development: `examples/native.rs` runs it on a folder in a
//! shadow session). Everything platform-shaped — where files are kept,
//! Dropbox — is behind [`store::Backend`], installed before the shell
//! starts ([`install`]).

pub mod rows;
pub mod store;

use std::cell::RefCell;
use std::time::Duration;

use cce_ui::context::UiContext;
use cce_ui::engine::{AppSender, Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::doc_editor::{self, DocEditor, EditorTheme};
use cce_ui::widget::markdown::{self, srgb_u8, Hit, ShapingMeasure, SpanLink, Theme};
use cce_ui::widget::{
    Adapted, Bounds, Button, ElementState, Event, Handle, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey,
    ScrollMotion, TextBox, WidgetHost, WidgetId,
};
use cce_vault::FileKind;
use web_time::Instant;

use rows::{Fonts, Kind, Row, Rows, PAD};
use store::{Backend, Msg, NullBackend, Shared, Store};

const HEADER_H: f32 = 52.0;
const TOOLBAR_H: f32 = 56.0;
const BODY_SIZE: f32 = 17.0;
const SMALL_SIZE: f32 = 13.0;
const TITLE_SIZE: f32 = 17.0;
/// The reading column's widest (Obsidian's readable line length).
const READ_MAX_W: f32 = 720.0;
const READ_PAD: f32 = 18.0;
const FORM_FIELD_H: f32 = 44.0;
/// Save an edit this long after typing stops.
const AUTOSAVE_AFTER: Duration = Duration::from_millis(1200);
/// The Dropbox folder offered on first run.
pub const DEFAULT_ROOT: &str = "/Apps/remotely-save/Vault 1";

thread_local! {
    static INSTALL: RefCell<Option<Installer>> = const { RefCell::new(None) };
}

/// Builds the store and backend once the shell hands the app its sender.
pub type Installer = Box<dyn FnOnce(AppSender<Msg>) -> (Shared, Box<dyn Backend>)>;

/// Set what [`VaultApp::create`] runs to get its vault and backend. Call
/// before starting the shell. Without one the app opens an empty vault
/// with no storage (tests).
pub fn install(f: Installer) {
    INSTALL.with(|i| *i.borrow_mut() = Some(f));
}

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    /// First run: which Dropbox folder, and the login.
    Setup,
    /// A folder of the vault ("" = the root).
    Files(String),
    Note(String),
    Search,
    Tasks,
    Backlinks(String),
    /// A note's actions: backlinks, rename, delete.
    Actions(String),
    /// The vault's settings and sync state.
    Vault,
    /// Name a new note in a folder.
    NewNote(String),
    Rename(String),
}

/// What a tapped row does.
#[derive(Clone, Debug)]
enum Target {
    Folder(String),
    Open(String),
    /// Open a note scrolled to a line.
    OpenAt(String, usize),
    Task { path: String, line: usize, done: bool },
    Backlinks(String),
    Rename(String),
    Delete(String),
    SyncNow,
    Disconnect,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Read,
    Edit,
}

pub struct VaultApp {
    store: Shared,
    backend: Box<dyn Backend>,
    stack: Vec<Screen>,
    rows: Rows<Target>,
    ui: UiContext,
    back: Handle<Adapted<Button>>,
    edit: Handle<Adapted<Button>>,
    more: Handle<Adapted<Button>>,
    tools: [Handle<Adapted<Button>>; 5],
    search_box: Handle<Adapted<TextBox>>,
    name_box: Handle<Adapted<TextBox>>,
    root_box: Handle<Adapted<TextBox>>,
    primary: Handle<Adapted<Button>>,
    // The open note.
    mode: Mode,
    editor: DocEditor,
    /// The text last saved from (or loaded into) the editor.
    saved_text: String,
    edit_seen: u64,
    edit_changed_at: Instant,
    read_layout: Option<(String, f32, markdown::Layout)>,
    read_scroll: f32,
    read_motion: ScrollMotion,
    read_pending_line: Option<usize>,
    measure: Option<ShapingMeasure>,
    press_at: Option<(f32, f32)>,
    editor_pressed: bool,
    /// A delete waiting for its second tap.
    confirm_delete: Option<String>,
    show_done: bool,
    width: f32,
    height: f32,
    needs_layout: bool,
    status: String,
}

fn fonts() -> Fonts {
    Fonts { family: "sans-serif".into(), size: BODY_SIZE, small: SMALL_SIZE }
}

/// A note's name without folders or `.md`.
fn title_of(path: &str) -> String {
    cce_vault::index::stem(path).to_string()
}

impl VaultApp {
    fn screen(&self) -> Screen {
        self.stack.last().cloned().unwrap_or(Screen::Files(String::new()))
    }

    fn header(&self) -> Rect {
        Rect { x: 0.0, y: 0.0, width: self.width, height: HEADER_H }
    }

    fn has_toolbar(&self) -> bool {
        !matches!(self.screen(), Screen::Setup) && !(matches!(self.screen(), Screen::Note(_)) && self.mode == Mode::Edit)
    }

    fn body(&self) -> Rect {
        let bottom = if self.has_toolbar() { self.height - TOOLBAR_H } else { self.height };
        Rect { x: 0.0, y: HEADER_H, width: self.width, height: (bottom - HEADER_H).max(0.0) }
    }

    /// The list area: the body, below a form's field when the screen has one.
    fn list_rect(&self) -> Rect {
        let b = self.body();
        match self.screen() {
            Screen::Search => Rect { y: b.y + FORM_FIELD_H + 16.0, height: (b.height - FORM_FIELD_H - 16.0).max(0.0), ..b },
            _ => b,
        }
    }

    fn root_ids(&self) -> Vec<WidgetId> {
        let mut ids = vec![self.back.id(), self.edit.id(), self.more.id(), self.search_box.id(), self.name_box.id()];
        ids.extend([self.root_box.id(), self.primary.id()]);
        ids.extend(self.tools.iter().map(|t| t.id()));
        ids
    }

    // ---- navigation -----------------------------------------------------

    fn go(&mut self, screen: Screen) {
        self.leave_note();
        self.stack.push(screen);
        self.enter();
    }

    fn back(&mut self) {
        if self.stack.len() > 1 {
            self.leave_note();
            self.stack.pop();
            self.enter();
        }
    }

    /// Start over at the vault root with `screen` on top (the toolbar).
    fn reset_to(&mut self, screen: Option<Screen>) {
        self.leave_note();
        self.stack = vec![Screen::Files(String::new())];
        if let Some(s) = screen {
            self.stack.push(s);
        }
        self.enter();
    }

    /// Set the screen up as it becomes current.
    fn enter(&mut self) {
        self.confirm_delete = None;
        // A field left editing on the screen just left would keep taking
        // keys while hidden.
        self.ui.clear_focus();
        for h in [self.search_box, self.name_box, self.root_box] {
            self.ui.unfocus_id(h.id());
        }
        self.needs_layout = true;
        match self.screen() {
            Screen::Note(path) => self.open_note(&path),
            Screen::NewNote(_) => {
                self.ui[self.name_box].set_text("");
            }
            Screen::Rename(path) => {
                let shown = path.strip_suffix(".md").unwrap_or(&path).to_string();
                self.ui[self.name_box].set_text(&shown);
            }
            _ => {}
        }
        self.refresh_rows(false);
    }

    fn open_note(&mut self, path: &str) {
        let text = self.store.borrow().index.read_text(path).unwrap_or_default();
        self.editor.set_text(&text);
        self.saved_text = text;
        self.edit_seen = self.editor.buf.revision;
        self.read_layout = None;
        self.read_scroll = 0.0;
        self.read_motion = ScrollMotion::new();
        if FileKind::of(path) != FileKind::Note {
            self.mode = Mode::Read;
        }
    }

    /// Save the open note's edits, if any, before leaving it.
    fn leave_note(&mut self) {
        self.save_editor();
        self.mode = Mode::Read;
    }

    fn open_path(&mut self, path: &str, line: Option<usize>) {
        let kind = FileKind::of(path);
        if kind == FileKind::Attachment {
            let held = self.store.borrow().sync.tracked(path).is_none_or(|t| t.held);
            if !held {
                self.backend.fetch_attachment(path);
            }
        }
        self.go(Screen::Note(path.to_string()));
        self.read_pending_line = line;
    }

    // ---- writes ---------------------------------------------------------

    /// Persist what the vault just wrote and schedule a sync.
    fn flush_writes(&mut self) {
        let writes = self.store.borrow_mut().take_writes();
        if !writes.is_empty() {
            self.backend.persist(writes);
            self.backend.sync_soon();
        }
        self.status = self.backend.status();
    }

    fn current_note(&self) -> Option<String> {
        match self.screen() {
            Screen::Note(p) => Some(p),
            _ => None,
        }
    }

    fn save_editor(&mut self) {
        let Some(path) = self.current_note() else { return };
        if self.mode != Mode::Edit {
            return;
        }
        let text = self.editor.text();
        if text == self.saved_text {
            return;
        }
        let r = self.store.borrow_mut().index.write_text(&path, &text);
        match r {
            Ok(()) => {
                self.saved_text = text;
                self.read_layout = None;
                self.flush_writes();
            }
            Err(e) => self.status = format!("Not saved: {e}"),
        }
    }

    fn create_note(&mut self, folder: &str, name: &str) -> Option<String> {
        let name = name.trim().trim_end_matches(".md");
        if name.is_empty() {
            return None;
        }
        let path = if folder.is_empty() { format!("{name}.md") } else { format!("{folder}/{name}.md") };
        let r = self.store.borrow_mut().index.create(&path, "");
        match r {
            Ok(p) => {
                self.flush_writes();
                Some(p)
            }
            Err(e) => {
                self.status = e.to_string();
                None
            }
        }
    }

    fn follow(&mut self, link: SpanLink) {
        let cur = self.current_note();
        match link {
            SpanLink::Note { target, .. } | SpanLink::Embed { target, .. } => {
                if target.is_empty() {
                    return;
                }
                let hit = self.store.borrow().index.resolve_text(cur.as_deref(), &target);
                match hit {
                    Some(p) => self.open_path(&p, None),
                    None => {
                        // Obsidian creates a missing note on click.
                        if let Some(p) = self.create_note("", &target) {
                            self.open_path(&p, None);
                            self.mode = Mode::Edit;
                        }
                    }
                }
            }
            SpanLink::Tag(tag) => {
                self.go(Screen::Search);
                self.ui[self.search_box].set_text(&format!("#{tag}"));
                self.refresh_rows(false);
            }
            SpanLink::Url(url) => self.backend_open_url(&url),
        }
    }

    fn backend_open_url(&mut self, url: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(w) = web_sys::window() {
            let _ = w.open_with_url_and_target(url, "_blank");
        }
        #[cfg(not(target_arch = "wasm32"))]
        log::info!("open {url}");
    }

    fn toggle_task(&mut self, path: &str, line: usize, done: bool) {
        let status = if done { ' ' } else { 'x' };
        let r = self.store.borrow_mut().index.set_task(path, line, status);
        if let Err(e) = r {
            self.status = e.to_string();
        }
        self.read_layout = None;
        self.flush_writes();
        if self.current_note().as_deref() == Some(path) {
            let text = self.store.borrow().index.read_text(path).unwrap_or_default();
            self.editor.set_text(&text);
            self.saved_text = text;
        }
        self.refresh_rows(true);
    }

    fn today(&mut self) {
        let date = chrono::Local::now().date_naive();
        let r = self.store.borrow_mut().index.daily(date, true);
        match r {
            Ok((path, created)) => {
                self.flush_writes();
                self.reset_to(None);
                self.open_path(&path, None);
                if created {
                    self.mode = Mode::Edit;
                }
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    // ---- rows -----------------------------------------------------------

    fn refresh_rows(&mut self, keep_scroll: bool) {
        let store = self.store.clone();
        let st = store.borrow();
        let ix = &st.index;
        let mut empty = String::new();
        let rows: Vec<Row<Target>> = match self.screen() {
            Screen::Files(folder) => {
                empty = "No notes here yet.".into();
                folder_rows(ix, &folder)
            }
            Screen::Search => {
                let q = self.ui[self.search_box].text.trim().to_string();
                empty = if q.is_empty() { "Search note names and text.".into() } else { "Nothing found.".into() };
                search_rows(ix, &q)
            }
            Screen::Tasks => {
                empty = "No tasks in Tasks/.".into();
                task_rows(ix, self.show_done)
            }
            Screen::Backlinks(path) => {
                empty = "No other note links here.".into();
                ix.backlinks(&path)
                    .iter()
                    .map(|b| Row::new(title_of(b.source), Kind::Note, Target::OpenAt(b.source.to_string(), b.link.line)).with_sub(b.source))
                    .collect()
            }
            Screen::Actions(path) => {
                let n = ix.backlinks(&path).len();
                let delete = if self.confirm_delete.as_deref() == Some(&path) { "Tap again to delete" } else { "Delete" };
                vec![
                    Row::new(format!("Backlinks ({n})"), Kind::Action, Target::Backlinks(path.clone())),
                    Row::new("Rename or move", Kind::Action, Target::Rename(path.clone())),
                    Row::new(delete, Kind::Danger, Target::Delete(path.clone())),
                    Row::new(path.clone(), Kind::Note2, Target::None),
                ]
            }
            Screen::Vault => {
                let connected = self.backend.connected();
                let mut v = vec![
                    Row::new("Sync now", Kind::Action, Target::SyncNow),
                    Row::new(self.backend.status(), Kind::Note2, Target::None),
                    Row::new(format!("Dropbox folder: {}", st.root), Kind::Note2, Target::None),
                    Row::new(format!("{} files on this device", ix.files().len()), Kind::Note2, Target::None),
                ];
                if connected {
                    v.push(Row::new("Disconnect Dropbox", Kind::Danger, Target::Disconnect));
                }
                v
            }
            _ => Vec::new(),
        };
        drop(st);
        self.rows.set(rows, keep_scroll);
        self.rows.empty = empty;
        let view = self.list_rect();
        self.rows.clamp(view);
    }

    fn activate(&mut self, target: Target) {
        match target {
            Target::Folder(f) => self.go(Screen::Files(f)),
            Target::Open(p) => self.open_path(&p, None),
            Target::OpenAt(p, line) => self.open_path(&p, Some(line)),
            Target::Task { path, line, done } => self.toggle_task(&path, line, done),
            Target::Backlinks(p) => self.go(Screen::Backlinks(p)),
            Target::Rename(p) => self.go(Screen::Rename(p)),
            Target::Delete(p) => {
                if self.confirm_delete.as_deref() == Some(&p) {
                    let r = self.store.borrow_mut().index.delete(&p);
                    if let Err(e) = r {
                        self.status = e.to_string();
                    }
                    self.flush_writes();
                    // Out of the note and its actions, back to its folder.
                    self.reset_to(None);
                    let folder = cce_vault::index::parent(&p).to_string();
                    if !folder.is_empty() {
                        self.go(Screen::Files(folder));
                    }
                } else {
                    self.confirm_delete = Some(p);
                    self.refresh_rows(true);
                }
            }
            Target::SyncNow => {
                self.backend.sync_soon();
                self.status = self.backend.status();
                self.refresh_rows(true);
            }
            Target::Disconnect => {
                self.backend.disconnect();
                self.refresh_rows(true);
            }
            Target::None => {}
        }
    }

    fn submit_form(&mut self) {
        match self.screen() {
            Screen::Setup => {
                let root = self.ui[self.root_box].edit_buffer.clone();
                let root = if root.trim().is_empty() { self.ui[self.root_box].text.clone() } else { root };
                let root = root.trim().trim_end_matches('/').to_string();
                if root.is_empty() {
                    return;
                }
                self.store.borrow_mut().root = root.clone();
                self.backend.connect(&root);
                self.status = self.backend.status();
            }
            Screen::NewNote(folder) => {
                let name = self.field_text(self.name_box);
                if let Some(p) = self.create_note(&folder, &name) {
                    self.stack.pop();
                    self.open_path(&p, None);
                    self.mode = Mode::Edit;
                }
            }
            Screen::Rename(from) => {
                let to = self.field_text(self.name_box);
                let to = to.trim().trim_start_matches('/');
                if to.is_empty() {
                    return;
                }
                let to = if FileKind::of(&from) == FileKind::Note && !to.ends_with(".md") { format!("{to}.md") } else { to.to_string() };
                let r = self.store.borrow_mut().index.rename(&from, &to);
                match r {
                    Ok(plan) => {
                        self.flush_writes();
                        self.status = format!("Renamed; {} link(s) updated", plan.edits.len());
                        self.reset_to(None);
                        self.open_path(&plan.to, None);
                    }
                    Err(e) => self.status = e.to_string(),
                }
            }
            _ => {}
        }
    }

    /// A field's text as typed, committed or not.
    fn field_text(&self, h: Handle<Adapted<TextBox>>) -> String {
        let tb = &self.ui[h];
        if tb.editing {
            tb.edit_buffer.clone()
        } else {
            tb.text.clone()
        }
    }

    // ---- layout ---------------------------------------------------------

    fn place_widgets(&mut self) {
        let screen = self.screen();
        let (w, h) = (self.width, self.height);
        let show = |ui: &mut UiContext, id: WidgetId, on: bool| {
            if let Some(wd) = ui.get_widget_mut(id) {
                wd.set_visible(on);
            }
        };
        let bh = 36.0;
        let by = (HEADER_H - bh) / 2.0;
        let has_back = self.stack.len() > 1;
        show(&mut self.ui, self.back.id(), has_back);
        self.ui[self.back].set_rect(8.0, by, 72.0, bh);

        let note = matches!(screen, Screen::Note(_));
        let is_md = self.current_note().is_some_and(|p| FileKind::of(&p) == FileKind::Note);
        show(&mut self.ui, self.edit.id(), note && is_md);
        self.ui[self.edit].set_label(if self.mode == Mode::Edit { "Done" } else { "Edit" });
        let more_on = note || matches!(screen, Screen::Files(_));
        show(&mut self.ui, self.more.id(), more_on);
        self.ui[self.more].set_rect(w - 8.0 - 48.0, by, 48.0, bh);
        self.ui[self.edit].set_rect(w - 8.0 - 48.0 - 8.0 - 72.0, by, 72.0, bh);

        let tb = self.has_toolbar();
        let n = self.tools.len() as f32;
        let tw = (w - 16.0) / n;
        for (i, t) in self.tools.iter().enumerate() {
            show(&mut self.ui, t.id(), tb);
            self.ui[*t].set_rect(8.0 + i as f32 * tw + 3.0, h - TOOLBAR_H + 8.0, tw - 6.0, TOOLBAR_H - 16.0);
        }

        let body = self.body();
        let field_w = (body.width - 2.0 * PAD).min(560.0);
        let fx = body.x + PAD;
        show(&mut self.ui, self.search_box.id(), screen == Screen::Search);
        self.ui[self.search_box].set_rect(fx, body.y + 12.0, body.width - 2.0 * PAD, FORM_FIELD_H);

        let naming = matches!(screen, Screen::NewNote(_) | Screen::Rename(_));
        show(&mut self.ui, self.name_box.id(), naming);
        self.ui[self.name_box].set_rect(fx, body.y + 48.0, field_w, FORM_FIELD_H);

        let setup = screen == Screen::Setup;
        show(&mut self.ui, self.root_box.id(), setup);
        self.ui[self.root_box].set_rect(fx, body.y + 112.0, field_w, FORM_FIELD_H);

        show(&mut self.ui, self.primary.id(), naming || setup);
        let label = match screen {
            Screen::Setup => "Connect Dropbox",
            Screen::NewNote(_) => "Create",
            _ => "Rename",
        };
        self.ui[self.primary].set_label(label);
        let py = if setup { body.y + 112.0 + FORM_FIELD_H + 20.0 } else { body.y + 48.0 + FORM_FIELD_H + 20.0 };
        self.ui[self.primary].set_rect(fx, py, field_w.min(240.0), 44.0);
        self.ui.rebuild_spatial_grid();
    }

    fn title(&self) -> String {
        match self.screen() {
            Screen::Setup => "Vault".into(),
            Screen::Files(f) if f.is_empty() => "Vault".into(),
            Screen::Files(f) => f.rsplit('/').next().unwrap_or(&f).to_string(),
            Screen::Note(p) => title_of(&p),
            Screen::Search => "Search".into(),
            Screen::Tasks => "Tasks".into(),
            Screen::Backlinks(p) => format!("Links to {}", title_of(&p)),
            Screen::Actions(p) => title_of(&p),
            Screen::Vault => "Sync".into(),
            Screen::NewNote(_) => "New note".into(),
            Screen::Rename(p) => format!("Rename {}", title_of(&p)),
        }
    }

    // ---- the note view --------------------------------------------------

    fn read_frame(&self) -> (f32, f32, f32) {
        let b = self.body();
        let width = (b.width - 2.0 * READ_PAD).clamp(80.0, READ_MAX_W);
        let x = b.x + ((b.width - width) / 2.0).max(READ_PAD);
        (x, b.y + READ_PAD, width)
    }

    fn ensure_read_layout(&mut self) {
        let Some(path) = self.current_note() else { return };
        let (_, _, width) = self.read_frame();
        if matches!(&self.read_layout, Some((p, w, _)) if *p == path && (w - width).abs() < 0.5) {
            return;
        }
        let st = self.store.borrow();
        let text = st.index.read_text(&path).unwrap_or_default();
        let blocks = markdown::blocks(&text);
        let m = self.measure.get_or_insert_with(|| ShapingMeasure::new(false));
        let theme = Theme { body_font: "sans-serif".into(), mono_font: "monospace".into(), size: BODY_SIZE };
        let ix = &st.index;
        let resolved = |l: &SpanLink| match l {
            SpanLink::Note { target, .. } | SpanLink::Embed { target, .. } => {
                target.is_empty() || ix.resolve_text(Some(&path), target).is_some()
            }
            _ => true,
        };
        let laid = markdown::layout(&blocks, width, &theme, m, &resolved);
        drop(st);
        self.read_layout = Some((path, width, laid));
    }

    fn read_max_scroll(&self) -> f32 {
        let h = self.read_layout.as_ref().map_or(0.0, |(_, _, l)| l.height);
        (h + 2.0 * READ_PAD - self.body().height).max(0.0)
    }

    fn paint_note(&mut self, pc: &mut PaintCtx) {
        let Some(path) = self.current_note() else { return };
        let body = self.body();
        if FileKind::of(&path) != FileKind::Note {
            let held = self.store.borrow().sync.tracked(&path).is_none_or(|t| t.held);
            let msg = match FileKind::of(&path) {
                _ if !held => "Downloading…",
                FileKind::Canvas => "Canvases open in Obsidian.",
                _ => "This file is not a note.",
            };
            pc.text_with(msg, body.x + PAD, body.y + PAD, BODY_SIZE, srgb_u8(cce_ui::color::TEXT_DIM), Some("sans-serif".into()), None);
            return;
        }
        match self.mode {
            Mode::Edit => {
                let st = self.store.clone();
                let st = st.borrow();
                self.editor.prepare(body);
                let resolved = |t: &doc_editor::Target| match t {
                    doc_editor::Target::Note { target, .. } => {
                        target.is_empty() || st.index.resolve_text(Some(&path), target).is_some()
                    }
                    doc_editor::Target::Url(_) => true,
                };
                self.editor.paint_prepared_with(pc, true, &resolved);
            }
            Mode::Read => {
                self.ensure_read_layout();
                if let Some(line) = self.read_pending_line.take() {
                    let y = self.read_layout.as_ref().map_or(0.0, |(_, _, l)| l.y_of_line(line));
                    self.read_scroll = y.clamp(0.0, self.read_max_scroll());
                    self.read_motion.y.jump_to(self.read_scroll);
                }
                let (ox, oy, _) = self.read_frame();
                if let Some((_, _, l)) = &self.read_layout {
                    let scroll = self.read_scroll;
                    pc.clip(body, |pc| l.paint(pc, (ox, oy), scroll, body));
                    if l.draws.is_empty() {
                        pc.text_with("Empty note. Tap Edit to write.", ox, oy, BODY_SIZE, srgb_u8(cce_ui::color::TEXT_DIM), Some("sans-serif".into()), None);
                    }
                }
            }
        }
    }

    fn note_tap(&mut self, x: f32, y: f32) {
        let (ox, oy, _) = self.read_frame();
        let hit = self.read_layout.as_ref().and_then(|(_, _, l)| l.hit(x - ox, y - oy + self.read_scroll).cloned());
        match hit {
            Some(Hit::Link(link)) => self.follow(link),
            Some(Hit::Task { line, status }) => {
                if let Some(p) = self.current_note() {
                    self.toggle_task(&p, line, status != ' ');
                }
            }
            None => {}
        }
    }

    // ---- widgets --------------------------------------------------------

    fn drain_widgets(&mut self) {
        if self.ui[self.back].take_click() {
            self.back();
        }
        if self.ui[self.edit].take_click() {
            if self.mode == Mode::Edit {
                self.save_editor();
                self.mode = Mode::Read;
                self.read_layout = None;
            } else {
                self.mode = Mode::Edit;
            }
            self.needs_layout = true;
        }
        if self.ui[self.more].take_click() {
            match self.screen() {
                Screen::Note(p) => self.go(Screen::Actions(p)),
                _ => self.go(Screen::Vault),
            }
        }
        let clicked: Vec<usize> = (0..self.tools.len()).filter(|&i| self.ui[self.tools[i]].take_click()).collect();
        for i in clicked {
            match i {
                0 => self.reset_to(None),
                1 => {
                    self.reset_to(Some(Screen::Search));
                }
                2 => self.today(),
                3 => self.reset_to(Some(Screen::Tasks)),
                _ => {
                    let folder = match self.screen() {
                        Screen::Files(f) => f,
                        Screen::Note(p) => cce_vault::index::parent(&p).to_string(),
                        _ => String::new(),
                    };
                    self.go(Screen::NewNote(folder));
                }
            }
        }
        if self.ui[self.search_box].take_change() {
            self.refresh_rows(false);
        }
        self.ui[self.name_box].take_change();
        self.ui[self.root_box].take_change();
        if self.ui[self.primary].take_click() {
            self.submit_form();
        }
    }

    fn route(&mut self, ev: &Event, short_circuit: bool) -> bool {
        let mut any = false;
        for id in self.root_ids() {
            if self.ui.propagate_event(ev, id) {
                any = true;
                if short_circuit {
                    break;
                }
            }
        }
        any
    }

    fn any_field_editing(&self) -> bool {
        [self.search_box, self.name_box, self.root_box].iter().any(|h| self.ui[*h].editing)
    }
}

/// The rows of one folder: subfolders first, then files, by name.
fn folder_rows(ix: &cce_vault::Index, folder: &str) -> Vec<Row<Target>> {
    let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
    let mut folders = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for path in ix.files().keys() {
        let Some(rest) = path.strip_prefix(&prefix) else { continue };
        match rest.split_once('/') {
            Some((sub, _)) => {
                folders.insert(sub.to_string());
            }
            None => files.push(path.clone()),
        }
    }
    let mut out: Vec<Row<Target>> =
        folders.into_iter().map(|f| Row::new(f.clone(), Kind::Folder, Target::Folder(format!("{prefix}{f}")))).collect();
    files.sort_by_key(|p| p.to_lowercase());
    for p in files {
        let kind = match FileKind::of(&p) {
            FileKind::Note => Kind::Note,
            _ => Kind::Attachment,
        };
        let name = match kind {
            Kind::Note => title_of(&p),
            _ => p.rsplit('/').next().unwrap_or(&p).to_string(),
        };
        out.push(Row::new(name, kind, Target::Open(p)));
    }
    out
}

fn search_rows(ix: &cce_vault::Index, q: &str) -> Vec<Row<Target>> {
    if q.is_empty() {
        return Vec::new();
    }
    if let Some(tag) = q.strip_prefix('#') {
        return ix
            .tagged(tag)
            .into_iter()
            .map(|p| Row::new(title_of(p), Kind::Note, Target::Open(p.to_string())).with_sub(p))
            .collect();
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let names = ix.find(q, 8);
    if !names.is_empty() {
        out.push(Row::new("Names", Kind::Heading, Target::None));
    }
    for m in names {
        seen.insert(m.path.clone());
        out.push(Row::new(title_of(&m.path), Kind::Note, Target::Open(m.path.clone())).with_sub(m.path.clone()));
    }
    let hits = ix.search(q, 40);
    if !hits.is_empty() {
        out.push(Row::new("Text", Kind::Heading, Target::None));
    }
    for h in hits {
        let line = h.lines.first().map(|l| l.line).unwrap_or(0);
        let snippet = h.lines.first().map(|l| l.text.clone()).unwrap_or_default();
        out.push(Row::new(title_of(&h.path), Kind::Note, Target::OpenAt(h.path.clone(), line)).with_sub(snippet));
    }
    out
}

/// The tasks of the notes under `Tasks/`, grouped by note, open ones
/// first; done ones only when asked.
fn task_rows(ix: &cce_vault::Index, show_done: bool) -> Vec<Row<Target>> {
    let mut out = Vec::new();
    let mut current = String::new();
    for (path, task) in ix.tasks() {
        if !path.starts_with("Tasks/") {
            continue;
        }
        let done = task.status != ' ';
        if done && !show_done {
            continue;
        }
        if path != current {
            current = path.to_string();
            out.push(Row::new(title_of(path), Kind::Heading, Target::None));
        }
        let (text, due) = cce_vault::split_due(&task.text);
        let mut row = Row::new(text, Kind::Task(done), Target::Task { path: path.to_string(), line: task.line, done });
        if let Some(d) = due {
            row = row.with_sub(format!("Due {d}"));
        }
        out.push(row);
    }
    out
}

impl Application for VaultApp {
    type Message = Msg;

    fn create(sender: AppSender<Msg>) -> Self {
        let installer = INSTALL.with(|i| i.borrow_mut().take());
        let (store, backend): (Shared, Box<dyn Backend>) = match installer {
            Some(f) => f(sender),
            None => (
                std::rc::Rc::new(RefCell::new(Store::new(Vec::new(), Default::default(), DEFAULT_ROOT.into()))),
                Box::new(NullBackend),
            ),
        };
        let mut ui = UiContext::new();
        let button = |ui: &mut UiContext, label: &str| ui.insert(Button::new(0.0, 0.0, 0.0, 0.0).with_label(label));
        let back = button(&mut ui, "Back");
        let edit = button(&mut ui, "Edit");
        let more = button(&mut ui, "•••");
        let tools = [
            button(&mut ui, "Files"),
            button(&mut ui, "Search"),
            button(&mut ui, "Today"),
            button(&mut ui, "Tasks"),
            button(&mut ui, "New"),
        ];
        let search_box = ui.insert(
            TextBox::new(String::new()).with_placeholder("Search").with_update_on_type(true).with_font_size(BODY_SIZE),
        );
        let name_box = ui.insert(TextBox::new(String::new()).with_placeholder("Note name").with_font_size(BODY_SIZE));
        let root_root = store.borrow().root.clone();
        let root_box = ui.insert(TextBox::new(root_root).with_placeholder(DEFAULT_ROOT).with_font_size(BODY_SIZE));
        let primary = button(&mut ui, "Create");
        let connected = backend.connected();
        let empty = store.borrow().index.files().is_empty();
        let stack = if !connected && empty { vec![Screen::Setup] } else { vec![Screen::Files(String::new())] };
        let mut app = VaultApp {
            store,
            backend,
            stack,
            rows: Rows::default(),
            ui,
            back,
            edit,
            more,
            tools,
            search_box,
            name_box,
            root_box,
            primary,
            mode: Mode::Read,
            editor: DocEditor::new("", EditorTheme::new(BODY_SIZE), false),
            saved_text: String::new(),
            edit_seen: 0,
            edit_changed_at: Instant::now(),
            read_layout: None,
            read_scroll: 0.0,
            read_motion: ScrollMotion::new(),
            read_pending_line: None,
            measure: None,
            press_at: None,
            editor_pressed: false,
            confirm_delete: None,
            show_done: false,
            width: 390.0,
            height: 800.0,
            needs_layout: true,
            status: String::new(),
        };
        app.editor.pad = 18.0;
        app.editor.max_width = READ_MAX_W;
        app.status = app.backend.status();
        app.enter();
        if connected {
            app.backend.sync_soon();
        }
        app
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Vault".into(),
            app_id: "cce-vault-web".into(),
            width: 430,
            height: 860,
            fullscreen: false,
            min_size: Some((320, 480)),
        }
    }

    fn update(&mut self, msg: Msg, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            Msg::Remote { changed } => {
                if let Some(path) = self.current_note() {
                    if changed.iter().any(|c| c.eq_ignore_ascii_case(&path)) {
                        self.remote_changed_open_note(&path);
                    }
                }
                self.read_layout = None;
                if !matches!(self.screen(), Screen::Note(_)) || self.current_note().is_some_and(|p| !self.store.borrow().index.files().contains_key(&p)) {
                    // The open note was dropped remotely: back to its folder.
                    if let Some(p) = self.current_note() {
                        if !self.store.borrow().index.files().contains_key(&p) {
                            self.status = format!("{} was deleted on another device", title_of(&p));
                            self.stack.pop();
                        }
                    }
                }
                self.refresh_rows(true);
            }
            Msg::Status => {}
            Msg::Connection => {
                if self.backend.connected() && self.screen() == Screen::Setup {
                    self.stack = vec![Screen::Files(String::new())];
                    self.enter();
                }
            }
            Msg::Hidden => self.save_editor(),
            Msg::Shown => self.backend.sync_soon(),
        }
        self.status = self.backend.status();
        if matches!(self.screen(), Screen::Vault) {
            self.refresh_rows(true);
        }
        self.needs_layout = true;
        *needs_rebuild = true;
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.ui.tick(dt) {
            *needs_rebuild = true;
        }
        let view = self.list_rect();
        if self.rows.tick(dt, view) {
            *needs_rebuild = true;
        }
        if self.read_motion.is_animating() {
            let max = self.read_max_scroll();
            self.read_motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
            self.read_scroll = self.read_motion.y.pos();
            *needs_rebuild = true;
        }
        if self.mode == Mode::Edit {
            if self.editor.tick(dt) {
                *needs_rebuild = true;
            }
            let now = Instant::now();
            if self.editor.buf.revision != self.edit_seen {
                self.edit_seen = self.editor.buf.revision;
                self.edit_changed_at = now;
            } else if self.editor.text() != self.saved_text && now.duration_since(self.edit_changed_at) >= AUTOSAVE_AFTER {
                self.save_editor();
                *needs_rebuild = true;
            }
        }
    }

    fn idle_poll_interval(&self) -> Option<Duration> {
        // The autosave deadline is wall-clock time the loop cannot see.
        (self.mode == Mode::Edit && self.editor.text() != self.saved_text).then_some(Duration::from_millis(300))
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        if self.width != size.width || self.height != size.height {
            self.width = size.width;
            self.height = size.height;
            self.needs_layout = true;
            self.read_layout = None;
        }
        cce_ui::scale::set_scale_factor(scale as f32);
        if self.needs_layout {
            self.place_widgets();
            self.needs_layout = false;
        }
        let (w, h) = (self.width, self.height);
        let mut pc = PaintCtx::new();
        pc.root_plate(w, h);
        // The header carved into the plate, its one wall facing the content.
        pc.recess_edges(self.header(), (0.0, 0.0, 0.0, 0.0), cce_ui::layout::bar_wall_width(), (false, false, true, false));
        if self.has_toolbar() {
            let tb = Rect { x: 0.0, y: h - TOOLBAR_H, width: w, height: TOOLBAR_H };
            pc.recess_edges(tb, (0.0, 0.0, 0.0, 0.0), cce_ui::layout::bar_wall_width(), (true, false, false, false));
        }
        let fg = srgb_u8(cce_ui::color::TEXT_FG);
        let dim = srgb_u8(cce_ui::color::TEXT_DIM);
        let left = if self.stack.len() > 1 { 8.0 + 72.0 + 12.0 } else { PAD };
        let right_buttons = match self.screen() {
            Screen::Note(_) => 8.0 + 48.0 + 8.0 + 72.0 + 12.0,
            Screen::Files(_) => 8.0 + 48.0 + 12.0,
            _ => PAD,
        };
        let title_bounds = [left, 0.0, w - right_buttons, HEADER_H];
        let status = self.status.clone();
        let with_status = matches!(self.screen(), Screen::Files(_)) && !status.is_empty();
        let ty = if with_status { 7.0 } else { cce_ui::layout::align_text_y(0.0, HEADER_H, TITLE_SIZE, 0.0) };
        pc.text_with(self.title(), left, ty, TITLE_SIZE, fg, Some("sans-serif".into()), Some(title_bounds));
        if with_status {
            pc.text_with(status, left, 7.0 + TITLE_SIZE * 1.35, SMALL_SIZE - 1.0, dim, Some("sans-serif".into()), Some(title_bounds));
        }

        let body = self.body();
        let f = fonts();
        match self.screen() {
            Screen::Note(_) => self.paint_note(&mut pc),
            Screen::Setup => {
                let lines = [
                    "Your vault, synced with Dropbox.",
                    "",
                    "Dropbox folder that holds the vault:",
                ];
                for (i, l) in lines.iter().enumerate() {
                    pc.text_with(l.to_string(), body.x + PAD, body.y + 20.0 + i as f32 * 24.0, BODY_SIZE, if i == 0 { fg } else { dim }, Some(f.family.clone()), None);
                }
                if !self.status.is_empty() {
                    pc.text_with(self.status.clone(), body.x + PAD, body.y + 112.0 + FORM_FIELD_H + 84.0, SMALL_SIZE, dim, Some(f.family.clone()), Some([body.x, body.y, body.x + body.width - PAD, body.y + body.height]));
                }
            }
            Screen::NewNote(folder) => {
                let label = if folder.is_empty() { "In the vault root".to_string() } else { format!("In {folder}/") };
                pc.text_with(label, body.x + PAD, body.y + 16.0, BODY_SIZE, dim, Some(f.family.clone()), None);
            }
            Screen::Rename(_) => {
                pc.text_with("New name (folders with /); links are updated", body.x + PAD, body.y + 16.0, SMALL_SIZE, dim, Some(f.family.clone()), Some([body.x, body.y, body.x + body.width - PAD, body.y + body.height]));
            }
            Screen::Tasks => {
                let view = self.list_rect();
                self.rows.paint(&mut pc, view, &f);
            }
            _ => {
                let view = self.list_rect();
                self.rows.paint(&mut pc, view, &f);
            }
        }

        for id in self.root_ids() {
            if let Some(wd) = self.ui.get_widget(id) {
                cce_ui::widget::painter::paint_root_into(&self.ui, wd, &mut pc);
            }
        }
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn ui_context(&self) -> Option<&UiContext> {
        Some(&self.ui)
    }

    fn ui_context_mut(&mut self) -> Option<&mut UiContext> {
        Some(&mut self.ui)
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.width = width;
        self.height = height;
        self.needs_layout = true;
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let ev = Event::PointerMove { x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
        if self.route(&ev, false) {
            *needs_rebuild = true;
        }
        if self.editor_pressed && self.editor.drag(pos.x, pos.y) != doc_editor::Response::None {
            *needs_rebuild = true;
        }
        // A press that wandered off its row is not a tap.
        if let (Some((px, py)), Some(_)) = (self.press_at, self.rows.pressed) {
            if (pos.x - px).abs() > 12.0 || (pos.y - py).abs() > 12.0 {
                self.rows.pressed = None;
                *needs_rebuild = true;
            }
        }
        self.drain_widgets();
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Msg> {
        let (x, y) = (pos.x, pos.y);
        let ev = Event::MouseButton { button, state, x, y, local_x: x, local_y: y };
        let before = self.screen();
        let took = self.route(&ev, false);
        self.drain_widgets();
        if std::env::var_os("CCE_VAULT_DEBUG").is_some() {
            eprintln!("[vault] {state:?} ({x:.0},{y:.0}) took={took} {before:?} -> {:?}", self.screen());
        }
        *needs_rebuild = true;
        if button != MouseButton::Left {
            return None;
        }
        let body = self.body();
        let in_widget = took && state == ElementState::Pressed;
        match state {
            ElementState::Pressed if !in_widget => {
                self.press_at = Some((x, y));
                if self.current_note().is_some() && self.mode == Mode::Edit && body.contains(x, y) {
                    self.editor_pressed = true;
                    if let doc_editor::Response::Follow(t) = self.editor.press(x, y, false, false) {
                        self.editor_pressed = false;
                        let link = match t {
                            doc_editor::Target::Note { target, subpath } => SpanLink::Note { target, subpath },
                            doc_editor::Target::Url(u) => SpanLink::Url(u),
                        };
                        self.save_editor();
                        self.follow(link);
                    }
                    self.needs_layout = true;
                } else {
                    self.rows.pressed = self.rows.hit(self.list_rect(), x, y);
                }
            }
            ElementState::Released => {
                if self.editor_pressed {
                    self.editor.release();
                    self.editor_pressed = false;
                }
                let tap = self.press_at.take().is_some_and(|(px, py)| (x - px).abs() < 12.0 && (y - py).abs() < 12.0);
                if let Some(i) = self.rows.pressed.take() {
                    if tap && self.rows.hit(self.list_rect(), x, y) == Some(i) {
                        let target = self.rows.rows[i].target.clone();
                        self.activate(target);
                    }
                } else if tap && self.current_note().is_some() && self.mode == Mode::Read && body.contains(x, y) {
                    self.note_tap(x, y);
                }
            }
            _ => {}
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let ev = Event::MouseWheel { delta: *delta, x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
        if self.route(&ev, true) {
            *needs_rebuild = true;
            return;
        }
        // A scroll is not a tap.
        self.press_at = None;
        if self.current_note().is_some() {
            match self.mode {
                Mode::Edit => {
                    if self.editor.wheel(delta) {
                        *needs_rebuild = true;
                    }
                }
                Mode::Read => {
                    let max = self.read_max_scroll();
                    self.read_motion.reconcile(0.0, self.read_scroll);
                    let line = BODY_SIZE * 1.5;
                    if self.read_motion.apply(delta, (line, line * 3.0), Bounds::max(0.0), Bounds::max(max)) {
                        self.read_scroll = self.read_motion.y.pos();
                        *needs_rebuild = true;
                    }
                }
            }
            return;
        }
        if self.rows.wheel(delta, self.list_rect()) {
            *needs_rebuild = true;
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Msg> {
        *needs_rebuild = true;
        let pressed = event.state == ElementState::Pressed;
        if self.any_field_editing() {
            let enter = pressed && event.logical_key == Key::Named(NamedKey::Enter);
            let ev = Event::KeyInput(event.clone());
            self.route(&ev, true);
            self.drain_widgets();
            if enter && matches!(self.screen(), Screen::Setup | Screen::NewNote(_) | Screen::Rename(_)) {
                self.submit_form();
            }
            return None;
        }
        if self.current_note().is_some() && self.mode == Mode::Edit {
            if pressed && event.logical_key == Key::Named(NamedKey::Escape) {
                self.save_editor();
                self.mode = Mode::Read;
                self.read_layout = None;
                self.needs_layout = true;
                return None;
            }
            // The editor takes presses only: a browser's keyup carries the
            // key's text too, and would type it again.
            if !pressed {
                return None;
            }
            if let doc_editor::Response::Follow(t) = self.editor.key(event) {
                let link = match t {
                    doc_editor::Target::Note { target, subpath } => SpanLink::Note { target, subpath },
                    doc_editor::Target::Url(u) => SpanLink::Url(u),
                };
                self.save_editor();
                self.follow(link);
            }
            return None;
        }
        if pressed && event.logical_key == Key::Named(NamedKey::Escape) {
            self.back();
            return None;
        }
        let ev = Event::KeyInput(event.clone());
        self.route(&ev, true);
        self.drain_widgets();
        None
    }

    fn undo(&mut self, needs_rebuild: &mut bool) -> bool {
        if self.mode == Mode::Edit && self.editor.undo() {
            *needs_rebuild = true;
            return true;
        }
        false
    }

    fn redo(&mut self, needs_rebuild: &mut bool) -> bool {
        if self.mode == Mode::Edit && self.editor.redo() {
            *needs_rebuild = true;
            return true;
        }
        false
    }

    fn on_exit(&mut self) {
        self.save_editor();
    }
}

impl VaultApp {
    /// Sync replaced the note that is open. A clean editor reloads; edits
    /// typed since the last save are kept as a conflict copy first.
    fn remote_changed_open_note(&mut self, path: &str) {
        let mine = self.editor.text();
        if self.mode == Mode::Edit && mine != self.saved_text {
            let now = chrono::Local::now().format("%Y-%m-%d %H%M").to_string();
            let copy = {
                let st = self.store.borrow();
                crate::sync::conflict_name(path, &now, |p| st.index.files().contains_key(p))
            };
            let r = self.store.borrow_mut().index.write_text(&copy, &mine);
            if r.is_ok() {
                self.status = format!("Edited elsewhere too: your text is in {}", title_of(&copy));
            }
            self.flush_writes();
        }
        let text = self.store.borrow().index.read_text(path).unwrap_or_default();
        if text != self.editor.text() {
            self.editor.set_text(&text);
        }
        self.saved_text = text;
        self.edit_seen = self.editor.buf.revision;
    }
}
