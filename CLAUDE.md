# cce-vault-web

The vault on a phone: a **PWA** that runs a cce-ui app in the browser
(`cce_ui::web::run`, WebGPU on a `<canvas>`) over an **in-memory
`cce_vault::Index`**, keeps the vault offline in the browser's storage, and
syncs it with the vault's server, **cce-sync** (`../cce-sync`, served by
`../cce-sync-server`), by the same rules the laptop daemon runs. It exists
because iOS has no shared filesystem. The site is served by the cce-sync
Worker itself, so the page, the sync API and sign-in share one origin.

Read `../cce-compositor/WORKSPACE.md` first (multi-repo rule, standalone
builds, concurrent sessions), `../cce-ui/docs/platforms.md` for the
browser shell, and `../cce-sync/CLAUDE.md` for the sync rules.

## Layout

| Module | What it owns |
| --- | --- |
| `sync` | `PhoneSync`: `cce_sync::client`'s `SyncState` plus the phone's one difference, lazy attachments (placeholders, `Act::Stub`, `to_fetch`), and `migrated` (the first run after Dropbox). Scenarios against `cce_sync::core` in its tests. |
| `login` | Signing in as data: the first-party client `cce-vault-web`, the authorize URL (resource `<origin>/v1`), PKCE, the token forms, `Tokens`, the live socket's ticket URL. |
| `app` | The `Application`: a phone-first, one-pane navigator (header, body, toolbar of Files / Search / Today / Tasks / New). Screens are a stack (`Screen`); lists are `app::rows::Rows`; notes read in cce-ui's markdown layout and edit in `DocEditor` (autosave 1.2 s after typing stops, and on leaving the note). |
| `app::store` | What app and platform share: `Store` (the in-memory `Index`, the `PhoneSync`, the server's name, a hash cache) as `Rc<RefCell<…>>`, the `Backend` trait (persist, sync, sign in/out, attachments, status) and the backend's `Msg`s. `Store::take_writes` turns the index's journal into writes to persist; `Store::local` is what the rules see (hidden files excluded, placeholders as their hash). |
| `web` (wasm32) | The page half: `start` (open IndexedDB, finish a sign-in, migrate a Dropbox-era device, load the vault, run the app with `Sizing::Page`); `idb` (files + meta stores); `net` (same-origin fetch, the token endpoint); `backend::WebBackend` (the pass: pull, conditional acts, push rounds; debounced 2.5 s after a write, on a live-socket notice, every minute while shown; the live socket by ticket; PKCE sign-in by redirect). |
| `web/` (files) | The PWA shell: `index.html`, `app.js` (fonts, WebGPU check, service worker), `sw.js` (shell cache, stamped per build; never touches `/v1`, `/oauth`, `/mcp`, `/.well-known`), `manifest.webmanifest`, icons, `_headers` (CSP), `build.sh` (→ `site/`). No configuration: the server is the page's origin. |
| `examples/native.rs` | The same app natively on a folder (a COPY of a vault), for driving in a shadow session. |

## Invariants

- **The sync rules are cce-sync's, not this crate's.** `cce_sync::client`
  decides; `sync` only adds placeholders. A rule change goes into
  cce-sync (with its scenario there) so the laptop and the phone cannot
  disagree. `sync`'s tests drive a phone against `cce_sync::core` the way
  the browser host does (`Phone::pass`).
- **No edit is lost silently.** Every write carries the base revision,
  the server merges or refuses, a refusal keeps the local text as a
  `Name (conflict YYYY-MM-DD HHMM).md` copy, and every local write from
  sync is conditional on the hash the plan saw, checked again after the
  download. Never add an overwrite mode.
- **The sync state is saved after a pass, never during**, and a failed
  pass puts the rules' state back (`backend::restore`); app writes persist
  files only. Placeholders keep their hashes through a restore.
- **Hidden files never sync** (`cce_sync::proto::check_path`): an old
  `.obsidian/` on the phone stays there. `.cce/vault.kdl` does sync.
- **Attachments are lazy**: given an empty placeholder (so links resolve)
  that the rules read as the bytes it stands for, fetched by hash when
  opened. A placeholder is never pushed, and cannot be renamed until
  fetched (its empty bytes would go up as the file).
- **The phone is the server's first-party OAuth client** (`cce-vault-web`,
  redirect `<origin>/` only, resource `<origin>/v1`); its tokens are good
  for `/v1` only, never `/mcp`. The live socket takes a one-use ticket from
  `POST /v1/live-ticket`, since a browser cannot set a WebSocket header.
- **Every vault write goes through `VaultApp::flush_writes`** (persist +
  sync). A write that skips it is in memory only and is lost on reload.
- **The editor takes key presses only**: a browser's keyup carries the
  key's text, and handed to `DocEditor::key` it typed every letter twice.
- **Symbols fall back to the last page font** (`web/fonts/fa-regular-400.ttf`,
  for the Tasks plugin's 📅): the browser has no system fallback, and the
  measuring fallback is the first face in `FONTS` with the glyph.
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

### Deploy

The site ships with the server: `../cce-sync-server/deploy.sh` runs
`web/build.sh` into the Worker's `site/` and deploys both (Workers static
assets; `NO_APP=1` skips the app). The phone opens
`https://cce-sync.lsgalante12.workers.dev/` (or `vault.lucas.co` once its
DNS is on Cloudflare), signs in with Google, and is added to the home
screen. `web/_headers` sets the CSP for the site's files: scripts and wasm
from the site only, `connect-src 'self'`, no framing. It is why the page
script is `app.js` (not inline) and why the shell may build no code from
strings (cce-ui@e70b446). The old Pages project (`cce-vault-web.pages.dev`,
on Dropbox) is retired.

### In a browser, against a local server

`web/build.sh` builds `site/` (needs the wasm32 target and the
`wasm-bindgen` CLI at the lock's version; it checks). Run the server under
`wrangler dev` in `../cce-sync-server` (its `dev/e2e-mcp.sh` header has the
`.dev.vars`, including the mock Google), with `site/` built there by
`deploy.sh`'s build step, and seed the vault with `cce-sync import` from a
COPY. The built-in browser pane has no WebGPU adapter; use Playwright's
Chromium in a shadow (`--ozone-platform=wayland --enable-unsafe-webgpu
--enable-features=Vulkan,WebGPUService --app=http://localhost:8787/
--remote-debugging-port=9222`, a fresh `--user-data-dir`, `vkey hold`
running first). Drive it with `ccectl touch tap` (a finger) and
`verify/clients`' `vkey` (keys; `ccectl keypress` does not reach
Chromium), and read page state over CDP on :9222. Make remote changes with
the laptop daemon or curl. The service worker serves the cached shell
first, so a new build shows on the second reload.

The crate is a workspace member and builds standalone (its own
`Cargo.lock`, cce-vault pinned by rev on GitHub).
