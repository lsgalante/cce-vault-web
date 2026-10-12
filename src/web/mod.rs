//! The page half (wasm32 only): `start`, which the page's script calls
//! with the canvas, the font files and the configuration. It opens the
//! IndexedDB store, finishes a Dropbox login if this load is one's return,
//! loads the vault into memory, installs the browser backend and runs the
//! app on the canvas with cce-ui's browser shell.

mod backend;
mod idb;
mod net;

use std::cell::RefCell;
use std::rc::Rc;

use cce_ui::web::{Fonts, Sizing};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::app::store::{Backend, Msg, Store};
use crate::app::{install, VaultApp, DEFAULT_ROOT};
use crate::sync::SyncState;
use backend::{Config, WebBackend};

/// Run the app on `canvas`. `fonts` is an array of font files' bytes;
/// `families` names the generic serif / sans-serif / monospace families as
/// `"serif,sans,mono"`; `config` is `config.json`'s text.
#[wasm_bindgen]
pub async fn start(canvas: web_sys::HtmlCanvasElement, fonts: js_sys::Array, families: String, config: String) -> Result<(), JsValue> {
    std::panic::set_hook(Box::new(|info| web_sys::console::error_1(&info.to_string().into())));
    let config: Config = serde_json::from_str(&config).unwrap_or_default();
    let db = idb::Db::open().await?;

    let mut tokens = db.meta("tokens").await?.and_then(|t| serde_json::from_str(&t).ok());
    let mut root = db.meta("root").await?.unwrap_or_else(|| DEFAULT_ROOT.to_string());
    let mut login_error = None;
    match backend::finish_login(&db, &config).await {
        Some(Ok((t, r))) => {
            db.set_meta("tokens", &serde_json::to_string(&t).unwrap_or_default()).await?;
            if r != root {
                // Another folder: what this device held belongs to the old one.
                db.clear_files().await?;
                db.delete_meta("sync").await?;
            }
            db.set_meta("root", &r).await?;
            tokens = Some(t);
            root = r;
        }
        Some(Err(e)) => login_error = Some(e),
        None => {}
    }

    let files = db.files().await?;
    let sync: SyncState = db.meta("sync").await?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let store = Rc::new(RefCell::new(Store::new(files, sync, root)));

    let db = RefCell::new(Some(db));
    install(Box::new(move |sender| {
        watch_visibility(sender.clone());
        let db = db.borrow_mut().take().expect("installed once");
        let backend = WebBackend::new(db, store.clone(), sender, config, tokens);
        if let Some(e) = login_error {
            *backend_status(&backend) = format!("Login failed: {e}");
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

fn backend_status(b: &WebBackend) -> std::cell::RefMut<'_, String> {
    b.status_cell()
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
