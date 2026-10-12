//! The browser's [`Backend`]: files in IndexedDB, sync with Dropbox over
//! `fetch`, the login through OAuth with PKCE (a redirect to Dropbox and
//! back, no secret). A sync pass runs one at a time, a moment after the
//! last local write, on demand, and once a minute while the page shows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use cce_ui::engine::AppSender;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use super::idb::Db;
use super::net::{self, js_err};
use crate::app::store::{Backend, Msg, Shared};
use crate::dropbox::{self, ListFolder, Refusal, Tokens};
use crate::sync::{Fetched, RemoteFile, Step};

/// After the last local write, wait this long for more before syncing.
const DEBOUNCE_MS: i32 = 2500;
/// And sync this often regardless, while the page shows.
const PERIOD_MS: i32 = 60_000;

/// What the page was configured with (`config.json`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Config {
    /// The Dropbox app's key (public under PKCE).
    #[serde(default)]
    pub app_key: String,
    /// Dropbox's hosts, or a mock's.
    #[serde(default)]
    pub endpoints: dropbox::Endpoints,
}

pub struct Inner {
    pub db: Db,
    pub store: Shared,
    pub sender: AppSender<Msg>,
    pub config: Config,
    pub tokens: RefCell<Option<Tokens>>,
    status: RefCell<String>,
    running: Cell<bool>,
    again: Cell<bool>,
    timer: Cell<Option<i32>>,
    timer_cb: RefCell<Option<Closure<dyn FnMut()>>>,
    last_sync: RefCell<Option<String>>,
}

pub struct WebBackend(pub Rc<Inner>);

fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn local_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H%M").to_string()
}

impl WebBackend {
    pub fn new(db: Db, store: Shared, sender: AppSender<Msg>, config: Config, tokens: Option<Tokens>) -> WebBackend {
        let inner = Rc::new(Inner {
            db,
            store,
            sender,
            config,
            tokens: RefCell::new(tokens),
            status: RefCell::new(String::new()),
            running: Cell::new(false),
            again: Cell::new(false),
            timer: Cell::new(None),
            timer_cb: RefCell::new(None),
            last_sync: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        *inner.timer_cb.borrow_mut() = Some(Closure::new(move || {
            if let Some(i) = weak.upgrade() {
                i.timer.set(None);
                Inner::run(i);
            }
        }));
        // The periodic pass.
        let weak = Rc::downgrade(&inner);
        let tick = Closure::<dyn FnMut()>::new(move || {
            if let Some(i) = weak.upgrade() {
                let shows = web_sys::window().and_then(|w| w.document()).is_some_and(|d| !d.hidden());
                if shows {
                    Inner::run(i);
                }
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), PERIOD_MS);
        }
        tick.forget();
        WebBackend(inner)
    }
}

impl WebBackend {
    /// The status line, for a message set before the app's first frame.
    pub fn status_cell(&self) -> std::cell::RefMut<'_, String> {
        self.0.status.borrow_mut()
    }
}

impl Inner {
    fn set_status(&self, s: impl Into<String>) {
        *self.status.borrow_mut() = s.into();
        let _ = self.sender.send(Msg::Status);
    }

    fn schedule(&self, ms: i32) {
        let Some(w) = web_sys::window() else { return };
        if let Some(t) = self.timer.take() {
            w.clear_timeout_with_handle(t);
        }
        if let Some(cb) = self.timer_cb.borrow().as_ref() {
            self.timer.set(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms).ok());
        }
    }

    /// Start a pass now, or after the one running.
    fn run(this: Rc<Inner>) {
        if this.tokens.borrow().is_none() {
            return;
        }
        if this.running.get() {
            this.again.set(true);
            return;
        }
        this.running.set(true);
        spawn_local(async move {
            loop {
                this.again.set(false);
                this.set_status("Syncing…");
                match this.pass().await {
                    Ok(()) => {
                        let at = chrono::Local::now().format("%H:%M").to_string();
                        *this.last_sync.borrow_mut() = Some(at);
                        let line = this.summary();
                        this.set_status(line);
                    }
                    Err(e) => this.set_status(format!("Sync failed: {e}")),
                }
                if !this.again.get() {
                    break;
                }
            }
            this.running.set(false);
        });
    }

    fn summary(&self) -> String {
        let pending = self.store.borrow().sync.pending();
        let at = self.last_sync.borrow().clone();
        match (at, pending) {
            (Some(at), 0) => format!("Synced {at}"),
            (Some(at), n) => format!("Synced {at} · {n} to send"),
            (None, 0) => "Not synced yet".into(),
            (None, n) => format!("{n} to send"),
        }
    }

    /// A fresh access token, refreshing a stale one.
    async fn token(&self) -> Result<String, String> {
        let current = self.tokens.borrow().clone().ok_or("not connected")?;
        if current.fresh(now_ms()) {
            return Ok(current.access_token);
        }
        let reply = net::token(&dropbox::refresh_grant(&self.config.app_key, &current.refresh_token)).await?;
        if !reply.ok() {
            if reply.status == 400 || reply.status == 401 {
                // The refresh token was revoked: log in again.
                *self.tokens.borrow_mut() = None;
                let _ = self.db.delete_meta("tokens").await;
                let _ = self.sender.send(Msg::Connection);
            }
            return Err(format!("login refresh failed ({})", reply.status));
        }
        let parsed: dropbox::TokenReply = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
        let fresh = Tokens::from_reply(parsed, now_ms(), Some(&current)).ok_or("no token")?;
        let token = fresh.access_token.clone();
        let _ = self.db.set_meta("tokens", &serde_json::to_string(&fresh).unwrap_or_default()).await;
        *self.tokens.borrow_mut() = Some(fresh);
        Ok(token)
    }

    fn root(&self) -> String {
        self.store.borrow().root.clone()
    }

    async fn save_state(&self) {
        let json = serde_json::to_string(&self.store.borrow().sync).unwrap_or_default();
        if let Err(e) = self.db.set_meta("sync", &json).await {
            log::warn!("saving sync state: {}", js_err(e));
        }
    }

    async fn persist(&self, path: &str, bytes: Option<&[u8]>) {
        let r = match bytes {
            Some(b) => self.db.put_file(path, b).await,
            None => self.db.delete_file(path).await,
        };
        if let Err(e) = r {
            log::warn!("storing {path}: {}", js_err(e));
        }
    }

    /// List the remote: from the cursor, or everything when there is none
    /// (or it expired). Returns the entries, the new cursor, and whether
    /// it was a complete listing.
    async fn list(&self, token: &str) -> Result<(Vec<dropbox::Metadata>, String, bool), String> {
        let root = self.root();
        let mut cursor = self.store.borrow().sync.cursor.clone();
        let mut entries = Vec::new();
        let mut complete = cursor.is_none();
        loop {
            let reply = match &cursor {
                None => net::rpc(token, "files/list_folder", &dropbox::list_folder_body(&root)).await?,
                Some(c) => net::rpc(token, "files/list_folder/continue", &dropbox::continue_body(c)).await?,
            };
            if reply.status == 409 && dropbox::refusal(&reply.text()) == Refusal::Reset && cursor.is_some() {
                cursor = None;
                entries.clear();
                complete = true;
                continue;
            }
            if reply.status == 409 {
                return Err(format!("{root} not found in Dropbox"));
            }
            if !reply.ok() {
                return Err(format!("listing failed ({})", reply.status));
            }
            let page: ListFolder = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
            entries.extend(page.entries);
            cursor = Some(page.cursor);
            if !page.has_more {
                break;
            }
        }
        Ok((entries, cursor.unwrap_or_default(), complete))
    }

    /// Download `path` and apply it through the engine. Returns the paths
    /// the vault changed at.
    async fn fetch(&self, token: &str, path: &str) -> Result<Vec<String>, String> {
        let root = self.root();
        let reply = net::download(token, &dropbox::dropbox_path(&root, path)).await?;
        if reply.status == 409 {
            // Gone since the listing; the next listing reports it.
            return Ok(Vec::new());
        }
        if !reply.ok() {
            return Err(format!("download of {path} failed ({})", reply.status));
        }
        let meta: dropbox::FileMetadata =
            serde_json::from_str(reply.api_result.as_deref().unwrap_or("")).map_err(|e| format!("{path}: {e}"))?;
        let file = meta.remote_file(&root).unwrap_or(RemoteFile {
            path: path.to_string(),
            rev: meta.rev.clone(),
            hash: meta.content_hash.clone(),
            size: meta.size,
        });
        let decision = {
            let mut st = self.store.borrow_mut();
            let st = &mut *st;
            let local = st.index.read_bytes(&file.path).ok().map(|b| dropbox::content_hash(&b));
            let index = &st.index;
            st.sync.fetched(&file, local.as_deref(), &local_stamp(), |p| index.files().contains_key(p))
        };
        let mut changed = vec![file.path.clone()];
        match decision {
            Fetched::Same => return Ok(Vec::new()),
            Fetched::Take => {}
            Fetched::TakeKeepingLocal { copy_to } => {
                let mine = self.store.borrow().index.read_bytes(&file.path).unwrap_or_default();
                self.store.borrow_mut().index.put_files([(copy_to.clone(), mine.clone())]);
                self.persist(&copy_to, Some(&mine)).await;
                changed.push(copy_to);
            }
        }
        self.store.borrow_mut().index.put_files([(file.path.clone(), reply.body.clone())]);
        self.persist(&file.path, Some(&reply.body)).await;
        Ok(changed)
    }

    async fn pass(&self) -> Result<(), String> {
        let token = self.token().await?;
        let root = self.root();
        let (entries, cursor, complete) = self.list(&token).await?;
        let changes: Vec<_> = entries.iter().filter_map(|m| dropbox::remote_change(&root, m)).collect();
        let steps = {
            let mut st = self.store.borrow_mut();
            let st = &mut *st;
            let index = &st.index;
            st.sync.listing(&changes, complete, |p| index.read_bytes(p).ok().map(|b| dropbox::content_hash(&b)))
        };
        let mut changed = Vec::new();
        let total = steps.iter().filter(|s| matches!(s, Step::Fetch { .. })).count();
        let mut done = 0;
        for step in steps {
            match step {
                Step::Fetch { path } => {
                    done += 1;
                    if total > 5 {
                        self.set_status(format!("Downloading {done} of {total}…"));
                    }
                    changed.extend(self.fetch(&token, &path).await?);
                }
                Step::Stub { path } => {
                    self.store.borrow_mut().index.put_files([(path.clone(), Vec::new())]);
                    self.persist(&path, Some(&[])).await;
                    changed.push(path);
                }
                Step::Drop { path } => {
                    self.store.borrow_mut().index.remove_files([path.as_str()]);
                    self.persist(&path, None).await;
                    changed.push(path);
                }
            }
        }
        // Take the cursor only once the listing is applied: a pass that dies
        // half way lists the same changes again.
        self.store.borrow_mut().sync.cursor = Some(cursor);
        self.save_state().await;
        if !changed.is_empty() {
            let _ = self.sender.send(Msg::Remote { changed: changed.clone() });
        }

        // Push. A refusal turns into a fetch (which keeps the local side as
        // a conflict copy), and the copy uploads on the next round.
        for _round in 0..3 {
            let mut refetch = Vec::new();
            let uploads = self.store.borrow().sync.uploads();
            for up in uploads {
                let bytes = self.store.borrow().index.read_bytes(&up.path).unwrap_or_default();
                let reply = net::upload(&token, &dropbox::dropbox_path(&root, &up.path), up.rev.as_deref(), &bytes).await?;
                if reply.status == 409 {
                    if let Step::Fetch { path } = self.store.borrow_mut().sync.upload_conflicted(&up.path) {
                        refetch.push(path);
                    }
                    continue;
                }
                if !reply.ok() {
                    return Err(format!("upload of {} failed ({})", up.path, reply.status));
                }
                let meta: dropbox::FileMetadata = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
                let mut file = meta.remote_file(&root).ok_or("upload landed outside the vault")?;
                // Keep the vault's casing for the path we track.
                file.path = up.path.clone();
                let mut st = self.store.borrow_mut();
                let now = st.index.read_bytes(&up.path).ok().map(|b| dropbox::content_hash(&b));
                st.sync.uploaded(&file, now.as_deref());
            }
            let deletes = self.store.borrow().sync.deletes();
            for del in deletes {
                let reply =
                    net::rpc(&token, "files/delete_v2", &dropbox::delete_body(&dropbox::dropbox_path(&root, &del.path), &del.rev))
                        .await?;
                if reply.ok() || (reply.status == 409 && dropbox::refusal(&reply.text()) == Refusal::NotFound) {
                    self.store.borrow_mut().sync.deleted_remote(&del.path);
                } else if reply.status == 409 {
                    if let Step::Fetch { path } = self.store.borrow_mut().sync.delete_conflicted(&del.path) {
                        refetch.push(path);
                    }
                } else {
                    return Err(format!("delete of {} failed ({})", del.path, reply.status));
                }
            }
            self.save_state().await;
            if refetch.is_empty() {
                break;
            }
            let mut more = Vec::new();
            for p in refetch {
                more.extend(self.fetch(&token, &p).await?);
            }
            if !more.is_empty() {
                let _ = self.sender.send(Msg::Remote { changed: more });
            }
        }
        self.save_state().await;
        Ok(())
    }

    async fn fetch_attachment(&self, path: String) {
        let Ok(token) = self.token().await else { return };
        let root = self.root();
        let Ok(reply) = net::download(&token, &dropbox::dropbox_path(&root, &path)).await else { return };
        if !reply.ok() {
            return;
        }
        if let Some(file) = reply
            .api_result
            .as_deref()
            .and_then(|r| serde_json::from_str::<dropbox::FileMetadata>(r).ok())
            .map(|m| RemoteFile { path: path.clone(), rev: m.rev, hash: m.content_hash, size: m.size })
        {
            self.store.borrow_mut().sync.attachment_fetched(&file);
        }
        self.store.borrow_mut().index.put_files([(path.clone(), reply.body.clone())]);
        self.persist(&path, Some(&reply.body)).await;
        self.save_state().await;
        let _ = self.sender.send(Msg::Remote { changed: vec![path] });
    }
}

impl Backend for WebBackend {
    fn persist(&mut self, writes: Vec<(String, Option<Vec<u8>>)>) {
        let inner = self.0.clone();
        spawn_local(async move {
            for (path, bytes) in writes {
                inner.persist(&path, bytes.as_deref()).await;
            }
            inner.save_state().await;
        });
    }

    fn sync_soon(&mut self) {
        self.0.schedule(DEBOUNCE_MS);
        let line = self.0.summary();
        *self.0.status.borrow_mut() = line;
    }

    fn connected(&self) -> bool {
        self.0.tokens.borrow().is_some()
    }

    fn connect(&mut self, root: &str) {
        if self.0.config.app_key.is_empty() {
            self.0.set_status("No Dropbox app key: set app_key in config.json");
            return;
        }
        let inner = self.0.clone();
        let root = root.to_string();
        spawn_local(async move {
            if let Err(e) = start_login(&inner, &root).await {
                inner.set_status(format!("Login failed: {e}"));
            }
        });
    }

    fn disconnect(&mut self) {
        *self.0.tokens.borrow_mut() = None;
        let inner = self.0.clone();
        spawn_local(async move {
            let _ = inner.db.delete_meta("tokens").await;
            let _ = inner.sender.send(Msg::Connection);
        });
    }

    fn fetch_attachment(&mut self, path: &str) {
        let inner = self.0.clone();
        let path = path.to_string();
        spawn_local(async move { inner.fetch_attachment(path).await });
    }

    fn status(&self) -> String {
        let s = self.0.status.borrow().clone();
        if s.is_empty() {
            self.0.summary()
        } else {
            s
        }
    }
}

/// Where Dropbox sends the user back: this page, without its query.
pub fn redirect_uri() -> String {
    let loc = web_sys::window().map(|w| w.location());
    let origin = loc.as_ref().and_then(|l| l.origin().ok()).unwrap_or_default();
    let path = loc.as_ref().and_then(|l| l.pathname().ok()).unwrap_or_else(|| "/".into());
    format!("{origin}{path}")
}

/// The login's first half: remember a PKCE verifier and the folder, and
/// go to Dropbox. The page reloads at [`finish_login`].
async fn start_login(inner: &Inner, root: &str) -> Result<(), String> {
    let win = web_sys::window().ok_or("no window")?;
    let mut random = [0u8; 32];
    win.crypto().map_err(js_err)?.get_random_values_with_u8_array(&mut random).map_err(js_err)?;
    let verifier = dropbox::pkce_verifier(&random);
    let state: String = random[..8].iter().map(|b| format!("{b:02x}")).collect();
    let pending = serde_json::json!({ "verifier": verifier, "state": state, "root": root }).to_string();
    inner.db.set_meta("login", &pending).await.map_err(js_err)?;
    let url = dropbox::authorize_url(&inner.config.endpoints.authorize, &inner.config.app_key, &redirect_uri(), &dropbox::pkce_challenge(&verifier), &state);
    win.location().set_href(&url).map_err(js_err)
}

/// The login's second half, on the page Dropbox redirected back to:
/// trade the code for tokens and clean the URL. `None` when this load is
/// not a login's return.
pub async fn finish_login(db: &Db, config: &Config) -> Option<Result<(Tokens, String), String>> {
    let win = web_sys::window()?;
    let search = win.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    let code = params.get("code");
    let state = params.get("state");
    let error = params.get("error_description").or_else(|| params.get("error"));
    if code.is_none() && error.is_none() {
        return None;
    }
    // Whatever happens, the code is single-use: take it out of the URL.
    if let Ok(h) = win.history() {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&redirect_uri()));
    }
    if let Some(e) = error {
        return Some(Err(e));
    }
    let pending = db.meta("login").await.ok().flatten().and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    let _ = db.delete_meta("login").await;
    let Some(pending) = pending else { return Some(Err("no login was started on this device".into())) };
    if pending["state"].as_str() != state.as_deref() {
        return Some(Err("the login answer did not match the request".into()));
    }
    let verifier = pending["verifier"].as_str().unwrap_or_default();
    let root = pending["root"].as_str().unwrap_or(crate::app::DEFAULT_ROOT).to_string();
    let reply = match net::token(&dropbox::code_grant(&config.app_key, &redirect_uri(), code.as_deref()?, verifier)).await {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    if !reply.ok() {
        return Some(Err(format!("token request failed ({}): {}", reply.status, reply.text())));
    }
    let parsed: dropbox::TokenReply = match serde_json::from_slice(&reply.body) {
        Ok(p) => p,
        Err(e) => return Some(Err(e.to_string())),
    };
    Some(Tokens::from_reply(parsed, now_ms(), None).map(|t| (t, root)).ok_or_else(|| "no refresh token".into()))
}
