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

## Build and test

```sh
cargo test -p cce-vault-web          # the portable half
```

The crate is a workspace member and builds standalone (its own
`Cargo.lock`, cce-vault pinned by rev on GitHub).
