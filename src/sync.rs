//! Two-way sync between the vault this device holds and the Dropbox
//! folder, as a state machine with no I/O. The host (the browser half)
//! lists, downloads, uploads and deletes; this module decides which, and
//! keeps what it needs to decide next time in [`SyncState`], which the
//! host persists between runs.
//!
//! What a pass looks like, in host terms:
//!
//! 1. Feed local edits in as they happen: [`SyncState::local_put`] and
//!    [`SyncState::local_removed`] (from `Index::take_written`).
//! 2. List the remote (`list_folder`, or `continue` with
//!    [`SyncState::cursor`]) and hand the entries to
//!    [`SyncState::listing`], which answers with [`Step`]s: files to
//!    download, placeholders to put, local files to drop.
//! 3. Download each [`Step::Fetch`] and report it with
//!    [`SyncState::fetched`], which says whether to take the remote bytes
//!    and whether the local ones must be kept as a conflict copy first.
//! 4. Push [`SyncState::uploads`] and [`SyncState::deletes`], reporting
//!    each result.
//!
//! The rules, each chosen so no edit is ever lost silently:
//!
//! - Every file is tracked by the remote revision and content hash this
//!   device last synced (its base). A remote file whose hash equals the
//!   base or the local bytes is never downloaded.
//! - Uploads send the base revision, so a remote change in between makes
//!   Dropbox refuse the upload instead of overwriting it; the host then
//!   fetches the remote version.
//! - When both sides changed, the remote version takes the path and the
//!   local one is kept beside it as `Name (conflict YYYY-MM-DD HHMM).md`,
//!   which then uploads like any new note. Nothing is merged automatically.
//! - A remote deletion drops the local file only if it has no local edits;
//!   an edited one is re-uploaded as new. A local deletion is sent with the
//!   base revision, and a remote edit since wins (the file comes back).
//! - Notes, canvases and the few `.obsidian` settings this app reads are
//!   fetched eagerly; attachments only when opened (they get an empty
//!   placeholder so links still resolve). Every other hidden file is
//!   ignored, and nothing hidden is ever written.

use std::collections::BTreeMap;

use cce_vault::FileKind;
use serde::{Deserialize, Serialize};

/// A file as Dropbox reports it, by vault path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub path: String,
    pub rev: String,
    pub hash: String,
    pub size: u64,
}

/// One listing entry (folders are left out by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteChange {
    File(RemoteFile),
    Deleted(String),
}

/// What the sync knows about one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    /// The vault path, cased as last seen.
    pub path: String,
    /// The remote revision of the base; `None` until first uploaded.
    pub rev: Option<String>,
    /// The content hash of the base.
    pub hash: Option<String>,
    pub size: u64,
    /// The local bytes differ from the base (or the file is new here).
    pub dirty: bool,
    /// Deleted here; the remote delete is not done yet.
    pub deleted: bool,
    /// The real bytes are held here (false: an unfetched attachment's
    /// placeholder).
    pub held: bool,
}

/// How eagerly a remote file is mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Eager,
    Lazy,
    Ignore,
}

/// The `.obsidian` settings the vault code reads; every other hidden file
/// is left alone.
const SETTINGS: &[&str] = &[".obsidian/daily-notes.json", ".obsidian/app.json", ".obsidian/templates.json"];

pub fn want(path: &str) -> Want {
    if path.split('/').any(|c| c.starts_with('.')) {
        return if SETTINGS.iter().any(|s| s.eq_ignore_ascii_case(path)) { Want::Eager } else { Want::Ignore };
    }
    match FileKind::of(path) {
        FileKind::Note | FileKind::Canvas => Want::Eager,
        FileKind::Attachment if is_text(path) => Want::Eager,
        FileKind::Attachment => Want::Lazy,
    }
}

/// Small text files worth having offline (Obsidian's `.base` views).
fn is_text(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".base", ".txt", ".json", ".css"].iter().any(|e| lower.ends_with(e))
}

/// What the host does after a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Download this path, then call [`SyncState::fetched`].
    Fetch { path: String },
    /// Put an empty placeholder at this path (an attachment not fetched),
    /// replacing any stale bytes.
    Stub { path: String },
    /// Remove this path from the local vault.
    Drop { path: String },
}

/// What [`SyncState::fetched`] decided about downloaded bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// Put the remote bytes at the path.
    Take,
    /// Both sides changed: first write the local bytes to `copy_to` (a new
    /// local file, already tracked as dirty), then put the remote bytes at
    /// the path.
    TakeKeepingLocal { copy_to: String },
    /// The local bytes already equal the remote ones; write nothing.
    Same,
}

/// An upload to make: an update of `rev`, or a plain add when `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub path: String,
    pub rev: Option<String>,
}

/// A remote delete to make, guarded by the base revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delete {
    pub path: String,
    pub rev: String,
}

/// Everything sync keeps between runs; the host stores it as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// The `list_folder/continue` cursor; `None` lists from scratch.
    pub cursor: Option<String>,
    /// By lowercased vault path: Dropbox paths are case-insensitive.
    files: BTreeMap<String, Tracked>,
}

fn key(path: &str) -> String {
    path.to_lowercase()
}

impl SyncState {
    pub fn tracked(&self, path: &str) -> Option<&Tracked> {
        self.files.get(&key(path))
    }

    pub fn all(&self) -> impl Iterator<Item = &Tracked> {
        self.files.values()
    }

    /// Edits not yet pushed, for a status line.
    pub fn pending(&self) -> usize {
        self.files.values().filter(|t| (t.dirty && t.held) || (t.deleted && t.rev.is_some())).count()
    }

    /// The vault wrote `path` with bytes hashing to `hash`.
    pub fn local_put(&mut self, path: &str, hash: &str) {
        let t = self.files.entry(key(path)).or_insert_with(|| Tracked {
            path: path.to_string(),
            rev: None,
            hash: None,
            size: 0,
            dirty: false,
            deleted: false,
            held: true,
        });
        t.path = path.to_string();
        t.deleted = false;
        t.held = true;
        t.dirty = t.hash.as_deref() != Some(hash);
    }

    /// The vault removed `path`.
    pub fn local_removed(&mut self, path: &str) {
        let k = key(path);
        let Some(t) = self.files.get_mut(&k) else { return };
        if t.rev.is_some() {
            t.deleted = true;
            t.dirty = false;
        } else {
            // Never reached Dropbox: nothing to delete there.
            self.files.remove(&k);
        }
    }

    /// Take a listing. `complete` means a full listing (no cursor), in
    /// which a tracked file that is missing was deleted remotely.
    /// `local_hash` gives the content hash of a path's local bytes.
    pub fn listing(
        &mut self,
        changes: &[RemoteChange],
        complete: bool,
        local_hash: impl Fn(&str) -> Option<String>,
    ) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for change in changes {
            match change {
                RemoteChange::File(r) => {
                    seen.insert(key(&r.path));
                    self.remote_file(r, &local_hash, &mut steps);
                }
                RemoteChange::Deleted(path) => self.remote_deleted(path, &mut steps),
            }
        }
        if complete {
            let gone: Vec<String> = self
                .files
                .iter()
                .filter(|(k, t)| t.rev.is_some() && !seen.contains(*k))
                .map(|(_, t)| t.path.clone())
                .collect();
            for path in gone {
                self.remote_deleted(&path, &mut steps);
            }
        }
        steps
    }

    fn remote_file(&mut self, r: &RemoteFile, local_hash: &impl Fn(&str) -> Option<String>, steps: &mut Vec<Step>) {
        let w = want(&r.path);
        if w == Want::Ignore {
            return;
        }
        let k = key(&r.path);
        if let Some(t) = self.files.get_mut(&k) {
            if t.hash.as_deref() == Some(r.hash.as_str()) {
                // The base, unchanged remotely (perhaps renamed in case).
                t.rev = Some(r.rev.clone());
                t.size = r.size;
                if t.path != r.path && !t.dirty {
                    t.path = r.path.clone();
                }
                return;
            }
            if t.held && local_hash(&t.path).as_deref() == Some(r.hash.as_str()) {
                // Both sides made the same change: adopt it as the base.
                t.rev = Some(r.rev.clone());
                t.hash = Some(r.hash.clone());
                t.size = r.size;
                t.dirty = false;
                t.deleted = false;
                return;
            }
            // Changed remotely. A pending local delete loses to the edit.
            t.deleted = false;
            if w == Want::Lazy && !t.dirty {
                t.rev = Some(r.rev.clone());
                t.hash = Some(r.hash.clone());
                t.size = r.size;
                t.held = false;
                steps.push(Step::Stub { path: t.path.clone() });
                return;
            }
            steps.push(Step::Fetch { path: t.path.clone() });
            return;
        }
        match w {
            Want::Lazy => {
                self.files.insert(
                    k,
                    Tracked {
                        path: r.path.clone(),
                        rev: Some(r.rev.clone()),
                        hash: Some(r.hash.clone()),
                        size: r.size,
                        dirty: false,
                        deleted: false,
                        held: false,
                    },
                );
                steps.push(Step::Stub { path: r.path.clone() });
            }
            _ => steps.push(Step::Fetch { path: r.path.clone() }),
        }
    }

    fn remote_deleted(&mut self, path: &str, steps: &mut Vec<Step>) {
        // A deleted folder is reported once, as its own path.
        let k = key(path);
        let prefix = format!("{k}/");
        let hit: Vec<String> = self.files.keys().filter(|f| **f == k || f.starts_with(&prefix)).cloned().collect();
        for f in hit {
            let t = self.files.get_mut(&f).expect("key from the map");
            if t.dirty {
                // Edited here: keep it, and upload it as new.
                t.rev = None;
                t.hash = None;
                t.deleted = false;
            } else {
                let path = t.path.clone();
                let was_deleted = t.deleted;
                self.files.remove(&f);
                if !was_deleted {
                    steps.push(Step::Drop { path });
                }
            }
        }
    }

    /// A download finished: `file` is the remote metadata that came with
    /// it, `local_hash` the hash of what the vault holds at that path now
    /// (`None` if nothing). `taken` says whether a vault path is in use,
    /// for naming a conflict copy; `now` is local time as
    /// `YYYY-MM-DD HHMM`.
    pub fn fetched(
        &mut self,
        file: &RemoteFile,
        local_hash: Option<&str>,
        now: &str,
        taken: impl Fn(&str) -> bool,
    ) -> Fetched {
        let k = key(&file.path);
        let prior = self.files.get(&k).cloned();
        let base = Tracked {
            path: file.path.clone(),
            rev: Some(file.rev.clone()),
            hash: Some(file.hash.clone()),
            size: file.size,
            dirty: false,
            deleted: false,
            held: true,
        };
        if local_hash == Some(file.hash.as_str()) {
            self.files.insert(k, base);
            return Fetched::Same;
        }
        let conflicted = prior.as_ref().is_some_and(|t| t.dirty && t.held) && local_hash.is_some();
        self.files.insert(k, base);
        if !conflicted {
            return Fetched::Take;
        }
        let copy_to = conflict_name(&file.path, now, |p| taken(p) || self.files.contains_key(&key(p)));
        self.files.insert(
            key(&copy_to),
            Tracked {
                path: copy_to.clone(),
                rev: None,
                hash: None,
                size: 0,
                dirty: true,
                deleted: false,
                held: true,
            },
        );
        Fetched::TakeKeepingLocal { copy_to }
    }

    /// Files to upload, in path order.
    pub fn uploads(&self) -> Vec<Upload> {
        self.files
            .values()
            .filter(|t| t.dirty && t.held && !t.deleted && want(&t.path) != Want::Ignore)
            .map(|t| Upload { path: t.path.clone(), rev: t.rev.clone() })
            .collect()
    }

    /// Remote deletes to make, in path order.
    pub fn deletes(&self) -> Vec<Delete> {
        self.files
            .values()
            .filter_map(|t| Some(Delete { path: t.path.clone(), rev: t.rev.clone().filter(|_| t.deleted)? }))
            .collect()
    }

    /// An upload succeeded with `file`. `local_hash` is what the vault
    /// holds now: an edit made while the upload was in flight stays dirty.
    pub fn uploaded(&mut self, file: &RemoteFile, local_hash: Option<&str>) {
        let t = self.files.entry(key(&file.path)).or_insert_with(|| Tracked {
            path: file.path.clone(),
            rev: None,
            hash: None,
            size: 0,
            dirty: false,
            deleted: false,
            held: true,
        });
        t.rev = Some(file.rev.clone());
        t.hash = Some(file.hash.clone());
        t.size = file.size;
        t.dirty = local_hash != Some(file.hash.as_str());
    }

    /// Dropbox refused an upload as a conflict: the remote changed (or
    /// appeared) since the base. Fetch it; `fetched` then keeps the local
    /// version as a conflict copy.
    pub fn upload_conflicted(&mut self, path: &str) -> Step {
        Step::Fetch { path: path.to_string() }
    }

    /// A remote delete succeeded, or found the file already gone.
    pub fn deleted_remote(&mut self, path: &str) {
        let k = key(path);
        if self.files.get(&k).is_some_and(|t| t.deleted) {
            self.files.remove(&k);
        }
    }

    /// Dropbox refused a delete because the file changed remotely: the
    /// edit wins and the file comes back.
    pub fn delete_conflicted(&mut self, path: &str) -> Step {
        if let Some(t) = self.files.get_mut(&key(path)) {
            t.deleted = false;
            t.dirty = false;
        }
        Step::Fetch { path: path.to_string() }
    }

    /// An attachment's bytes were fetched on demand.
    pub fn attachment_fetched(&mut self, file: &RemoteFile) {
        if let Some(t) = self.files.get_mut(&key(&file.path)) {
            t.rev = Some(file.rev.clone());
            t.hash = Some(file.hash.clone());
            t.size = file.size;
            t.held = true;
        }
    }
}

/// `dir/Name (conflict 2026-10-12 1430).md`, numbered if taken.
pub fn conflict_name(path: &str, now: &str, taken: impl Fn(&str) -> bool) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let base = format!("{dir}{stem} (conflict {now})");
    let mut candidate = format!("{base}{ext}");
    let mut n = 2;
    while taken(&candidate) {
        candidate = format!("{base} {n}{ext}");
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake Dropbox and a fake vault, driven through the engine the way
    /// the browser host drives it, so the tests read as scenarios.
    #[derive(Default)]
    struct World {
        remote: BTreeMap<String, (String, Vec<u8>)>,
        local: BTreeMap<String, Vec<u8>>,
        state: SyncState,
        next_rev: u32,
        /// Paths whose remote state changed since the last pass, for the
        /// cursor listings (`None` = deleted).
        changed: Vec<String>,
    }

    fn h(b: &[u8]) -> String {
        crate::dropbox::content_hash(b)
    }

    impl World {
        fn remote_put(&mut self, path: &str, text: &str) {
            self.next_rev += 1;
            self.remote.insert(path.to_string(), (format!("r{}", self.next_rev), text.as_bytes().to_vec()));
            self.changed.push(path.to_string());
        }
        fn remote_delete(&mut self, path: &str) {
            self.remote.remove(path);
            self.changed.push(path.to_string());
        }
        fn local_edit(&mut self, path: &str, text: &str) {
            self.local.insert(path.to_string(), text.as_bytes().to_vec());
            self.state.local_put(path, &h(text.as_bytes()));
        }
        fn local_delete(&mut self, path: &str) {
            self.local.remove(path);
            self.state.local_removed(path);
        }
        fn meta(&self, path: &str) -> RemoteFile {
            let (rev, bytes) = &self.remote[path];
            RemoteFile { path: path.to_string(), rev: rev.clone(), hash: h(bytes), size: bytes.len() as u64 }
        }
        fn listing(&mut self, complete: bool) -> Vec<RemoteChange> {
            if complete {
                self.changed.clear();
                return self.remote.keys().map(|p| RemoteChange::File(self.meta(p))).collect();
            }
            let changed: Vec<String> = std::mem::take(&mut self.changed);
            changed
                .iter()
                .map(|p| match self.remote.get(p) {
                    Some(_) => RemoteChange::File(self.meta(p)),
                    None => RemoteChange::Deleted(p.clone()),
                })
                .collect()
        }
        fn local_hash(&self, p: &str) -> Option<String> {
            self.local.get(p).map(|b| h(b))
        }

        /// One full pass: list, fetch, push. Returns the conflict copies made.
        fn pass(&mut self, complete: bool) -> Vec<String> {
            let changes = self.listing(complete);
            let local = self.local.clone();
            let steps = self.state.listing(&changes, complete, |p| local.get(p).map(|b| h(b)));
            let mut copies = Vec::new();
            let mut fetches: Vec<String> = Vec::new();
            for step in steps {
                match step {
                    Step::Fetch { path } => fetches.push(path),
                    Step::Stub { path } => {
                        self.local.insert(path, Vec::new());
                    }
                    Step::Drop { path } => {
                        self.local.remove(&path);
                    }
                }
            }
            self.fetch_all(fetches, &mut copies);
            // Push: a conflicted upload turns into a fetch, then pushes again.
            for _ in 0..3 {
                let mut refetch = Vec::new();
                for up in self.state.uploads() {
                    let bytes = self.local[&up.path].clone();
                    let current = self.remote.get(&up.path).map(|(r, _)| r.clone());
                    if current != up.rev {
                        if let Step::Fetch { path } = self.state.upload_conflicted(&up.path) {
                            refetch.push(path);
                        }
                        continue;
                    }
                    self.next_rev += 1;
                    self.remote.insert(up.path.clone(), (format!("r{}", self.next_rev), bytes));
                    let meta = self.meta(&up.path);
                    let now = self.local_hash(&up.path);
                    self.state.uploaded(&meta, now.as_deref());
                }
                for del in self.state.deletes() {
                    match self.remote.get(&del.path) {
                        None => self.state.deleted_remote(&del.path),
                        Some((rev, _)) if *rev == del.rev => {
                            self.remote.remove(&del.path);
                            self.state.deleted_remote(&del.path);
                        }
                        Some(_) => {
                            if let Step::Fetch { path } = self.state.delete_conflicted(&del.path) {
                                refetch.push(path);
                            }
                        }
                    }
                }
                if refetch.is_empty() {
                    break;
                }
                self.fetch_all(refetch, &mut copies);
            }
            self.changed.clear(); // our own pushes are not news
            copies
        }

        fn fetch_all(&mut self, paths: Vec<String>, copies: &mut Vec<String>) {
            for path in paths {
                if !self.remote.contains_key(&path) {
                    continue;
                }
                let meta = self.meta(&path);
                let remote_bytes = self.remote[&path].1.clone();
                let local_hash = self.local_hash(&path);
                let local = self.local.clone();
                match self.state.fetched(&meta, local_hash.as_deref(), "2026-10-12 1430", |p| local.contains_key(p)) {
                    Fetched::Take => {
                        self.local.insert(path, remote_bytes);
                    }
                    Fetched::Same => {}
                    Fetched::TakeKeepingLocal { copy_to } => {
                        let mine = self.local[&path].clone();
                        self.local.insert(copy_to.clone(), mine);
                        self.local.insert(path, remote_bytes);
                        copies.push(copy_to);
                    }
                }
            }
        }

        fn text(&self, map: &BTreeMap<String, Vec<u8>>, p: &str) -> Option<String> {
            map.get(p).map(|b| String::from_utf8(b.clone()).unwrap())
        }
        fn local_text(&self, p: &str) -> Option<String> {
            self.text(&self.local, p)
        }
        fn remote_text(&self, p: &str) -> Option<String> {
            self.remote.get(p).map(|(_, b)| String::from_utf8(b.clone()).unwrap())
        }
        fn in_step(&self) -> bool {
            let remote: HashMap<&String, &Vec<u8>> = self.remote.iter().map(|(p, (_, b))| (p, b)).collect();
            let local: HashMap<&String, &Vec<u8>> =
                self.local.iter().filter(|(p, _)| want(p) == Want::Eager).collect();
            let remote_eager: HashMap<&String, &Vec<u8>> =
                remote.into_iter().filter(|(p, _)| want(p) == Want::Eager).collect();
            local == remote_eager
        }
    }

    #[test]
    fn first_pass_mirrors_the_remote() {
        let mut w = World::default();
        w.remote_put("A.md", "alpha");
        w.remote_put("Tasks/T.md", "- [ ] x");
        w.remote_put("pic.png", "PNG");
        w.remote_put(".obsidian/daily-notes.json", "{}");
        w.remote_put(".obsidian/workspace.json", "{}");
        w.pass(true);
        assert!(w.in_step());
        assert_eq!(w.local_text("pic.png").as_deref(), Some(""), "attachments are placeholders");
        assert!(!w.state.tracked("pic.png").unwrap().held);
        assert!(w.local.contains_key(".obsidian/daily-notes.json"));
        assert!(!w.local.contains_key(".obsidian/workspace.json"), "other hidden files are ignored");
        assert_eq!(w.state.pending(), 0);
        // A second pass downloads nothing and uploads nothing.
        let before = w.next_rev;
        w.pass(true);
        assert_eq!(w.next_rev, before);
    }

    #[test]
    fn edits_flow_both_ways() {
        let mut w = World::default();
        w.remote_put("A.md", "one");
        w.pass(true);
        w.local_edit("A.md", "two");
        w.local_edit("New.md", "fresh");
        assert_eq!(w.state.pending(), 2);
        w.pass(false);
        assert_eq!(w.remote_text("A.md").as_deref(), Some("two"));
        assert_eq!(w.remote_text("New.md").as_deref(), Some("fresh"));
        w.remote_put("A.md", "three");
        w.pass(false);
        assert_eq!(w.local_text("A.md").as_deref(), Some("three"));
        assert!(w.in_step());
    }

    #[test]
    fn both_sides_edited_keeps_a_conflict_copy() {
        let mut w = World::default();
        w.remote_put("notes/A.md", "base");
        w.pass(true);
        w.local_edit("notes/A.md", "mine");
        w.remote_put("notes/A.md", "theirs");
        let copies = w.pass(false);
        assert_eq!(copies, vec!["notes/A (conflict 2026-10-12 1430).md"]);
        assert_eq!(w.local_text("notes/A.md").as_deref(), Some("theirs"));
        assert_eq!(w.remote_text("notes/A (conflict 2026-10-12 1430).md").as_deref(), Some("mine"));
        assert!(w.in_step());
    }

    #[test]
    fn a_conflict_found_at_upload_is_kept_too() {
        // The remote changed after this device listed: the upload is refused.
        let mut w = World::default();
        w.remote_put("A.md", "base");
        w.pass(true);
        w.local_edit("A.md", "mine");
        w.remote_put("A.md", "theirs");
        w.changed.clear(); // the listing does not see it yet
        let copies = w.pass(false);
        assert_eq!(copies.len(), 1);
        assert_eq!(w.local_text("A.md").as_deref(), Some("theirs"));
        assert_eq!(w.remote_text(&copies[0]).as_deref(), Some("mine"));
    }

    #[test]
    fn the_same_edit_on_both_sides_is_not_a_conflict() {
        let mut w = World::default();
        w.remote_put("A.md", "base");
        w.pass(true);
        w.local_edit("A.md", "same");
        w.remote_put("A.md", "same");
        let rev = w.next_rev;
        assert!(w.pass(false).is_empty());
        assert_eq!(w.next_rev, rev, "nothing uploaded");
        assert_eq!(w.state.pending(), 0);
    }

    #[test]
    fn deletions() {
        let mut w = World::default();
        w.remote_put("A.md", "a");
        w.remote_put("B.md", "b");
        w.remote_put("C.md", "c");
        w.pass(true);
        // Remote delete of an untouched file drops it here.
        w.remote_delete("A.md");
        w.pass(false);
        assert!(!w.local.contains_key("A.md"));
        // Remote delete of a file edited here keeps it, re-uploaded as new.
        w.local_edit("B.md", "b edited");
        w.remote_delete("B.md");
        w.pass(false);
        assert_eq!(w.remote_text("B.md").as_deref(), Some("b edited"));
        // Local delete goes remote.
        w.local_delete("C.md");
        w.pass(false);
        assert!(!w.remote.contains_key("C.md"));
        // A local delete loses to a remote edit made since.
        w.remote_put("D.md", "d");
        w.pass(false);
        w.local_delete("D.md");
        w.remote_put("D.md", "d edited");
        w.changed.clear();
        w.pass(false);
        assert_eq!(w.local_text("D.md").as_deref(), Some("d edited"));
        assert!(w.in_step());
        // A file created and deleted before any sync leaves no trace.
        w.local_edit("Tmp.md", "x");
        w.local_delete("Tmp.md");
        assert!(w.state.tracked("Tmp.md").is_none());
    }

    #[test]
    fn a_full_relisting_notices_deletions() {
        let mut w = World::default();
        w.remote_put("A.md", "a");
        w.remote_put("B.md", "b");
        w.pass(true);
        w.remote.remove("A.md"); // deleted while the cursor was lost
        w.state.cursor = None;
        w.pass(true);
        assert!(!w.local.contains_key("A.md"));
        assert!(w.in_step());
    }

    #[test]
    fn a_deleted_folder_takes_its_files() {
        let mut w = World::default();
        w.remote_put("F/a.md", "a");
        w.remote_put("F/b.md", "b");
        w.remote_put("G.md", "g");
        w.pass(true);
        w.local_edit("F/b.md", "kept");
        w.remote.remove("F/a.md");
        w.remote.remove("F/b.md");
        w.changed = vec!["F".into()];
        let changes = vec![RemoteChange::Deleted("F".into())];
        let local = w.local.clone();
        let steps = w.state.listing(&changes, false, |p| local.get(p).map(|b| h(b)));
        assert_eq!(steps, vec![Step::Drop { path: "F/a.md".into() }]);
        assert_eq!(w.state.uploads(), vec![Upload { path: "F/b.md".into(), rev: None }]);
    }

    #[test]
    fn an_edit_during_upload_stays_pending() {
        let mut w = World::default();
        w.local_edit("A.md", "v1");
        let up = w.state.uploads().pop().unwrap();
        w.next_rev += 1;
        w.remote.insert(up.path.clone(), ("r1".into(), b"v1".to_vec()));
        w.local_edit("A.md", "v2"); // typed while the upload ran
        let meta = w.meta("A.md");
        let now = w.local_hash("A.md");
        w.state.uploaded(&meta, now.as_deref());
        assert_eq!(w.state.uploads(), vec![Upload { path: "A.md".into(), rev: Some("r1".into()) }]);
    }

    #[test]
    fn case_only_differences_are_one_file() {
        let mut w = World::default();
        w.remote_put("Note.md", "x");
        w.pass(true);
        let changes = vec![RemoteChange::File(RemoteFile {
            path: "note.md".into(),
            rev: "r9".into(),
            hash: h(b"x"),
            size: 1,
        })];
        let steps = w.state.listing(&changes, false, |_| None);
        assert!(steps.is_empty());
        assert_eq!(w.state.tracked("NOTE.md").unwrap().path, "note.md");
    }

    #[test]
    fn state_round_trips_as_json() {
        let mut w = World::default();
        w.remote_put("A.md", "a");
        w.pass(true);
        w.local_edit("B.md", "b");
        w.state.cursor = Some("c".into());
        let json = serde_json::to_string(&w.state).unwrap();
        assert_eq!(serde_json::from_str::<SyncState>(&json).unwrap(), w.state);
    }

    #[test]
    fn conflict_names() {
        assert_eq!(conflict_name("a/B.md", "2026-10-12 1430", |_| false), "a/B (conflict 2026-10-12 1430).md");
        assert_eq!(
            conflict_name("B.md", "2026-10-12 1430", |p| p == "B (conflict 2026-10-12 1430).md"),
            "B (conflict 2026-10-12 1430) 2.md"
        );
        assert_eq!(conflict_name(".hidden", "x", |_| false), ".hidden (conflict x)");
    }

    #[test]
    fn wants() {
        assert_eq!(want("A.md"), Want::Eager);
        assert_eq!(want("Board.canvas"), Want::Eager);
        assert_eq!(want("Untitled.base"), Want::Eager);
        assert_eq!(want("img/x.JPG"), Want::Lazy);
        assert_eq!(want(".obsidian/daily-notes.json"), Want::Eager);
        assert_eq!(want(".obsidian/plugins/x/main.js"), Want::Ignore);
        assert_eq!(want(".trash/A.md"), Want::Ignore);
    }
}
