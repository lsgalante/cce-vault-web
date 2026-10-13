//! The page half (wasm32 only): `start`, which the page's script calls
//! with the canvas and the font files. It opens the IndexedDB store,
//! finishes a sign-in if this load is one's return, loads the vault into
//! memory, installs the browser backend and runs the app on the canvas
//! with cce-ui's browser shell.

mod backend;
mod idb;
mod net;

use std::cell::RefCell;
use std::rc::Rc;

use cce_ui::web::{Fonts, Sizing};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::app::store::{Backend, Msg, Store};
use crate::app::{install, VaultApp};
use crate::sync::PhoneSync;
use backend::{WebBackend, META_SESSION, META_SYNC};

/// What a phone that synced with Dropbox kept: its state and login.
const DROPBOX_META: [&str; 3] = ["sync", "root", "tokens"];

/// Run the app on `canvas`. `fonts` is an array of font files' bytes;
/// `families` names the generic serif / sans-serif / monospace families as
/// `"serif,sans,mono"`.
#[wasm_bindgen]
pub async fn start(canvas: web_sys::HtmlCanvasElement, fonts: js_sys::Array, families: String) -> Result<(), JsValue> {
    std::panic::set_hook(Box::new(|info| web_sys::console::error_1(&info.to_string().into())));
    let db = idb::Db::open().await?;

    let mut tokens = db.meta(META_SESSION).await?.and_then(|t| serde_json::from_str(&t).ok());
    let mut login_error = None;
    match backend::finish_login(&db).await {
        Some(Ok(t)) => {
            db.set_meta(META_SESSION, &serde_json::to_string(&t).unwrap_or_default()).await?;
            tokens = Some(t);
        }
        Some(Err(e)) => login_error = Some(e),
        None => {}
    }

    let files = db.files().await?;
    let sync = match db.meta(META_SYNC).await?.and_then(|s| serde_json::from_str::<PhoneSync>(&s).ok()) {
        Some(s) => s,
        None => {
            // First run on cce-sync. Files kept from Dropbox are matched
            // against the server by hash on the first pass (an unsent edit
            // becomes a conflict copy); its empty placeholders stay ones.
            let s = PhoneSync::migrated(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
            for k in DROPBOX_META {
                db.delete_meta(k).await?;
            }
            s
        }
    };
    let server = net::origin();
    let server = server.split("://").nth(1).unwrap_or(&server).to_string();
    let store = Rc::new(RefCell::new(Store::new(files, sync, server)));

    let db = RefCell::new(Some(db));
    install(Box::new(move |sender| {
        watch_visibility(sender.clone());
        let db = db.borrow_mut().take().expect("installed once");
        let backend = WebBackend::new(db, store.clone(), sender, tokens);
        if let Some(e) = login_error {
            *backend.status_cell() = format!("Sign-in failed: {e}");
        }
        (store, Box::new(backend) as Box<dyn Backend>)
    }));

    let mut fonts = Fonts::new(fonts.iter().map(|f| js_sys::Uint8Array::new(&f).to_vec()).collect());
    let mut names = families.split(',').map(|f| Some(f.trim()).filter(|f| !f.is_empty()).map(String::from));
    fonts.serif = names.next().flatten();
    fonts.sans_serif = names.next().flatten();
    fonts.monospace = names.next().flatten();
    cce_ui::web::run::<VaultApp>(canvas, fonts, Sizing::Page).await
}

/// Tell the app when the page goes to the background (save now: iOS may
/// end it there) and comes back (sync).
fn watch_visibility(sender: cce_ui::engine::AppSender<Msg>) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    let d = doc.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let _ = sender.send(if d.hidden() { Msg::Hidden } else { Msg::Shown });
    });
    let _ = doc.add_event_listener_with_callback("visibilitychange", cb.as_ref().unchecked_ref());
    cb.forget();
}
