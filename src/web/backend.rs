//! The browser's [`Backend`]: files in IndexedDB, sync with the vault's
//! server (the origin that served this page) over `fetch`, signing in
//! through the server's OAuth with PKCE (a redirect and back, no secret).
//!
//! A pass runs one at a time: a moment after the last local write, on
//! demand, when the server's live socket says the vault moved on, and
//! once a minute while the page shows. It is the laptop daemon's pass
//! (`cce-sync`'s `pass.rs`) over [`PhoneSync`](crate::sync::PhoneSync):
//! pull (a listing the first time, the change feed after), carry out each
//! act only if the file still holds what the plan saw, then push in
//! rounds. The state is saved only after a pass that finished; a pass that
//! fails puts the rules' state back as it was, which is safe because every
//! act is conditional.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use cce_sync::client::{Push, SyncState};
use cce_sync::proto::{encode_path, hash, Applied, ChangesPage, Committed, Listing, LiveNotice, Pre, Refusal, H_DEVICE};
use cce_ui::engine::AppSender;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use super::idb::Db;
use super::net::{self, js_err, origin, Reply};
use crate::app::store::{Backend, Msg, Shared};
use crate::login::{self, Tokens};
use crate::sync::Act;

/// After the last local write, wait this long for more before syncing.
const DEBOUNCE_MS: i32 = 2500;
/// After a live notice (another device's write), this long.
const NOTICE_MS: i32 = 300;
/// And sync this often regardless, while the page shows.
const PERIOD_MS: i32 = 60_000;
/// Push rounds per pass: a refusal's conflict copy goes up in the next.
const ROUNDS: usize = 4;
/// What the server records this device's changes as.
const DEVICE: &str = "phone";

/// IndexedDB meta keys.
pub const META_SESSION: &str = "session";
pub const META_SYNC: &str = "cce-sync";
const META_LOGIN: &str = "login";

type Handler = Closure<dyn FnMut(JsValue)>;

pub struct Inner {
    pub db: Db,
    pub store: Shared,
    pub sender: AppSender<Msg>,
    pub tokens: RefCell<Option<Tokens>>,
    status: RefCell<String>,
    running: Cell<bool>,
    again: Cell<bool>,
    timer: Cell<Option<i32>>,
    timer_cb: RefCell<Option<Closure<dyn FnMut()>>>,
    last_sync: RefCell<Option<String>>,
    /// Writes the server refused for good (a case collision, a bad name).
    refused: RefCell<Vec<String>>,
    socket: RefCell<Option<web_sys::WebSocket>>,
    opening: Cell<bool>,
    socket_cbs: RefCell<Vec<Handler>>,
}

pub struct WebBackend(pub Rc<Inner>);

/// A write's outcome: done, or refused by the server's rules.
enum Answer<T> {
    Done(T),
    Refused(Refusal),
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn local_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H%M").to_string()
}

fn shows() -> bool {
    web_sys::window().and_then(|w| w.document()).is_some_and(|d| !d.hidden())
}

fn random_hex<const N: usize>() -> Result<String, String> {
    let mut b = [0u8; N];
    let win = web_sys::window().ok_or("no window")?;
    win.crypto().map_err(js_err)?.get_random_values_with_u8_array(&mut b).map_err(js_err)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

impl WebBackend {
    pub fn new(db: Db, store: Shared, sender: AppSender<Msg>, tokens: Option<Tokens>) -> WebBackend {
        let inner = Rc::new(Inner {
            db,
            store,
            sender,
            tokens: RefCell::new(tokens),
            status: RefCell::new(String::new()),
            running: Cell::new(false),
            again: Cell::new(false),
            timer: Cell::new(None),
            timer_cb: RefCell::new(None),
            last_sync: RefCell::new(None),
            refused: RefCell::new(Vec::new()),
            socket: RefCell::new(None),
            opening: Cell::new(false),
            socket_cbs: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&inner);
        *inner.timer_cb.borrow_mut() = Some(Closure::new(move || {
            if let Some(i) = weak.upgrade() {
                i.timer.set(None);
                Inner::run(i);
            }
        }));
        // The periodic pass; in the background, drop the live socket (iOS
        // would cut it anyway) and let the pass on coming back reopen it.
        let weak = Rc::downgrade(&inner);
        let tick = Closure::<dyn FnMut()>::new(move || {
            if let Some(i) = weak.upgrade() {
                if shows() {
                    Inner::run(i);
                } else {
                    i.close_live();
                }
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), PERIOD_MS);
        }
        tick.forget();
        WebBackend(inner)
    }

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
                let before = this.store.borrow().sync.sync.clone();
                let mut changed = Vec::new();
                let result = this.pass(&mut changed).await;
                if !changed.is_empty() {
                    let _ = this.sender.send(Msg::Remote { changed });
                }
                match result {
                    Ok(()) => {
                        this.save_state().await;
                        let at = chrono::Local::now().format("%H:%M").to_string();
                        *this.last_sync.borrow_mut() = Some(at);
                        let line = this.summary();
                        this.set_status(line);
                        Inner::open_live(&this);
                    }
                    Err(e) => {
                        restore(&this.store, before);
                        this.set_status(format!("Sync failed: {e}"));
                    }
                }
                if !this.again.get() {
                    break;
                }
            }
            this.running.set(false);
        });
    }

    fn summary(&self) -> String {
        let pending = self.store.borrow_mut().pending();
        let at = self.last_sync.borrow().clone();
        let mut line = match (at, pending) {
            (Some(at), 0) => format!("Synced {at}"),
            (Some(at), n) => format!("Synced {at} · {n} to send"),
            (None, 0) => "Not synced yet".into(),
            (None, n) => format!("{n} to send"),
        };
        if let Some(r) = self.refused.borrow().first() {
            line.push_str(&format!(" · refused: {r}"));
        }
        line
    }

    // ---- the API --------------------------------------------------------

    /// A fresh access token, refreshing a stale one (or any, when `force`).
    async fn token(&self, force: bool) -> Result<String, String> {
        let current = self.tokens.borrow().clone().ok_or("signed out")?;
        if !force && current.fresh(now_ms()) {
            return Ok(current.access_token);
        }
        let reply = net::token(&login::refresh_grant(&current.refresh_token)).await?;
        if !reply.ok() {
            if reply.status == 400 || reply.status == 401 {
                // Revoked, expired or reused: sign in again.
                self.sign_out().await;
                return Err("signed out: sign in again".into());
            }
            return Err(format!("refreshing the sign-in failed ({})", reply.status));
        }
        let parsed: login::TokenReply = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
        let fresh = Tokens::from_reply(parsed, now_ms(), Some(&current)).ok_or("no token")?;
        let token = fresh.access_token.clone();
        let _ = self.db.set_meta(META_SESSION, &serde_json::to_string(&fresh).unwrap_or_default()).await;
        *self.tokens.borrow_mut() = Some(fresh);
        Ok(token)
    }

    async fn sign_out(&self) {
        *self.tokens.borrow_mut() = None;
        self.close_live();
        let _ = self.db.delete_meta(META_SESSION).await;
        let _ = self.sender.send(Msg::Connection);
    }

    /// A call to `/v1/<path>`. A `401` refreshes the token and tries once
    /// more.
    async fn api(&self, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&[u8]>) -> Result<Reply, String> {
        let url = format!("{}/v1/{path}", origin());
        let mut force = false;
        loop {
            let auth = format!("Bearer {}", self.token(force).await?);
            let mut headers = vec![("Authorization", auth.as_str()), (H_DEVICE, DEVICE)];
            headers.extend_from_slice(extra);
            let reply = net::send(method, &url, &headers, body).await?;
            if reply.status == 401 && !force {
                force = true;
                continue;
            }
            return Ok(reply);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let reply = self.api("GET", path, &[], None).await?;
        if !reply.ok() {
            return Err(format!("GET {path}: {} {}", reply.status, reply.text().trim()));
        }
        serde_json::from_slice(&reply.body).map_err(|e| format!("GET {path}: {e}"))
    }

    async fn blob(&self, h: &str) -> Result<Vec<u8>, String> {
        let reply = self.api("GET", &format!("blobs/{h}"), &[], None).await?;
        if !reply.ok() {
            return Err(format!("blob {h}: {}", reply.status));
        }
        if hash(&reply.body) != h {
            return Err(format!("blob {h} did not match its hash"));
        }
        Ok(reply.body)
    }

    fn answer<T: serde::de::DeserializeOwned>(what: &str, reply: Reply) -> Result<Answer<T>, String> {
        match reply.status {
            200 => serde_json::from_slice(&reply.body).map(Answer::Done).map_err(|e| format!("{what}: {e}")),
            400 | 404 | 409 | 413 => serde_json::from_slice(&reply.body)
                .map(Answer::Refused)
                .map_err(|_| format!("{what}: {} {}", reply.status, reply.text().trim())),
            s => Err(format!("{what}: {s} {}", reply.text().trim())),
        }
    }

    async fn put(&self, path: &str, pre: Pre, bytes: &[u8]) -> Result<Answer<Applied>, String> {
        let what = format!("PUT {path}");
        let rev;
        let cond = match pre {
            Pre::Rev(r) => {
                rev = r.to_string();
                ("If-Match", rev.as_str())
            }
            Pre::Absent => ("If-None-Match", "*"),
            Pre::Any => return Err(format!("{what}: a device write needs a precondition")),
        };
        let reply = self.api("PUT", &format!("files/{}", encode_path(path)), &[cond], Some(bytes)).await?;
        match Self::answer::<Committed>(&what, reply)? {
            Answer::Done(mut c) if !c.applied.is_empty() => Ok(Answer::Done(c.applied.remove(0))),
            Answer::Done(_) => Err(format!("{what}: empty answer")),
            Answer::Refused(r) => Ok(Answer::Refused(r)),
        }
    }

    async fn delete(&self, path: &str, rev: u64) -> Result<Answer<Committed>, String> {
        let rev = rev.to_string();
        let reply = self.api("DELETE", &format!("files/{}", encode_path(path)), &[("If-Match", &rev)], None).await?;
        Self::answer(&format!("DELETE {path}"), reply)
    }

    // ---- storage ----------------------------------------------------------

    async fn save_state(&self) {
        let json = serde_json::to_string(&self.store.borrow().sync).unwrap_or_default();
        if let Err(e) = self.db.set_meta(META_SYNC, &json).await {
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

    fn holds(&self, path: &str, expect: Option<&str>) -> bool {
        self.store.borrow_mut().holds(path, expect)
    }

    // ---- the pass ---------------------------------------------------------

    async fn pass(&self, changed: &mut Vec<String>) -> Result<(), String> {
        self.refused.borrow_mut().clear();
        self.pull(changed).await?;
        for _ in 0..ROUNDS {
            let pushes = {
                let mut st = self.store.borrow_mut();
                let local = st.local();
                st.sync.pushes(&local)
            };
            if pushes.is_empty() {
                break;
            }
            let mut progressed = false;
            for push in pushes {
                progressed |= self.push(push, changed).await?;
            }
            if !progressed {
                break;
            }
        }
        Ok(())
    }

    async fn pull(&self, changed: &mut Vec<String>) -> Result<(), String> {
        loop {
            if !self.store.borrow().sync.sync.listed {
                let listing: Listing = self.get_json("files").await?;
                let acts = {
                    let mut st = self.store.borrow_mut();
                    let local = st.local();
                    let steps = st.sync.sync.listing(&listing, &local);
                    st.sync.acts(steps)
                };
                return self.carry_out(acts, changed).await;
            }
            loop {
                let seq = self.store.borrow().sync.sync.seq;
                let page: ChangesPage = self.get_json(&format!("changes?since={seq}")).await?;
                if page.reset {
                    // Older than the trimmed feed: list again.
                    self.store.borrow_mut().sync.sync.listed = false;
                    break;
                }
                let acts = {
                    let mut st = self.store.borrow_mut();
                    let local = st.local();
                    let steps = st.sync.sync.changes(&page, &local);
                    st.sync.acts(steps)
                };
                self.carry_out(acts, changed).await?;
                if !page.more {
                    return Ok(());
                }
            }
        }
    }

    /// Send one push; whether it moved anything.
    async fn push(&self, push: Push, changed: &mut Vec<String>) -> Result<bool, String> {
        match push {
            Push::Put { path, pre, hash: sent } => {
                let Ok(bytes) = self.store.borrow().index.read_bytes(&path) else { return Ok(false) };
                if hash(&bytes) != sent {
                    return Ok(false); // changed since the plan; the next round sees it
                }
                match self.put(&path, pre, &bytes).await? {
                    Answer::Done(applied) => {
                        let acts = {
                            let mut st = self.store.borrow_mut();
                            let step = st.sync.sync.put_ok(&path, &sent, &applied);
                            st.sync.acts(step.into_iter().collect())
                        };
                        self.carry_out(acts, changed).await?;
                    }
                    Answer::Refused(r @ Refusal::Conflict { .. }) => {
                        let acts = {
                            let mut st = self.store.borrow_mut();
                            let st = &mut *st;
                            let index = &st.index;
                            let steps =
                                st.sync.sync.put_refused(&path, &sent, &r, &local_stamp(), |p| index.read_bytes(p).is_ok());
                            st.sync.acts(steps)
                        };
                        self.carry_out(acts, changed).await?;
                    }
                    Answer::Refused(r) => {
                        self.refused.borrow_mut().push(refusal_text(&path, &r));
                        return Ok(false);
                    }
                }
            }
            Push::Delete { path, rev } => match self.delete(&path, rev).await? {
                Answer::Done(_) | Answer::Refused(Refusal::NotFound { .. }) => {
                    self.store.borrow_mut().sync.sync.delete_ok(&path);
                }
                Answer::Refused(r) => {
                    let acts = {
                        let mut st = self.store.borrow_mut();
                        let steps = st.sync.sync.delete_refused(&path, &r);
                        st.sync.acts(steps)
                    };
                    self.carry_out(acts, changed).await?;
                }
            },
        }
        Ok(true)
    }

    async fn carry_out(&self, acts: Vec<Act>, changed: &mut Vec<String>) -> Result<(), String> {
        let total = acts.iter().filter(|a| matches!(a, Act::Fetch { .. })).count();
        let mut done = 0;
        for act in acts {
            match act {
                Act::Fetch { path, rev, hash: h, expect } => {
                    if !self.holds(&path, expect.as_deref()) {
                        continue;
                    }
                    done += 1;
                    if total > 5 {
                        self.set_status(format!("Downloading {done} of {total}…"));
                    }
                    let bytes = self.blob(&h).await?;
                    // The download took time: check again right before the write.
                    if !self.holds(&path, expect.as_deref()) {
                        continue;
                    }
                    self.store.borrow_mut().put(&path, bytes.clone());
                    self.persist(&path, Some(&bytes)).await;
                    self.store.borrow_mut().sync.downloaded(&path, rev, &h);
                    changed.push(path);
                }
                Act::Stub { path, rev, hash: h, expect } => {
                    if !self.holds(&path, expect.as_deref()) {
                        continue;
                    }
                    self.store.borrow_mut().put(&path, Vec::new());
                    self.persist(&path, Some(&[])).await;
                    self.store.borrow_mut().sync.stubbed(&path, rev, &h);
                    changed.push(path);
                }
                Act::Remove { path, expect } => {
                    if !self.holds(&path, Some(&expect)) {
                        continue;
                    }
                    {
                        let mut st = self.store.borrow_mut();
                        st.remove(&path);
                        st.sync.removed(&path);
                    }
                    self.persist(&path, None).await;
                    changed.push(path);
                }
                Act::KeepCopy { path, to, expect } => {
                    if !self.holds(&path, Some(&expect)) {
                        continue;
                    }
                    let Ok(bytes) = self.store.borrow().index.read_bytes(&path) else { continue };
                    self.store.borrow_mut().put(&to, bytes.clone());
                    self.persist(&to, Some(&bytes)).await;
                    changed.push(to);
                }
            }
        }
        Ok(())
    }

    /// Fetch a placeholder's bytes, on opening it.
    async fn fetch_attachment(&self, path: String) {
        let Some(h) = self.store.borrow().sync.to_fetch(&path) else { return };
        let bytes = match self.blob(&h).await {
            Ok(b) => b,
            Err(e) => return self.set_status(format!("{path}: {e}")),
        };
        let rev = {
            let st = self.store.borrow();
            // Still the placeholder for these bytes, and still their base?
            let same = st.sync.to_fetch(&path).as_deref() == Some(h.as_str());
            st.sync.sync.files.get(&path).filter(|b| same && b.hash == h).map(|b| b.rev)
        };
        let Some(rev) = rev else { return };
        self.store.borrow_mut().put(&path, bytes.clone());
        self.persist(&path, Some(&bytes)).await;
        self.store.borrow_mut().sync.downloaded(&path, rev, &h);
        // The base did not move (same rev and hash), so saving outside a
        // pass is safe here; it records that the file is no placeholder.
        if !self.running.get() {
            self.save_state().await;
        }
        let _ = self.sender.send(Msg::Remote { changed: vec![path] });
    }

    // ---- the live socket --------------------------------------------------

    /// Open the live socket if it is not: trade the token for a one-use
    /// ticket (a browser cannot set a header on a WebSocket) and connect.
    /// A notice past our seq starts a pass; a closed socket is reopened by
    /// the next pass that succeeds.
    fn open_live(this: &Rc<Inner>) {
        if this.socket.borrow().is_some() || this.opening.get() || !shows() {
            return;
        }
        this.opening.set(true);
        let this = this.clone();
        spawn_local(async move {
            if let Err(e) = Inner::connect_live(&this).await {
                log::warn!("live socket: {e}");
            }
            this.opening.set(false);
        });
    }

    async fn connect_live(this: &Rc<Inner>) -> Result<(), String> {
        let reply = this.api("POST", "live-ticket", &[], None).await?;
        if !reply.ok() {
            return Err(format!("ticket: {}", reply.status));
        }
        let v: serde_json::Value = serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
        let ticket = v["ticket"].as_str().ok_or("no ticket")?;
        let ws = web_sys::WebSocket::new(&login::live_url(&origin(), ticket)).map_err(js_err)?;
        let weak: Weak<Inner> = Rc::downgrade(this);
        let on_message = Handler::new(move |e: JsValue| {
            let Some(i) = weak.upgrade() else { return };
            let Some(text) = e.dyn_ref::<web_sys::MessageEvent>().and_then(|m| m.data().as_string()) else { return };
            if let Ok(n) = serde_json::from_str::<LiveNotice>(&text) {
                if n.seq > i.store.borrow().sync.sync.seq {
                    i.schedule(NOTICE_MS);
                }
            }
        });
        let weak: Weak<Inner> = Rc::downgrade(this);
        let on_close = Handler::new(move |_| {
            if let Some(i) = weak.upgrade() {
                i.socket.borrow_mut().take();
            }
        });
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_close.as_ref().unchecked_ref()));
        *this.socket_cbs.borrow_mut() = vec![on_message, on_close];
        *this.socket.borrow_mut() = Some(ws);
        Ok(())
    }

    fn close_live(&self) {
        if let Some(ws) = self.socket.borrow_mut().take() {
            ws.set_onclose(None);
            ws.set_onerror(None);
            ws.set_onmessage(None);
            let _ = ws.close();
        }
    }
}

/// A failed pass: the rules' state goes back to where the pass started
/// (bases, seq). Files it already wrote match the server, so the next pass
/// adopts them; placeholders it made keep their hashes, so none is ever
/// taken for an empty edit.
fn restore(store: &Shared, before: SyncState) {
    store.borrow_mut().sync.sync = before;
}

fn refusal_text(path: &str, r: &Refusal) -> String {
    match r {
        Refusal::CaseCollision { existing, .. } => format!("{path} differs only in case from {existing}"),
        Refusal::BadPath { reason, .. } => format!("{path}: {reason}"),
        Refusal::TooLarge { .. } => format!("{path} is over 100 MB"),
        other => format!("{path}: {other:?}"),
    }
}

impl Backend for WebBackend {
    fn persist(&mut self, writes: Vec<(String, Option<Vec<u8>>)>) {
        // Only the files: the sync state is saved after a pass, never in one.
        let inner = self.0.clone();
        spawn_local(async move {
            for (path, bytes) in writes {
                inner.persist(&path, bytes.as_deref()).await;
            }
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

    fn connect(&mut self) {
        let inner = self.0.clone();
        spawn_local(async move {
            if let Err(e) = start_login(&inner).await {
                inner.set_status(format!("Sign-in failed: {e}"));
            }
        });
    }

    fn disconnect(&mut self) {
        let inner = self.0.clone();
        spawn_local(async move { inner.sign_out().await });
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

/// Signing in, first half: remember a PKCE verifier and go to the
/// server's sign-in page. The page reloads at [`finish_login`].
async fn start_login(inner: &Inner) -> Result<(), String> {
    let win = web_sys::window().ok_or("no window")?;
    let mut random = [0u8; 32];
    win.crypto().map_err(js_err)?.get_random_values_with_u8_array(&mut random).map_err(js_err)?;
    let verifier = login::pkce_verifier(&random);
    let state = random_hex::<16>()?;
    let pending = serde_json::json!({ "verifier": verifier, "state": state }).to_string();
    inner.db.set_meta(META_LOGIN, &pending).await.map_err(js_err)?;
    let url = login::authorize_url(&origin(), &login::pkce_challenge(&verifier), &state);
    win.location().set_href(&url).map_err(js_err)
}

/// Signing in, second half, on the page the server sent the user back to:
/// trade the code for tokens and clean the URL. `None` when this load is
/// not a sign-in's return.
pub async fn finish_login(db: &Db) -> Option<Result<Tokens, String>> {
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
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&login::redirect_uri(&origin())));
    }
    let pending = db.meta(META_LOGIN).await.ok().flatten().and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    let _ = db.delete_meta(META_LOGIN).await;
    if let Some(e) = error {
        return Some(Err(e));
    }
    let Some(pending) = pending else { return Some(Err("no sign-in was started on this device".into())) };
    if pending["state"].as_str() != state.as_deref() {
        return Some(Err("the sign-in answer did not match the request".into()));
    }
    let verifier = pending["verifier"].as_str().unwrap_or_default();
    let reply = match net::token(&login::code_grant(&origin(), code.as_deref()?, verifier)).await {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    if !reply.ok() {
        return Some(Err(format!("token request failed ({}): {}", reply.status, reply.text())));
    }
    let parsed: login::TokenReply = match serde_json::from_slice(&reply.body) {
        Ok(p) => p,
        Err(e) => return Some(Err(e.to_string())),
    };
    Some(Tokens::from_reply(parsed, now_ms(), None).ok_or_else(|| "no refresh token".into()))
}
