//! Signing in to the vault's server, as data: the phone is the server's
//! built-in OAuth client (`cce-vault-web`), served from the same origin,
//! so it sends the user to `/oauth/authorize` (consent, then Google) and
//! is sent back to `<origin>/` with a code, which it trades with PKCE for
//! tokens bound to `<origin>/v1`, the sync API. No secret: PKCE stands in
//! for one. Refresh tokens rotate; a refused refresh means sign in again.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The server's name for this app (its first-party client).
pub const CLIENT_ID: &str = "cce-vault-web";

/// Where the server sends the user back: the app's own page.
pub fn redirect_uri(origin: &str) -> String {
    format!("{origin}/")
}

/// The resource the tokens are for: the sync API.
pub fn resource(origin: &str) -> String {
    format!("{origin}/v1")
}

pub fn pkce_verifier(random: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The page that starts sign-in.
pub fn authorize_url(origin: &str, challenge: &str, state: &str) -> String {
    let q = query(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", &redirect_uri(origin)),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("resource", &resource(origin)),
    ]);
    format!("{origin}/oauth/authorize?{q}")
}

/// The token request's form body for the code sign-in came back with.
pub fn code_grant(origin: &str, code: &str, verifier: &str) -> String {
    query(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", CLIENT_ID),
        ("redirect_uri", &redirect_uri(origin)),
        ("code_verifier", verifier),
        ("resource", &resource(origin)),
    ])
}

pub fn refresh_grant(refresh_token: &str) -> String {
    query(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", CLIENT_ID)])
}

fn query(params: &[(&str, &str)]) -> String {
    params.iter().map(|(k, v)| format!("{}={}", escape(k), escape(v))).collect::<Vec<_>>().join("&")
}

/// Percent-encoding for a query or form value.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The token endpoint's answer.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenReply {
    pub access_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// What the phone keeps (in IndexedDB) to call the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Milliseconds since the epoch after which the access token is stale.
    pub expires_at_ms: f64,
    pub refresh_token: String,
}

impl Tokens {
    /// Take a token reply received at `now_ms`, refreshing a minute early.
    /// A reply without a refresh token keeps the previous one.
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

/// The live socket's URL for a ticket: `ws(s)://<host>/v1/live?ticket=…`.
pub fn live_url(origin: &str, ticket: &str) -> String {
    let ws = if let Some(r) = origin.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = origin.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        origin.to_string()
    };
    format!("{ws}/v1/live?ticket={}", escape(ticket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_asks_for_the_sync_api_and_comes_back_here() {
        let url = authorize_url("https://vault.example", "CH", "st");
        assert!(url.starts_with("https://vault.example/oauth/authorize?response_type=code&client_id=cce-vault-web&"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fvault.example%2F&"));
        assert!(url.contains("code_challenge=CH&code_challenge_method=S256&state=st&"));
        assert!(url.ends_with("resource=https%3A%2F%2Fvault.example%2Fv1"));
        let form = code_grant("https://vault.example", "c/1", "v");
        assert_eq!(
            form,
            "grant_type=authorization_code&code=c%2F1&client_id=cce-vault-web&redirect_uri=https%3A%2F%2Fvault.example%2F&code_verifier=v&resource=https%3A%2F%2Fvault.example%2Fv1"
        );
    }

    #[test]
    fn pkce_matches_rfc_7636() {
        // Appendix B's example.
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(pkce_verifier(&[0; 32]).len(), 43);
    }

    #[test]
    fn tokens_refresh_early_and_keep_the_refresh_token() {
        let first = Tokens::from_reply(
            TokenReply { access_token: "a".into(), expires_in: 3600, refresh_token: Some("r".into()) },
            1000.0,
            None,
        )
        .unwrap();
        assert!(first.fresh(1000.0 + 3_539_000.0));
        assert!(!first.fresh(1000.0 + 3_540_000.0));
        let next = Tokens::from_reply(TokenReply { access_token: "b".into(), expires_in: 3600, refresh_token: None }, 0.0, Some(&first)).unwrap();
        assert_eq!(next.refresh_token, "r");
        assert!(Tokens::from_reply(TokenReply { access_token: "a".into(), expires_in: 1, refresh_token: None }, 0.0, None).is_none());
    }

    #[test]
    fn the_live_socket_carries_a_ticket() {
        assert_eq!(live_url("https://v.example", "t+1"), "wss://v.example/v1/live?ticket=t%2B1");
        assert_eq!(live_url("http://127.0.0.1:8787", "t"), "ws://127.0.0.1:8787/v1/live?ticket=t");
    }
}
