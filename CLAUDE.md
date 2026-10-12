# cce-vault-web

The vault on a phone: a **PWA** that runs a cce-ui app in the browser
(`cce_ui::web::run`, WebGPU on a `<canvas>`) over an **in-memory
`cce_vault::Index`**, keeps the vault offline in the browser's storage, and
syncs it two ways with the vault's **Dropbox** folder through Dropbox's HTTP
API. It exists because iOS has no shared filesystem: the laptop syncs the
vault with the Dropbox daemon, the phone with this app, and Obsidian keeps
working on the same files.

Read `../cce-compositor/WORKSPACE.md` first (multi-repo rule, standalone
builds, concurrent sessions), and `../cce-ui/docs/platforms.md` for the
browser shell.

## Layout

| Module | What it owns |
| --- | --- |
| `sync` | The sync engine: a state machine with no I/O (`SyncState`, `Step`, `Fetched`). The rules are in its module doc. |
| `dropbox` | Dropbox's API as data: request bodies, `Dropbox-API-Arg` headers (ASCII-escaped), 409 causes, the content hash, PKCE and token grants, Dropbox path ↔ vault path. |
| `app` | The `Application`: a phone-first, one-pane navigator (header, body, toolbar of Files / Search / Today / Tasks / New). Screens are a stack (`Screen`); lists are `app::rows::Rows`; notes read in cce-ui's markdown layout and edit in `DocEditor` (autosave 1.2 s after typing stops, and on leaving the note). |
| `app::store` | What app and platform share: `Store` (the in-memory `Index`, the `SyncState`, the Dropbox root) as `Rc<RefCell<…>>`, the `Backend` trait (persist, sync, login, attachments, status) and the backend's `Msg`s. `Store::take_writes` turns the index's journal into writes to persist and marks them for sync. |
| `web` (wasm32) | The page half: `start` (open IndexedDB, finish a login returning from Dropbox, load the vault, run the app with `Sizing::Page`); `idb` (files + meta stores); `net` (fetch: RPC, download, upload, token); `backend::WebBackend` (the sync pass over the engine, debounced 2.5 s after a write and every minute while shown; PKCE login by redirect). |
| `web/` (files) | The PWA shell: `index.html` (fonts, `config.json`, WebGPU check, service worker), `sw.js` (shell cache, stamped per build), `manifest.webmanifest`, icons, `build.sh` (→ `site/`). `config.json` holds the Dropbox app key and is not committed (`config.example.json`). |
| `examples/native.rs` | The same app natively on a folder (a COPY of a vault), for driving in a shadow session. |

## Invariants

- **The portable half has no I/O and builds natively.** `sync` and
  `dropbox` are tested with `cargo test -p cce-vault-web`; the sync tests
  drive the engine against a fake Dropbox and a fake vault the way the
  browser host does (`World::pass`). A sync rule change gets a scenario
  there first.
- **No edit is lost silently.** Uploads carry the base revision (Dropbox
  refuses on a remote change), both-sides edits keep the local text as a
  `Name (conflict YYYY-MM-DD HHMM).md` copy, and a remote deletion never
  drops a file edited here. Never add an overwrite mode.
- **Nothing hidden is written, and only three `.obsidian` settings are
  read** (`sync::SETTINGS`). Plugins, workspace state and `.trash` are
  never touched.
- **Attachments are lazy**: tracked and given an empty placeholder (so
  links resolve), fetched when opened.
- **Every vault write goes through `VaultApp::flush_writes`** (persist +
  sync). A write that skips it is in memory only and is lost on reload.
- **Leaving a screen clears field focus** (`enter`): a hidden `TextBox`
  left editing keeps taking every key.

## Build and test

```sh
cargo test -p cce-vault-web          # the portable half
cargo build -p cce-vault-web --example native
```

Drive the native build in a shadow at scale 2 with a portrait output:
`cce-shadow start --new --scale 2`, `cce-shadow run wlr-randr --output
HEADLESS-1 --custom-mode 1000x1800`, then spawn
`target/debug/examples/native <vault copy>` with `CCE_FONTS_DIR` and
`CCE_ICONS_DIR` set, and `resize-window 430 860`. `CCE_VAULT_DEBUG=1` logs
each press and the screen it led to. A `touch tap` on a text field raises
the shadow's cce-keyboard over the bottom of the screen (as iOS does), so
a later tap there lands on its keys: `cce-shadow run cce-keyboard hide`,
or drive forms with the pointer.

The crate is a workspace member and builds standalone (its own
`Cargo.lock`, cce-vault pinned by rev on GitHub).
