//! The phone's side of sync, over cce-sync's shared device rules
//! (`cce_sync::client`, which the laptop daemon runs too). What this adds
//! is the phone's one difference: **attachments are lazy**. An image or a
//! PDF the phone has never opened is held as an empty placeholder, so links
//! to it resolve, and its bytes are fetched when it is opened.
//!
//! The rules never see a placeholder's empty bytes. [`PhoneSync::local`]
//! reports a placeholder as holding the revision it stands for (so it is
//! never "edited here", never pushed), and a download of a lazy file becomes
//! [`Act::Stub`]: record the new base, keep (or put) the placeholder,
//! fetch nothing. Opening the file later fetches its base's blob.
//!
//! The host (the browser half) carries out each [`Act`] only if the file
//! still holds the hash it names, as the laptop does, by asking
//! [`PhoneSync::hash_now`], which reads a placeholder as its base.

use std::collections::BTreeMap;

use cce_sync::client::{Local, Push, Step, SyncState};
use cce_sync::proto::{check_path, hash, Kind, Rev};
use serde::{Deserialize, Serialize};

/// Everything the phone keeps between passes, as JSON in IndexedDB.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneSync {
    pub sync: SyncState,
    /// Attachments held as placeholders, each with the hash of the bytes
    /// it stands for (`None`: not known yet, a placeholder from Dropbox).
    #[serde(default)]
    pub stubs: BTreeMap<String, Option<String>>,
}

/// What the host does to the vault, each conditional on the file still
/// holding `expect` (by [`PhoneSync::hash_now`]; `None`: absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Fetch the blob `hash` and put it at `path`, then call
    /// [`PhoneSync::downloaded`].
    Fetch { path: String, rev: Rev, hash: String, expect: Option<String> },
    /// Put (or keep) an empty placeholder at `path`, then call
    /// [`PhoneSync::stubbed`].
    Stub { path: String, rev: Rev, hash: String, expect: Option<String> },
    Remove { path: String, expect: String },
    /// Copy the bytes at `path` to the new path `to` (a conflict copy).
    KeepCopy { path: String, to: String, expect: String },
}

/// Fetched only when opened: attachments that are not small text.
pub fn lazy(path: &str) -> bool {
    Kind::of(path) == Kind::Attachment && !is_text(path)
}

/// Small text files worth having offline (`.base` views and the like).
fn is_text(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".base", ".txt", ".json", ".css"].iter().any(|e| lower.ends_with(e))
}

impl PhoneSync {
    /// The state a phone that synced with Dropbox starts from: no bases
    /// (the first pass lists and adopts every file whose bytes match), and
    /// every empty lazy file taken for the placeholder it was, so it is
    /// fetched or stubbed rather than pushed as an edit.
    pub fn migrated<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> PhoneSync {
        let stubs = files.into_iter().filter(|(p, b)| b.is_empty() && lazy(p)).map(|(p, _)| (p.to_string(), None)).collect();
        PhoneSync { sync: SyncState::default(), stubs }
    }

    pub fn is_stub(&self, path: &str) -> bool {
        self.stubs.contains_key(path)
    }

    /// The hash the rules see for a file the phone holds: a placeholder
    /// reads as the bytes it stands for, and as absent when unknown. (Not
    /// as its base: a remote deletion drops the base before the removal
    /// is carried out, and the removal must still find the placeholder.)
    fn virtual_hash(&self, path: &str, bytes: &[u8]) -> Option<String> {
        if let Some(h) = self.stubs.get(path) {
            return h.clone();
        }
        Some(hash(bytes))
    }

    /// What the vault holds, as the rules see it. `files` is every file
    /// the phone keeps; hidden ones (old `.obsidian/` settings) don't sync.
    pub fn local<'a>(&self, files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Local {
        let mut out = BTreeMap::new();
        for (path, bytes) in files {
            if check_path(path).is_err() {
                continue;
            }
            if let Some(h) = self.virtual_hash(path, bytes) {
                out.insert(path.to_string(), h);
            }
        }
        out
    }

    /// [`local`](Self::local) for one file, for checking an act's `expect`.
    pub fn hash_now(&self, path: &str, bytes: Option<&[u8]>) -> Option<String> {
        bytes.and_then(|b| self.virtual_hash(path, b))
    }

    /// The rules' steps, as acts for the phone.
    pub fn acts(&self, steps: Vec<Step>) -> Vec<Act> {
        steps
            .into_iter()
            .map(|s| match s {
                Step::Download { path, rev, hash, expect } if lazy(&path) => Act::Stub { path, rev, hash, expect },
                Step::Download { path, rev, hash, expect } => Act::Fetch { path, rev, hash, expect },
                Step::Remove { path, expect } => Act::Remove { path, expect },
                Step::KeepCopy { path, to, expect } => Act::KeepCopy { path, to, expect },
            })
            .collect()
    }

    /// The real bytes of `path` were written.
    pub fn downloaded(&mut self, path: &str, rev: Rev, hash: &str) {
        self.stubs.remove(path);
        self.sync.downloaded(path, rev, hash);
    }

    /// A placeholder was put (or kept) at `path` for this revision.
    pub fn stubbed(&mut self, path: &str, rev: Rev, hash: &str) {
        self.stubs.insert(path.to_string(), Some(hash.to_string()));
        self.sync.downloaded(path, rev, hash);
    }

    /// A file was removed (by sync or by the user).
    pub fn removed(&mut self, path: &str) {
        self.stubs.remove(path);
    }

    /// A placeholder's real bytes, fetched on opening: the blob to fetch.
    pub fn to_fetch(&self, path: &str) -> Option<String> {
        self.stubs.get(path).cloned().flatten()
    }

    /// Edits not yet sent, for the status line.
    pub fn pending(&self, local: &Local) -> usize {
        self.sync.pushes(local).len()
    }

    /// What to send. A placeholder never is (its local hash is its base).
    pub fn pushes(&self, local: &Local) -> Vec<Push> {
        self.sync.pushes(local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cce_sync::block_on_ready as run;
    use cce_sync::core::{ApplyError, Operation, Store, Vault};
    use cce_sync::mem::MemStore;
    use cce_sync::proto::{Pre, Refusal};

    /// The phone, driven the way the browser backend drives it.
    #[derive(Default)]
    struct Phone {
        files: BTreeMap<String, Vec<u8>>,
        state: PhoneSync,
        fetched: Vec<String>,
    }

    impl Phone {
        fn view(&self) -> Vec<(&str, &[u8])> {
            self.files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect()
        }

        fn local(&self) -> Local {
            self.state.local(self.view())
        }

        fn holds(&self, path: &str, expect: &Option<String>) -> bool {
            self.state.hash_now(path, self.files.get(path).map(Vec::as_slice)) == *expect
        }

        fn carry_out(&mut self, v: &Vault<MemStore>, acts: Vec<Act>) {
            for act in acts {
                match act {
                    Act::Fetch { path, rev, hash, expect } => {
                        if self.holds(&path, &expect) {
                            self.files.insert(path.clone(), run(v.store.blob_get(&hash)).unwrap().unwrap());
                            self.fetched.push(path.clone());
                            self.state.downloaded(&path, rev, &hash);
                        }
                    }
                    Act::Stub { path, rev, hash, expect } => {
                        if self.holds(&path, &expect) {
                            self.files.insert(path.clone(), Vec::new());
                            self.state.stubbed(&path, rev, &hash);
                        }
                    }
                    Act::Remove { path, expect } => {
                        if self.holds(&path, &Some(expect)) {
                            self.files.remove(&path);
                            self.state.removed(&path);
                        }
                    }
                    Act::KeepCopy { path, to, expect } => {
                        if self.holds(&path, &Some(expect)) {
                            let b = self.files[&path].clone();
                            self.files.insert(to, b);
                        }
                    }
                }
            }
        }

        fn pass(&mut self, v: &mut Vault<MemStore>) {
            let steps = if self.state.sync.listed {
                let page = v.changes(self.state.sync.seq, 1000).unwrap();
                self.state.sync.changes(&page, &self.local())
            } else {
                let listing = v.listing().unwrap();
                self.state.sync.listing(&listing, &self.local())
            };
            let acts = self.state.acts(steps);
            self.carry_out(v, acts);
            for push in self.state.pushes(&self.local()) {
                match push {
                    Push::Put { path, pre, hash: sent } => {
                        let bytes = self.files[&path].clone();
                        match run(v.apply(vec![Operation::Put { path: path.clone(), pre, bytes }], "phone", 1)) {
                            Ok(c) => {
                                let step = self.state.sync.put_ok(&path, &sent, &c.applied[0]);
                                let acts = self.state.acts(step.into_iter().collect());
                                self.carry_out(v, acts);
                            }
                            Err(ApplyError::Refused(r)) => {
                                let steps = self.state.sync.put_refused(&path, &sent, &r, "2026-10-13 1200", |_| false);
                                let acts = self.state.acts(steps);
                                self.carry_out(v, acts);
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                    Push::Delete { path, rev } => {
                        match run(v.apply(vec![Operation::Delete { path: path.clone(), pre: Pre::Rev(rev) }], "phone", 1)) {
                            Ok(_) | Err(ApplyError::Refused(Refusal::NotFound { .. })) => self.state.sync.delete_ok(&path),
                            Err(ApplyError::Refused(r)) => {
                                let steps = self.state.sync.delete_refused(&path, &r);
                                let acts = self.state.acts(steps);
                                self.carry_out(v, acts);
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                }
            }
        }
    }

    fn server(files: &[(&str, &[u8])]) -> Vault<MemStore> {
        let mut v = Vault::new(MemStore::default());
        let ops = files.iter().map(|(p, b)| Operation::Put { path: p.to_string(), pre: Pre::Absent, bytes: b.to_vec() }).collect();
        run(v.apply(ops, "laptop", 1)).unwrap();
        v
    }

    #[test]
    fn attachments_are_placeholders_until_opened() {
        let mut v = server(&[("Note.md", b"![[pic.png]]\n"), ("pic.png", b"PNGBYTES"), ("view.base", b"filters: []\n")]);
        let mut phone = Phone::default();
        phone.pass(&mut v);
        assert_eq!(phone.files["Note.md"], b"![[pic.png]]\n");
        assert_eq!(phone.files["pic.png"], b"", "a placeholder");
        assert_eq!(phone.files["view.base"], b"filters: []\n", "small text is fetched");
        assert_eq!(phone.fetched, vec!["Note.md", "view.base"]);
        assert!(phone.state.pushes(&phone.local()).is_empty(), "a placeholder is never an edit");
        // Opening it fetches its base's blob.
        let h = phone.state.to_fetch("pic.png").unwrap();
        assert_eq!(run(v.store.blob_get(&h)).unwrap().unwrap(), b"PNGBYTES");
        phone.files.insert("pic.png".into(), b"PNGBYTES".to_vec());
        let rev = phone.state.sync.files["pic.png"].rev;
        phone.state.downloaded("pic.png", rev, &h);
        assert!(!phone.state.is_stub("pic.png"));
        phone.pass(&mut v);
        assert!(phone.state.pushes(&phone.local()).is_empty());
    }

    #[test]
    fn a_placeholder_follows_remote_changes_without_fetching() {
        let mut v = server(&[("pic.png", b"one")]);
        let mut phone = Phone::default();
        phone.pass(&mut v);
        run(v.apply(vec![Operation::Put { path: "pic.png".into(), pre: Pre::Rev(1), bytes: b"two".to_vec() }], "laptop", 2)).unwrap();
        phone.pass(&mut v);
        assert_eq!(phone.state.sync.files["pic.png"].hash, hash(b"two"));
        assert!(phone.fetched.is_empty());
        run(v.apply(vec![Operation::Delete { path: "pic.png".into(), pre: Pre::Rev(2) }], "laptop", 3)).unwrap();
        phone.pass(&mut v);
        assert!(!phone.files.contains_key("pic.png"), "a removed placeholder goes");
        assert!(!phone.state.is_stub("pic.png"));
    }

    #[test]
    fn edits_on_the_phone_go_up_and_conflicts_keep_a_copy() {
        let mut v = server(&[("Daily/D.md", b"# Day\n")]);
        let mut phone = Phone::default();
        phone.pass(&mut v);
        phone.files.insert("Daily/D.md".into(), b"# Day\n- phone\n".to_vec());
        phone.files.insert("New.md".into(), b"new\n".to_vec());
        phone.pass(&mut v);
        assert_eq!(run(v.read("New.md")).unwrap().unwrap().1, b"new\n");
        assert_eq!(run(v.read("Daily/D.md")).unwrap().unwrap().1, b"# Day\n- phone\n");
        // The laptop and the phone append at once.
        run(v.apply(vec![Operation::Put { path: "Daily/D.md".into(), pre: Pre::Rev(2), bytes: b"# Day\n- phone\n- laptop\n".to_vec() }], "laptop", 2)).unwrap();
        phone.files.insert("Daily/D.md".into(), b"# Day\n- phone\n- on the bus\n".to_vec());
        phone.pass(&mut v);
        assert_eq!(phone.files["Daily/D.md"], b"# Day\n- phone\n- laptop\n");
        assert_eq!(phone.files["Daily/D (conflict 2026-10-13 1200).md"], b"# Day\n- phone\n- on the bus\n");
        phone.pass(&mut v);
        assert!(run(v.read("Daily/D (conflict 2026-10-13 1200).md")).unwrap().is_some(), "the copy went up");
    }

    #[test]
    fn a_phone_that_synced_with_dropbox_migrates_without_pushing_placeholders() {
        let mut v = server(&[("Note.md", b"same\n"), ("pic.png", b"PNG"), ("Edited.md", b"server\n")]);
        // What the phone held after Dropbox: matching text, an empty
        // placeholder, a stale .obsidian file, and an edit never sent.
        let mut phone = Phone::default();
        phone.files.insert("Note.md".into(), b"same\n".to_vec());
        phone.files.insert("pic.png".into(), Vec::new());
        phone.files.insert(".obsidian/app.json".into(), b"{}".to_vec());
        phone.files.insert("Edited.md".into(), b"phone\n".to_vec());
        phone.state = PhoneSync::migrated(phone.view());
        assert!(phone.state.is_stub("pic.png"));
        phone.pass(&mut v);
        assert_eq!(phone.files["pic.png"], b"", "still a placeholder, now tracked");
        assert_eq!(phone.state.sync.files["pic.png"].hash, hash(b"PNG"));
        assert_eq!(run(v.read("pic.png")).unwrap().unwrap().1, b"PNG", "the placeholder was not pushed");
        assert!(run(v.read(".obsidian/app.json")).unwrap().is_none(), "hidden files stay off the server");
        assert_eq!(phone.fetched, vec!["Edited.md"], "matching text is adopted, not fetched");
        // The unsent edit is kept as a conflict copy, not lost.
        assert_eq!(phone.files["Edited.md"], b"server\n");
        assert_eq!(phone.files["Edited (conflict 2026-10-13 1200).md"], b"phone\n");
    }
}
