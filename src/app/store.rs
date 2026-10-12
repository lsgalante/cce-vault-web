//! What the app and its platform share: the vault (an in-memory
//! `cce_vault::Index`), the sync state, and the [`Backend`] that stores
//! files and talks to Dropbox. One thread owns all of it (a page has one),
//! so it is an `Rc<RefCell<…>>`; nobody holds the borrow across an await.

use std::cell::RefCell;
use std::rc::Rc;

use cce_vault::index::Written;
use cce_vault::Index;

use crate::sync::SyncState;

pub struct Store {
    pub index: Index,
    pub sync: SyncState,
    /// The Dropbox folder the vault lives in.
    pub root: String,
}

pub type Shared = Rc<RefCell<Store>>;

impl Store {
    pub fn new(files: Vec<(String, Vec<u8>)>, sync: SyncState, root: String) -> Store {
        Store { index: Index::in_memory(files), sync, root }
    }

    /// Hand the index's journal to sync and return it as writes to persist:
    /// `(path, Some(bytes))` for a put, `(path, None)` for a removal. Run
    /// after every vault write call.
    pub fn take_writes(&mut self) -> Vec<(String, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        for w in self.index.take_written() {
            match w {
                Written::Put(path) => {
                    let bytes = self.index.read_bytes(&path).unwrap_or_default();
                    self.sync.local_put(&path, &crate::dropbox::content_hash(&bytes));
                    out.push((path, Some(bytes)));
                }
                Written::Removed(path) => {
                    self.sync.local_removed(&path);
                    out.push((path, None));
                }
            }
        }
        out
    }

    /// The content hash of what the vault holds at `path`.
    pub fn local_hash(&self, path: &str) -> Option<String> {
        self.index.read_bytes(path).ok().map(|b| crate::dropbox::content_hash(&b))
    }
}

/// The platform half: where files are kept between runs, and sync.
pub trait Backend {
    /// Keep these writes (a put's bytes, or a removal).
    fn persist(&mut self, writes: Vec<(String, Option<Vec<u8>>)>);
    /// Run a sync pass soon. Local writes call this; so does the app on
    /// start and when it comes back to the foreground.
    fn sync_soon(&mut self);
    /// Whether a Dropbox login is held.
    fn connected(&self) -> bool;
    /// Start the Dropbox login for the vault at `root`.
    fn connect(&mut self, root: &str);
    /// Forget the login (the vault stays on the device).
    fn disconnect(&mut self);
    /// Fetch an attachment's bytes (it has a placeholder until then).
    fn fetch_attachment(&mut self, path: &str);
    /// One line for the status bar: last sync, pending edits, errors.
    fn status(&self) -> String;
}

/// A backend that keeps nothing and syncs nothing (tests, a vault opened
/// read-only).
pub struct NullBackend;

impl Backend for NullBackend {
    fn persist(&mut self, _: Vec<(String, Option<Vec<u8>>)>) {}
    fn sync_soon(&mut self) {}
    fn connected(&self) -> bool {
        true
    }
    fn connect(&mut self, _: &str) {}
    fn disconnect(&mut self) {}
    fn fetch_attachment(&mut self, _: &str) {}
    fn status(&self) -> String {
        String::new()
    }
}

/// Messages from the backend (and its async work) to the app.
#[derive(Debug, Clone)]
pub enum Msg {
    /// Sync changed these vault paths (downloaded or dropped).
    Remote { changed: Vec<String> },
    /// The status line changed.
    Status,
    /// The login came or went.
    Connection,
    /// The page went to the background: save now.
    Hidden,
    /// Back in the foreground.
    Shown,
}
