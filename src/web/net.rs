//! Dropbox over `fetch`: the four calls the sync makes and the token
//! grants, each returning the status and body for `dropbox` to read. A
//! network failure (offline) is `Err`; an HTTP error status is not.

use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, Response};

use crate::dropbox;

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// The `Dropbox-API-Result` header (a download's metadata).
    pub api_result: Option<String>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

async fn send(url: &str, headers: &[(&str, &str)], body: Option<JsValue>) -> Result<Reply, String> {
    let init = RequestInit::new();
    init.set_method("POST");
    let h = Headers::new().map_err(js_err)?;
    for (k, v) in headers {
        h.set(k, v).map_err(js_err)?;
    }
    init.set_headers(&h);
    if let Some(b) = body {
        init.set_body(&b);
    }
    let req = Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let win = web_sys::window().ok_or("no window")?;
    let resp: Response = JsFuture::from(win.fetch_with_request(&req))
        .await
        .map_err(|e| format!("offline? {}", js_err(e)))?
        .dyn_into()
        .map_err(js_err)?;
    let api_result = resp.headers().get("Dropbox-API-Result").ok().flatten();
    let buf = JsFuture::from(resp.array_buffer().map_err(js_err)?).await.map_err(js_err)?;
    Ok(Reply { status: resp.status(), body: Uint8Array::new(&buf).to_vec(), api_result })
}

pub fn js_err(e: JsValue) -> String {
    e.as_string()
        .or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())))
        .unwrap_or_else(|| format!("{e:?}"))
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// An RPC endpoint: a JSON body in, a JSON body out.
pub async fn rpc(token: &str, endpoint: &str, body: &str) -> Result<Reply, String> {
    let auth = bearer(token);
    send(
        &format!("{}/{endpoint}", dropbox::API),
        &[("Authorization", &auth), ("Content-Type", "application/json")],
        Some(JsValue::from_str(body)),
    )
    .await
}

pub async fn download(token: &str, path: &str) -> Result<Reply, String> {
    let auth = bearer(token);
    let arg = dropbox::download_arg(path);
    send(&format!("{}/files/download", dropbox::CONTENT), &[("Authorization", &auth), ("Dropbox-API-Arg", &arg)], None).await
}

pub async fn upload(token: &str, path: &str, rev: Option<&str>, bytes: &[u8]) -> Result<Reply, String> {
    let auth = bearer(token);
    let arg = dropbox::upload_arg(path, rev);
    let body = Uint8Array::from(bytes);
    send(
        &format!("{}/files/upload", dropbox::CONTENT),
        &[("Authorization", &auth), ("Dropbox-API-Arg", &arg), ("Content-Type", "application/octet-stream")],
        Some(body.into()),
    )
    .await
}

/// The token endpoint, with a form body (`dropbox::code_grant` /
/// `refresh_grant`).
pub async fn token(form: &str) -> Result<Reply, String> {
    send(dropbox::TOKEN, &[("Content-Type", "application/x-www-form-urlencoded")], Some(JsValue::from_str(form))).await
}
