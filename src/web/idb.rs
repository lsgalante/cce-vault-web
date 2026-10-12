//! IndexedDB, the page's durable storage: one database with two object
//! stores, `files` (vault path → bytes) and `meta` (a name → a JSON string:
//! the sync state, the login, the Dropbox folder). Requests are turned into
//! futures here; nothing else in the crate touches the IDB API.

use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbObjectStore, IdbRequest, IdbTransactionMode};

const DB: &str = "cce-vault-web";
const VERSION: u32 = 1;
const FILES: &str = "files";
const META: &str = "meta";

/// A request's result, once it succeeds.
async fn done(req: &IdbRequest) -> Result<JsValue, JsValue> {
    let req2 = req.clone();
    let promise = Promise::new(&mut |resolve, reject| {
        let r = req2.clone();
        let ok = Closure::once_into_js(move |_e: web_sys::Event| {
            let _ = resolve.call1(&JsValue::NULL, &r.result().unwrap_or(JsValue::UNDEFINED));
        });
        let r = req2.clone();
        let err = Closure::once_into_js(move |_e: web_sys::Event| {
            let msg = r.error().ok().flatten().map(|e| JsValue::from(e.message())).unwrap_or_else(|| "IndexedDB error".into());
            let _ = reject.call1(&JsValue::NULL, &msg);
        });
        req2.set_onsuccess(Some(ok.unchecked_ref()));
        req2.set_onerror(Some(err.unchecked_ref()));
    });
    JsFuture::from(promise).await
}

pub struct Db(IdbDatabase);

impl Db {
    pub async fn open() -> Result<Db, JsValue> {
        let factory = web_sys::window()
            .ok_or("no window")?
            .indexed_db()?
            .ok_or("IndexedDB is not available")?;
        let req = factory.open_with_u32(DB, VERSION)?;
        let upgrade = Closure::once_into_js(move |e: web_sys::Event| {
            let Some(target) = e.target() else { return };
            let Ok(req) = target.dyn_into::<IdbRequest>() else { return };
            let Ok(db) = req.result().and_then(|r| r.dyn_into::<IdbDatabase>().map_err(JsValue::from)) else { return };
            let names = db.object_store_names();
            for store in [FILES, META] {
                if !names.contains(store) {
                    let _ = db.create_object_store(store);
                }
            }
        });
        req.set_onupgradeneeded(Some(upgrade.unchecked_ref()));
        let db = done(&req).await?;
        Ok(Db(db.dyn_into()?))
    }

    fn store(&self, name: &str, write: bool) -> Result<IdbObjectStore, JsValue> {
        let mode = if write { IdbTransactionMode::Readwrite } else { IdbTransactionMode::Readonly };
        self.0.transaction_with_str_and_mode(name, mode)?.object_store(name)
    }

    /// Every file, as (vault path, bytes).
    pub async fn files(&self) -> Result<Vec<(String, Vec<u8>)>, JsValue> {
        let store = self.store(FILES, false)?;
        let keys: Array = done(&store.get_all_keys()?).await?.dyn_into()?;
        let values: Array = done(&store.get_all()?).await?.dyn_into()?;
        Ok(keys
            .iter()
            .zip(values.iter())
            .filter_map(|(k, v)| Some((k.as_string()?, v.dyn_into::<Uint8Array>().ok()?.to_vec())))
            .collect())
    }

    pub async fn put_file(&self, path: &str, bytes: &[u8]) -> Result<(), JsValue> {
        let store = self.store(FILES, true)?;
        done(&store.put_with_key(&Uint8Array::from(bytes), &JsValue::from_str(path))?).await?;
        Ok(())
    }

    pub async fn delete_file(&self, path: &str) -> Result<(), JsValue> {
        let store = self.store(FILES, true)?;
        done(&store.delete(&JsValue::from_str(path))?).await?;
        Ok(())
    }

    /// Forget every file (a new vault folder, or a disconnect that wipes).
    pub async fn clear_files(&self) -> Result<(), JsValue> {
        done(&self.store(FILES, true)?.clear()?).await?;
        Ok(())
    }

    pub async fn meta(&self, name: &str) -> Result<Option<String>, JsValue> {
        let store = self.store(META, false)?;
        Ok(done(&store.get(&JsValue::from_str(name))?).await?.as_string())
    }

    pub async fn set_meta(&self, name: &str, value: &str) -> Result<(), JsValue> {
        let store = self.store(META, true)?;
        done(&store.put_with_key(&JsValue::from_str(value), &JsValue::from_str(name))?).await?;
        Ok(())
    }

    pub async fn delete_meta(&self, name: &str) -> Result<(), JsValue> {
        done(&self.store(META, true)?.delete(&JsValue::from_str(name))?).await?;
        Ok(())
    }
}
