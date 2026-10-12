//! cce-vault-web: the vault on a phone. A cce-ui app run in the browser
//! (`cce_ui::web::run`, WebGPU on a canvas) over an in-memory
//! `cce_vault::Index`, kept offline in IndexedDB and synced two ways with
//! the vault's Dropbox folder.
//!
//! The portable half, tested natively:
//! - [`sync`]: the sync engine, a state machine with no I/O.
//! - [`dropbox`]: Dropbox's HTTP API as data (bodies, headers, content
//!   hash, PKCE, path mapping).
//!
//! The app itself, portable too (it runs natively for development):
//! - [`app`]: the screens, over [`app::store`]'s shared vault and the
//!   platform [`app::store::Backend`].

pub mod app;
pub mod dropbox;
pub mod sync;
