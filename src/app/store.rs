//! What the app and its platform share: the vault (an in-memory
//! `cce_vault::Index`), the sync state, and the [`Backend`] that stores
//! files and talks to the vault's server. One thread owns all of it (a
//! page has one), so it is an `Rc<RefCell<…>>`; nobody holds the borrow
//! across an await.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cce_sync::client::Local;
use cce_sync::proto::hash;
use cce_vault::index::Written;
use cce_vault::Index;

use crate::sync::PhoneSync;

pub struct Store {
    pub index: Index,
    pub sync: PhoneSync,
    /// The server the vault syncs with, for the Vault screen.
    pub server: String,
    /// Each file's hash, so a pass hashes only what changed.
    hashes: HashMap<String, String>,
    /// The agenda lines of each day asked for (`YYYY-MM-DD`), from the
    /// server's Google bridge, for drawing ```` ```agenda ```` blocks.
    pub agendas: HashMap<String, Vec<String>>,
    /// A note to open when the app starts (a link from a Google task).
    pub open_on_start: Option<String>,
}

pub type Shared = Rc<RefCell<Store>>;

impl Store {
    pub fn new(files: Vec<(String, Vec<u8>)>, sync: PhoneSync, server: String) -> Store {
        Store { index: Index::in_memory(files), sync, server, hashes: HashMap::new(), agendas: HashMap::new(), open_on_start: None }
    }

    /// Hand the index's journal to sync and return it as writes to persist:
    /// `(path, Some(bytes))` for a put, `(path, None)` for a removal. Run
    /// after every vault write call. The next pass finds the edits by hash.
    pub fn take_writes(&mut self) -> Vec<(String, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        for w in self.index.take_written() {
            match w {
                Written::Put(path) => {
                    self.hashes.remove(&path);
                    let bytes = self.index.read_bytes(&path).unwrap_or_default();
                    out.push((path, Some(bytes)));
                }
                Written::Removed(path) => {
                    self.hashes.remove(&path);
                    self.sync.removed(&path);
                    out.push((path, None));
                }
            }
        }
        out
    }

    /// Put bytes from sync (not journalled: the backend persists them).
    pub fn put(&mut self, path: &str, bytes: Vec<u8>) {
        self.hashes.remove(path);
        self.index.put_files([(path.to_string(), bytes)]);
    }

    /// Remove a file for sync.
    pub fn remove(&mut self, path: &str) {
        self.hashes.remove(path);
        self.index.remove_files([path]);
    }

    pub fn exists(&self, path: &str) -> bool {
        self.index.read_bytes(path).is_ok()
    }

    fn real_hash(&mut self, path: &str) -> Option<String> {
        if let Some(h) = self.hashes.get(path) {
            return Some(h.clone());
        }
        let h = hash(&self.index.read_bytes(path).ok()?);
        self.hashes.insert(path.to_string(), h.clone());
        Some(h)
    }

    /// The hash sync sees at `path` now (a placeholder reads as its base).
    pub fn hash_now(&mut self, path: &str) -> Option<String> {
        if self.sync.is_stub(path) {
            return self.exists(path).then(|| self.sync.hash_now(path, Some(&[]))).flatten();
        }
        self.real_hash(path)
    }

    /// Whether `path` holds `expect` (`None`: absent), as sync sees it.
    pub fn holds(&mut self, path: &str, expect: Option<&str>) -> bool {
        self.hash_now(path).as_deref() == expect
    }

    /// What the vault holds, as sync sees it: every syncable path (hidden
    /// ones, like an old `.obsidian/`, stay on the device).
    pub fn local(&mut self) -> Local {
        let paths: Vec<String> = self.index.memory_paths().into_iter().map(String::from).collect();
        let mut out = Local::new();
        for path in paths {
            if cce_sync::proto::check_path(&path).is_err() {
                continue;
            }
            if let Some(h) = self.hash_now(&path) {
                out.insert(path, h);
            }
        }
        out
    }

    /// Edits not yet sent, for the status line.
    pub fn pending(&mut self) -> usize {
        let local = self.local();
        self.sync.pending(&local)
    }
}

/// The platform half: where files are kept between runs, and sync.
pub trait Backend {
    /// Keep these writes (a put's bytes, or a removal).
    fn persist(&mut self, writes: Vec<(String, Option<Vec<u8>>)>);
    /// Run a sync pass soon. Local writes call this; so does the app on
    /// start and when it comes back to the foreground.
    fn sync_soon(&mut self);
    /// Whether the device is signed in to the server.
    fn connected(&self) -> bool;
    /// Start signing in (a redirect to the server's sign-in page).
    fn connect(&mut self);
    /// Sign out (the vault stays on the device).
    fn disconnect(&mut self);
    /// Fetch an attachment's bytes (it has a placeholder until then).
    fn fetch_attachment(&mut self, path: &str);
    /// Fetch a day's agenda (`YYYY-MM-DD`) into `Store::agendas`, then
    /// send [`Msg::Agenda`]. Without a server, nothing comes.
    fn fetch_agenda(&mut self, _date: &str) {}
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
    fn connect(&mut self) {}
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
    /// A day's agenda arrived.
    Agenda,
    /// The login came or went.
    Connection,
    /// The page went to the background: save now.
    Hidden,
    /// Back in the foreground.
    Shown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_cache_follows_every_write() {
        let mut st = Store::new(vec![("a.md".into(), b"one\n".to_vec()), (".obsidian/app.json".into(), b"{}".to_vec())], PhoneSync::default(), String::new());
        assert_eq!(st.local().into_iter().collect::<Vec<_>>(), vec![("a.md".to_string(), hash(b"one\n"))], "hidden files stay local");
        st.index.write_text("a.md", "two\n").unwrap();
        st.take_writes();
        assert_eq!(st.local()["a.md"], hash(b"two\n"));
        st.put("a.md", b"three\n".to_vec());
        assert_eq!(st.hash_now("a.md"), Some(hash(b"three\n")));
        st.remove("a.md");
        assert!(st.local().is_empty());
        assert!(st.holds("a.md", None));
    }

    #[test]
    fn a_placeholder_reads_as_its_base() {
        let mut st = Store::new(vec![("pic.png".into(), Vec::new())], PhoneSync::migrated([("pic.png", &b""[..])]), String::new());
        assert!(st.local().is_empty(), "no base yet: as if absent, so the listing fills it in");
        st.sync.stubbed("pic.png", 3, "h");
        assert_eq!(st.local()["pic.png"], "h");
        assert!(st.holds("pic.png", Some("h")));
        st.remove("pic.png");
        assert!(st.holds("pic.png", None), "a removed placeholder is absent");
    }
}
