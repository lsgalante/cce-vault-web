//! The vault's server over `fetch`, same origin: each call returns the
//! status and body for the backend to read. A network failure (offline)
//! is `Err`; an HTTP error status is not.

use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, Response};

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The page's origin (`https://host`): the server is the one that served it.
pub fn origin() -> String {
    web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default()
}

pub async fn send(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Reply, String> {
    let init = RequestInit::new();
    init.set_method(method);
    let h = Headers::new().map_err(js_err)?;
    for (k, v) in headers {
        h.set(k, v).map_err(js_err)?;
    }
    init.set_headers(&h);
    if let Some(b) = body {
        init.set_body(&Uint8Array::from(b).into());
    }
    let req = Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let win = web_sys::window().ok_or("no window")?;
    let resp: Response = JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| format!("offline? {}", js_err(e)))?
        .dyn_into()
        .map_err(js_err)?;
    let buf = JsFuture::from(resp.array_buffer().map_err(js_err)?).await.map_err(js_err)?;
    Ok(Reply { status: resp.status(), body: Uint8Array::new(&buf).to_vec() })
}

pub fn js_err(e: JsValue) -> String {
    e.as_string()
        .or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())))
        .unwrap_or_else(|| format!("{e:?}"))
}

/// The token endpoint, with a form body (`login::code_grant` /
/// `refresh_grant`).
pub async fn token(form: &str) -> Result<Reply, String> {
    send(
        "POST",
        &format!("{}/oauth/token", origin()),
        &[("Content-Type", "application/x-www-form-urlencoded")],
        Some(form.as_bytes()),
    )
    .await
}
