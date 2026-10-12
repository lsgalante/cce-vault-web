//! Dropbox's HTTP API as data: the request bodies and headers this app
//! sends, the replies it reads, the content hash, PKCE, and the mapping
//! between Dropbox paths and vault paths. No I/O here; the browser half
//! (`web::net`) sends what this builds.
//!
//! Endpoints used, all `POST`:
//! - `api.dropboxapi.com/2/files/list_folder` and `/list_folder/continue`
//! - `content.dropboxapi.com/2/files/download` and `/upload`
//! - `api.dropboxapi.com/2/files/delete_v2`
//! - `api.dropboxapi.com/oauth2/token` (code and refresh grants)

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sync::RemoteFile;

pub const API: &str = "https://api.dropboxapi.com/2";
pub const CONTENT: &str = "https://content.dropboxapi.com/2";
pub const AUTHORIZE: &str = "https://www.dropbox.com/oauth2/authorize";
pub const TOKEN: &str = "https://api.dropboxapi.com/oauth2/token";

/// Where the four kinds of call go: Dropbox's own hosts unless a
/// deployment's `config.json` points them elsewhere (a local mock, for
/// testing the whole sync without a Dropbox account).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Endpoints {
    pub api: String,
    pub content: String,
    pub authorize: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { api: API.into(), content: CONTENT.into(), authorize: AUTHORIZE.into(), token: TOKEN.into() }
    }
}

/// One entry of a `list_folder` reply.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = ".tag", rename_all = "snake_case")]
pub enum Metadata {
    File {
        path_display: String,
        rev: String,
        content_hash: String,
        size: u64,
    },
    Folder {
        path_display: String,
    },
    Deleted {
        path_display: String,
    },
}

/// A `list_folder` or `list_folder/continue` reply.
#[derive(Debug, Clone, Deserialize)]
pub struct ListFolder {
    pub entries: Vec<Metadata>,
    pub cursor: String,
    pub has_more: bool,
}

/// The body of `list_folder` for the whole vault. The first listing needs
/// no deleted entries; `continue` reports deletions since its cursor
/// regardless.
pub fn list_folder_body(root: &str) -> String {
    serde_json::json!({ "path": root, "recursive": true, "include_deleted": false }).to_string()
}

pub fn continue_body(cursor: &str) -> String {
    serde_json::json!({ "cursor": cursor }).to_string()
}

/// The `Dropbox-API-Arg` header for a download.
pub fn download_arg(path: &str) -> String {
    api_arg(&serde_json::json!({ "path": path }))
}

/// The `Dropbox-API-Arg` header for an upload: an update of the revision
/// this device last saw, so a remote change in between is a conflict
/// rather than an overwrite; or a plain add, which fails if the file has
/// appeared remotely. `strict_conflict` makes an update of a file deleted
/// remotely a conflict too.
pub fn upload_arg(path: &str, rev: Option<&str>) -> String {
    let mode = match rev {
        Some(rev) => serde_json::json!({ ".tag": "update", "update": rev }),
        None => serde_json::json!("add"),
    };
    api_arg(&serde_json::json!({
        "path": path,
        "mode": mode,
        "autorename": false,
        "mute": true,
        "strict_conflict": true,
    }))
}

/// The body of `delete_v2`, guarded by the revision this device last saw.
pub fn delete_body(path: &str, rev: &str) -> String {
    serde_json::json!({ "path": path, "parent_rev": rev }).to_string()
}

/// A JSON value as an HTTP header: Dropbox wants it ASCII, with anything
/// else escaped as `\uXXXX` (UTF-16, surrogate pairs for astral planes).
pub fn api_arg(value: &serde_json::Value) -> String {
    let json = value.to_string();
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// What a 409 reply means for this app. Dropbox puts the cause in
/// `error_summary`, a `/`-separated path of tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The file changed (or appeared, or went) remotely since the revision
    /// this device sent: an upload or delete to re-plan.
    Conflict,
    /// The path does not exist (any more).
    NotFound,
    /// The `list_folder/continue` cursor expired: list from scratch.
    Reset,
    Other,
}

pub fn refusal(body: &str) -> Refusal {
    let summary = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error_summary")?.as_str().map(String::from))
        .unwrap_or_default();
    if summary.starts_with("reset") {
        Refusal::Reset
    } else if summary.contains("not_found") {
        Refusal::NotFound
    } else if summary.contains("conflict") {
        Refusal::Conflict
    } else {
        Refusal::Other
    }
}

/// The vault path under `root` of a Dropbox path, or `None` for the root
/// itself or anything outside it. Dropbox paths are case-insensitive, and
/// `path_display` may case the root differently from how it was given.
pub fn vault_path(root: &str, path_display: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    let head = path_display.get(..root.len())?;
    if !head.eq_ignore_ascii_case(root) && head.to_lowercase() != root.to_lowercase() {
        return None;
    }
    let rest = path_display[root.len()..].strip_prefix('/')?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The Dropbox path of a vault path.
pub fn dropbox_path(root: &str, rel: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), rel.trim_start_matches('/'))
}

/// A listing entry as the sync engine sees it, or `None` for a folder or
/// anything outside the vault.
pub fn remote_change(root: &str, m: &Metadata) -> Option<crate::sync::RemoteChange> {
    use crate::sync::RemoteChange;
    match m {
        Metadata::File { path_display, rev, content_hash, size } => {
            Some(RemoteChange::File(RemoteFile {
                path: vault_path(root, path_display)?,
                rev: rev.clone(),
                hash: content_hash.clone(),
                size: *size,
            }))
        }
        Metadata::Deleted { path_display } => Some(RemoteChange::Deleted(vault_path(root, path_display)?)),
        Metadata::Folder { .. } => None,
    }
}

/// A file's metadata as returned by `upload`, or in a download's
/// `Dropbox-API-Result` header.
#[derive(Debug, Clone, Deserialize)]
pub struct FileMetadata {
    pub path_display: String,
    pub rev: String,
    pub content_hash: String,
    pub size: u64,
}

impl FileMetadata {
    pub fn remote_file(&self, root: &str) -> Option<RemoteFile> {
        Some(RemoteFile {
            path: vault_path(root, &self.path_display)?,
            rev: self.rev.clone(),
            hash: self.content_hash.clone(),
            size: self.size,
        })
    }
}

/// Dropbox's content hash: SHA-256 over the concatenated SHA-256 of each
/// 4 MiB block, hex. Equal hashes mean equal bytes, so a file this device
/// already holds is never downloaded again.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut outer = Sha256::new();
    for block in bytes.chunks(4 * 1024 * 1024) {
        outer.update(Sha256::digest(block));
    }
    hex(&outer.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// PKCE's S256 challenge for a verifier: base64url of its SHA-256, no
/// padding. The app has no secret; this is what proves the token request
/// came from whoever started the login.
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A PKCE verifier from 32 random bytes (the browser supplies them).
pub fn pkce_verifier(random: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

/// Where to send the user to log in. `token_access_type=offline` asks for
/// a refresh token, so the app stays signed in.
pub fn authorize_url(authorize: &str, app_key: &str, redirect: &str, challenge: &str, state: &str) -> String {
    format!(
        "{authorize}?client_id={}&response_type=code&code_challenge={}&code_challenge_method=S256\
         &token_access_type=offline&redirect_uri={}&state={}",
        form_encode(app_key),
        form_encode(challenge),
        form_encode(redirect),
        form_encode(state),
    )
}

/// The form body exchanging the login's code for tokens.
pub fn code_grant(app_key: &str, redirect: &str, code: &str, verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&code_verifier={}&client_id={}&redirect_uri={}",
        form_encode(code),
        form_encode(verifier),
        form_encode(app_key),
        form_encode(redirect),
    )
}

/// The form body trading the refresh token for a fresh access token.
pub fn refresh_grant(app_key: &str, refresh_token: &str) -> String {
    format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        form_encode(refresh_token),
        form_encode(app_key),
    )
}

/// The token endpoint's reply. A refresh grant carries no new refresh
/// token: keep the old one.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenReply {
    pub access_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// What the app keeps of a login (in the browser's storage).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Milliseconds since the epoch after which the access token is stale.
    pub expires_at_ms: f64,
    pub refresh_token: String,
}

impl Tokens {
    /// Apply a token reply received at `now_ms`, refreshing a minute early.
    pub fn from_reply(reply: TokenReply, now_ms: f64, previous: Option<&Tokens>) -> Option<Tokens> {
        let refresh_token = reply.refresh_token.or_else(|| previous.map(|p| p.refresh_token.clone()))?;
        Some(Tokens {
            access_token: reply.access_token,
            expires_at_ms: now_ms + (reply.expires_in.saturating_sub(60) as f64) * 1000.0,
            refresh_token,
        })
    }

    pub fn fresh(&self, now_ms: f64) -> bool {
        now_ms < self.expires_at_ms
    }
}

/// `application/x-www-form-urlencoded` / query-string encoding.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_matches_dropbox_reference() {
        // Dropbox's documented example: the empty file.
        assert_eq!(content_hash(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        // One block: SHA-256 of SHA-256("abc").
        let inner = Sha256::digest(b"abc");
        assert_eq!(content_hash(b"abc"), hex(&Sha256::digest(inner)));
        // Two blocks hash as two inner digests.
        let big = vec![7u8; 4 * 1024 * 1024 + 3];
        let mut outer = Sha256::new();
        outer.update(Sha256::digest(&big[..4 * 1024 * 1024]));
        outer.update(Sha256::digest(&big[4 * 1024 * 1024..]));
        assert_eq!(content_hash(&big), hex(&outer.finalize()));
    }

    #[test]
    fn api_arg_escapes_non_ascii() {
        let arg = download_arg("/Vault 1/Café 🎉.md");
        assert!(arg.is_ascii());
        assert!(arg.contains("Caf\\u00e9 \\ud83c\\udf89.md"), "{arg}");
        let back: serde_json::Value = serde_json::from_str(&arg).unwrap();
        assert_eq!(back["path"], "/Vault 1/Café 🎉.md");
    }

    #[test]
    fn upload_modes() {
        let add: serde_json::Value = serde_json::from_str(&upload_arg("/v/a.md", None)).unwrap();
        assert_eq!(add["mode"], "add");
        assert_eq!(add["autorename"], false);
        let upd: serde_json::Value = serde_json::from_str(&upload_arg("/v/a.md", Some("015f"))).unwrap();
        assert_eq!(upd["mode"][".tag"], "update");
        assert_eq!(upd["mode"]["update"], "015f");
        assert_eq!(upd["strict_conflict"], true);
    }

    #[test]
    fn vault_paths() {
        let root = "/Apps/remotely-save/Vault 1";
        assert_eq!(vault_path(root, "/Apps/remotely-save/Vault 1/Tasks/a.md").as_deref(), Some("Tasks/a.md"));
        assert_eq!(vault_path(root, "/apps/Remotely-Save/vault 1/a.md").as_deref(), Some("a.md"));
        assert_eq!(vault_path(root, "/Apps/remotely-save/Vault 1"), None);
        assert_eq!(vault_path(root, "/Apps/remotely-save/Vault 10/a.md"), None);
        assert_eq!(vault_path(root, "/Other/a.md"), None);
        assert_eq!(dropbox_path(root, "Tasks/a.md"), "/Apps/remotely-save/Vault 1/Tasks/a.md");
    }

    #[test]
    fn listing_entries() {
        let reply = r#"{"entries":[
            {".tag":"folder","path_display":"/V/Tasks","id":"x"},
            {".tag":"file","path_display":"/V/Tasks/a.md","rev":"01a","content_hash":"h","size":3,"id":"y"},
            {".tag":"deleted","path_display":"/V/old.md"}
        ],"cursor":"c1","has_more":false}"#;
        let list: ListFolder = serde_json::from_str(reply).unwrap();
        let changes: Vec<_> = list.entries.iter().filter_map(|m| remote_change("/V", m)).collect();
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[0], crate::sync::RemoteChange::File(f) if f.path == "Tasks/a.md" && f.rev == "01a"));
        assert!(matches!(&changes[1], crate::sync::RemoteChange::Deleted(p) if p == "old.md"));
    }

    #[test]
    fn refusals() {
        assert_eq!(refusal(r#"{"error_summary":"path/conflict/file/.."}"#), Refusal::Conflict);
        assert_eq!(refusal(r#"{"error_summary":"path_lookup/not_found/.."}"#), Refusal::NotFound);
        assert_eq!(refusal(r#"{"error_summary":"reset/.."}"#), Refusal::Reset);
        assert_eq!(refusal("garbage"), Refusal::Other);
    }

    #[test]
    fn pkce_and_tokens() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let url = authorize_url(AUTHORIZE, "key", "https://x.dev/", "ch", "st");
        assert!(url.contains("redirect_uri=https%3A%2F%2Fx.dev%2F"), "{url}");
        assert!(url.contains("token_access_type=offline"));
        let t = Tokens::from_reply(
            TokenReply { access_token: "a".into(), expires_in: 14400, refresh_token: Some("r".into()) },
            1000.0,
            None,
        )
        .unwrap();
        assert!(t.fresh(1000.0 + 14_000_000.0) && !t.fresh(1000.0 + 14_340_001.0));
        let refreshed =
            Tokens::from_reply(TokenReply { access_token: "b".into(), expires_in: 14400, refresh_token: None }, 0.0, Some(&t))
                .unwrap();
        assert_eq!(refreshed.refresh_token, "r");
        assert!(Tokens::from_reply(TokenReply { access_token: "b".into(), expires_in: 1, refresh_token: None }, 0.0, None)
            .is_none());
    }
}
