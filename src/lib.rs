//! cce-vault-web: the vault on a phone. A cce-ui app run in the browser
//! (`cce_ui::web::run`, WebGPU on a canvas) over an in-memory
//! `cce_vault::Index`, kept offline in IndexedDB and synced with the
//! vault's server (cce-sync) by the rules the laptop daemon runs too.
//!
//! The portable half, tested natively:
//! - [`sync`]: the phone's side of `cce_sync::client`, with lazy
//!   attachments.
//! - [`login`]: signing in to the server (OAuth with PKCE) as data.
//!
//! The app itself, portable too (it runs natively for development):
//! - [`app`]: the screens, over [`app::store`]'s shared vault and the
//!   platform [`app::store::Backend`].
//!
//! The page half, wasm32 only: `web` (`start`, IndexedDB, the sync API
//! over `fetch`, the live socket, the sign-in redirect).

pub mod app;
pub mod login;
pub mod sync;
#[cfg(target_arch = "wasm32")]
pub mod web;
