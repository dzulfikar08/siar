//! Hosted mode: the OAuth 2.1 authorization server for `/mcp` (MCP authorization, 2025-06-18), so
//! Claude.ai and ChatGPT connectors can sign in without an API key. Dynamic client registration
//! (RFC 7591, public clients only), authorization codes with PKCE S256, refresh token rotation.
//! Only SHA-256 hashes of codes and tokens are stored, like API keys. An access token acts as an
//! API key of the tenant that granted it. HTTP handlers are in web.rs; storage and checks here.

use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::control::{ct_eq, sha256_hex};
use crate::util::{bail, hex, now, random_bytes, Result};

pub const ACCESS_TTL_MS: i64 = 3_600_000;
pub const REFRESH_TTL_MS: i64 = 90 * 86_400_000;
pub const CODE_TTL_MS: i64 = 5 * 60_000;
pub const ACCESS_PREFIX: &str = "siar_at_";
const REFRESH_PREFIX: &str = "siar_rt_";
const CODE_PREFIX: &str = "siar_ac_";

/// RFC 7636 S256: BASE64URL(SHA256(verifier)) equals the challenge; the verifier is 43 to 128
/// unreserved characters.
pub fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    let valid = (43..=128).contains(&verifier.len()) && verifier.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    if !valid || challenge.is_empty() {
        return false;
    }
    let want = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    ct_eq(&want, challenge)
}

/// A code_challenge as S256 produces it: 43 base64url characters.
pub fn challenge_ok(c: &str) -> bool {
    c.len() == 43 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The host (and port) of an http(s) URL, for showing where a consent sends the user.
pub fn redirect_host(u: &str) -> String {
    let rest = u.split_once("://").map(|x| x.1).unwrap_or(u);
    rest.split(['/', '?', '#']).next().unwrap_or("").to_string()
}

/// Redirect URIs a client may register: https without a fragment or credentials, or http on
/// localhost / 127.0.0.1 / [::1] (native apps, RFC 8252).
pub fn redirect_uri_ok(u: &str) -> bool {
    if u.len() > 500 || u.contains('#') || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let host = redirect_host(u);
    if host.is_empty() || host.contains('@') {
        return false;
    }
    if u.starts_with("https://") {
        return true;
    }
    if u.starts_with("http://") {
        let name = if host.starts_with('[') {
            host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
        } else {
            host.split(':').next().unwrap_or("").to_string()
        };
        return matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]");
    }
    false
}

/// Untrusted display name from a registration: no control characters, at most 80 characters.
pub fn clean_name(n: &str) -> String {
    let s: String = n.chars().filter(|c| !c.is_control()).take(80).collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "MCP client".into()
    } else {
        s
    }
}

fn secret(prefix: &str) -> String {
    format!("{prefix}{}", hex(&random_bytes(32)))
}

// ---------- clients (RFC 7591) ----------

#[derive(Clone, Debug)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
}

/// Register a public client. Clients that never got a grant are removed after 7 days.
pub fn register(c: &Connection, name: &str, redirect_uris: &[String]) -> Result<Client> {
    if redirect_uris.is_empty() || redirect_uris.len() > 10 {
        bail!("redirect_uris must list 1 to 10 URIs");
    }
    if let Some(bad) = redirect_uris.iter().find(|u| !redirect_uri_ok(u)) {
        bail!("Redirect URI not allowed (https, or http on localhost only): {bad}");
    }
    let client = Client {
        id: format!("mcp_{}", hex(&random_bytes(16))),
        name: clean_name(name),
        redirect_uris: redirect_uris.to_vec(),
        created_at: now(),
    };
    c.execute(
        "DELETE FROM oauth_clients WHERE created_at < ? AND id NOT IN (SELECT client_id FROM oauth_grants)",
        [now() - 7 * 86_400_000],
    )?;
    c.execute(
        "INSERT INTO oauth_clients (id, name, redirect_uris, created_at) VALUES (?, ?, ?, ?)",
        params![client.id, client.name, serde_json::to_string(&client.redirect_uris)?, client.created_at],
    )?;
    Ok(client)
}

/// Client ID Metadata Documents (the MCP spec's preferred client identity, which Claude.ai uses
/// by default): the client_id is an https URL whose JSON names the client and its redirect URIs.
pub fn is_metadata_url(client_id: &str) -> bool {
    client_id.starts_with("https://") && client_id.len() <= 300 && redirect_host(client_id).len() + 8 < client_id.len() && !client_id.contains('#')
}

/// Check a fetched metadata document against the URL it came from.
pub fn client_from_metadata(url: &str, doc: &serde_json::Value) -> Result<Client> {
    if doc.get("client_id").and_then(|v| v.as_str()) != Some(url) {
        bail!("The client metadata document's client_id is not its own URL");
    }
    let uris: Vec<String> = doc
        .get("redirect_uris")
        .and_then(|u| u.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if uris.is_empty() || uris.len() > 10 || uris.iter().any(|u| !redirect_uri_ok(u)) {
        bail!("The client metadata document has no usable redirect_uris");
    }
    Ok(Client {
        id: url.to_string(),
        name: clean_name(doc.get("client_name").and_then(|v| v.as_str()).unwrap_or(&redirect_host(url))),
        redirect_uris: uris,
        created_at: now(),
    })
}

/// Store (or refresh) a client known by its metadata document.
pub fn save_client(c: &Connection, cl: &Client) -> Result<()> {
    c.execute(
        "INSERT INTO oauth_clients (id, name, redirect_uris, created_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, redirect_uris = excluded.redirect_uris",
        params![cl.id, cl.name, serde_json::to_string(&cl.redirect_uris)?, cl.created_at],
    )?;
    Ok(())
}

pub fn client(c: &Connection, id: &str) -> Result<Option<Client>> {
    let row: Option<(String, String, String, i64)> = c
        .query_row("SELECT id, name, redirect_uris, created_at FROM oauth_clients WHERE id = ?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    Ok(row.map(|(id, name, uris, created_at)| Client {
        id,
        name,
        redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
        created_at,
    }))
}

// ---------- authorization codes and tokens ----------

/// After the user allows a client: the tenant's grant for it (reused when one is live) and a
/// one-time code bound to the redirect URI and the PKCE challenge.
pub fn authorize(c: &Connection, tenant_id: &str, client_id: &str, redirect_uri: &str, challenge: &str, resource: Option<&str>) -> Result<String> {
    let existing: Option<String> = c
        .query_row(
            "SELECT id FROM oauth_grants WHERE tenant_id = ? AND client_id = ? AND revoked_at IS NULL",
            params![tenant_id, client_id],
            |r| r.get(0),
        )
        .optional()?;
    let grant = match existing {
        Some(g) => {
            c.execute("UPDATE oauth_grants SET redirect_uri = ? WHERE id = ?", params![redirect_uri, g])?;
            g
        }
        None => {
            let g = format!("grant_{}", hex(&random_bytes(8)));
            c.execute(
                "INSERT INTO oauth_grants (id, tenant_id, client_id, redirect_uri, created_at) VALUES (?, ?, ?, ?, ?)",
                params![g, tenant_id, client_id, redirect_uri, now()],
            )?;
            g
        }
    };
    let code = secret(CODE_PREFIX);
    c.execute("DELETE FROM oauth_codes WHERE expires_at < ?", [now() - 86_400_000])?;
    c.execute(
        "INSERT INTO oauth_codes (hash, grant_id, redirect_uri, challenge, resource, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
        params![sha256_hex(&code), grant, redirect_uri, challenge, resource, now() + CODE_TTL_MS],
    )?;
    Ok(code)
}

pub struct Issued {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

/// A token endpoint error (RFC 6749 5.2): `error` code and description.
#[derive(Debug, PartialEq)]
pub struct TokenError {
    pub error: &'static str,
    pub description: String,
}

fn invalid_grant(d: &str) -> TokenError {
    TokenError {
        error: "invalid_grant",
        description: d.into(),
    }
}

fn issue(c: &Connection, grant_id: &str) -> Result<Issued> {
    let (access, refresh) = (secret(ACCESS_PREFIX), secret(REFRESH_PREFIX));
    // Expired tokens go after a day; used refresh tokens stay until then so a replay is refused.
    c.execute("DELETE FROM oauth_tokens WHERE expires_at < ?", [now() - 86_400_000])?;
    for (t, kind, ttl) in [(&access, "access", ACCESS_TTL_MS), (&refresh, "refresh", REFRESH_TTL_MS)] {
        c.execute(
            "INSERT INTO oauth_tokens (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
            params![sha256_hex(t), grant_id, kind, now() + ttl, now()],
        )?;
    }
    Ok(Issued {
        access_token: access,
        refresh_token: refresh,
        expires_in: ACCESS_TTL_MS / 1000,
    })
}

/// grant_type=authorization_code. A code works once: presenting a used code again also revokes
/// the tokens issued from its grant (RFC 6749 4.1.2).
pub fn exchange_code(c: &Connection, client_id: &str, code: &str, redirect_uri: &str, verifier: &str) -> Result<std::result::Result<Issued, TokenError>> {
    if !code.starts_with(CODE_PREFIX) {
        return Ok(Err(invalid_grant("Unknown authorization code")));
    }
    type Row = (String, String, String, i64, Option<i64>, String, Option<i64>);
    let row: Option<Row> = c
        .query_row(
            "SELECT o.grant_id, o.redirect_uri, o.challenge, o.expires_at, o.used_at, g.client_id, g.revoked_at
             FROM oauth_codes o JOIN oauth_grants g ON g.id = o.grant_id WHERE o.hash = ?",
            [sha256_hex(code)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()?;
    let Some((grant, stored_uri, challenge, expires_at, used_at, owner, revoked_at)) = row else {
        return Ok(Err(invalid_grant("Unknown authorization code")));
    };
    if used_at.is_some() {
        c.execute("DELETE FROM oauth_tokens WHERE grant_id = ?", [&grant])?;
        return Ok(Err(invalid_grant("The authorization code was already used")));
    }
    if expires_at < now() {
        return Ok(Err(invalid_grant("The authorization code expired")));
    }
    if owner != client_id {
        return Ok(Err(invalid_grant("The code was issued to another client")));
    }
    if revoked_at.is_some() {
        return Ok(Err(invalid_grant("Access was revoked")));
    }
    if stored_uri != redirect_uri {
        return Ok(Err(invalid_grant("redirect_uri does not match the authorization request")));
    }
    if !pkce_ok(verifier, &challenge) {
        return Ok(Err(invalid_grant("PKCE verification failed")));
    }
    if c.execute(
        "UPDATE oauth_codes SET used_at = ? WHERE hash = ? AND used_at IS NULL",
        params![now(), sha256_hex(code)],
    )? == 0
    {
        return Ok(Err(invalid_grant("The authorization code was already used")));
    }
    Ok(Ok(issue(c, &grant)?))
}

/// grant_type=refresh_token: a refresh token works once and is replaced (rotation).
pub fn refresh(c: &Connection, client_id: &str, token: &str) -> Result<std::result::Result<Issued, TokenError>> {
    if !token.starts_with(REFRESH_PREFIX) {
        return Ok(Err(invalid_grant("Unknown refresh token")));
    }
    let h = sha256_hex(token);
    type Row = (String, i64, Option<i64>, String, Option<i64>);
    let row: Option<Row> = c
        .query_row(
            "SELECT t.grant_id, t.expires_at, t.used_at, g.client_id, g.revoked_at
             FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id WHERE t.hash = ? AND t.kind = 'refresh'",
            [&h],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((grant, expires_at, used_at, owner, revoked_at)) = row else {
        return Ok(Err(invalid_grant("Unknown refresh token")));
    };
    if used_at.is_some() {
        return Ok(Err(invalid_grant("The refresh token was already used")));
    }
    if expires_at < now() {
        return Ok(Err(invalid_grant("The refresh token expired")));
    }
    if !client_id.is_empty() && owner != client_id {
        return Ok(Err(invalid_grant("The refresh token was issued to another client")));
    }
    if revoked_at.is_some() {
        return Ok(Err(invalid_grant("Access was revoked")));
    }
    if c.execute("UPDATE oauth_tokens SET used_at = ? WHERE hash = ? AND used_at IS NULL", params![now(), h])? == 0 {
        return Ok(Err(invalid_grant("The refresh token was already used")));
    }
    Ok(Ok(issue(c, &grant)?))
}

pub struct Hit {
    pub grant_id: String,
    pub tenant_id: String,
}

/// An access token presented to /mcp: live, its grant not revoked, its tenant active.
pub fn find_access(c: &Connection, token: &str) -> Result<Option<Hit>> {
    if !token.starts_with(ACCESS_PREFIX) || token.len() > 200 {
        return Ok(None);
    }
    let h = sha256_hex(token);
    let row: Option<(String, String, String)> = c
        .prepare_cached(
            "SELECT t.grant_id, g.tenant_id, t.hash FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id JOIN tenants n ON n.id = g.tenant_id
             WHERE t.hash = ? AND t.kind = 'access' AND t.expires_at > ? AND g.revoked_at IS NULL AND n.status = 'active'",
        )?
        .query_row(params![h, now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    Ok(match row {
        Some((grant_id, tenant_id, stored)) if ct_eq(&stored, &h) => {
            c.execute("UPDATE oauth_grants SET last_used_at = ? WHERE id = ?", params![now(), grant_id])?;
            Some(Hit { grant_id, tenant_id })
        }
        _ => None,
    })
}

/// Revoke a tenant's grant ("Connected apps" in the dashboard): its tokens and codes die.
pub fn revoke_grant(c: &Connection, tenant_id: &str, grant_id: &str) -> Result<bool> {
    let n = c.execute(
        "UPDATE oauth_grants SET revoked_at = ? WHERE id = ? AND tenant_id = ? AND revoked_at IS NULL",
        params![now(), grant_id, tenant_id],
    )?;
    if n > 0 {
        c.execute("DELETE FROM oauth_tokens WHERE grant_id = ?", [grant_id])?;
        c.execute("DELETE FROM oauth_codes WHERE grant_id = ?", [grant_id])?;
    }
    Ok(n > 0)
}

/// RFC 7009: revoking a refresh token also ends the access tokens of its grant.
pub fn revoke_token(c: &Connection, token: &str) -> Result<()> {
    let h = sha256_hex(token);
    let row: Option<(String, String)> = c
        .query_row("SELECT grant_id, kind FROM oauth_tokens WHERE hash = ?", [&h], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    match row {
        Some((grant, kind)) if kind == "refresh" => {
            c.execute("DELETE FROM oauth_tokens WHERE grant_id = ?", [grant])?;
        }
        Some(_) => {
            c.execute("DELETE FROM oauth_tokens WHERE hash = ?", [h])?;
        }
        None => {}
    }
    Ok(())
}

pub struct GrantRow {
    pub id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// The tenant's connected apps (live grants).
pub fn grants(c: &Connection, tenant_id: &str) -> Result<Vec<GrantRow>> {
    crate::db::rows(
        c,
        "SELECT g.id, k.name, g.redirect_uri, g.created_at, g.last_used_at FROM oauth_grants g JOIN oauth_clients k ON k.id = g.client_id
         WHERE g.tenant_id = ? AND g.revoked_at IS NULL ORDER BY g.created_at",
        [tenant_id],
        |r| {
            Ok(GrantRow {
                id: r.get(0)?,
                client_name: r.get(1)?,
                redirect_uri: r.get(2)?,
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIER: &str = "siar-test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz";
    const CHALLENGE: &str = "3gg9_IUZwoQvg_dMdQ5tFis5A_Y6WoIhZ_XIqmVDea4";
    const CLAUDE: &str = "https://claude.ai/api/mcp/auth_callback";

    fn db() -> Connection {
        let path = std::env::temp_dir().join(format!("siar-authz-{}.sqlite", crate::util::new_id("t")));
        let c = crate::control::open_at(&path).unwrap();
        c.execute(
            "INSERT INTO tenants (id, name, data_key, status, created_at) VALUES ('t1', 'T1', x'00', 'active', 0), ('t2', 'T2', x'00', 'active', 0)",
            [],
        )
        .unwrap();
        c
    }

    fn code_for(c: &Connection, client: &Client) -> String {
        authorize(c, "t1", &client.id, CLAUDE, CHALLENGE, Some("https://app.example/mcp")).unwrap()
    }

    #[test]
    fn pkce_s256() {
        assert!(pkce_ok(VERIFIER, CHALLENGE));
        assert!(!pkce_ok(VERIFIER, "3gg9_IUZwoQvg_dMdQ5tFis5A_Y6WoIhZ_XIqmVDea5"));
        assert!(!pkce_ok("short", CHALLENGE), "verifiers have at least 43 characters");
        assert!(!pkce_ok(&format!("{VERIFIER}!"), CHALLENGE), "only unreserved characters");
        assert!(!pkce_ok(VERIFIER, ""));
        assert!(challenge_ok(CHALLENGE));
        assert!(!challenge_ok("plain-challenge"));
    }

    #[test]
    fn redirect_uris() {
        for ok in [
            CLAUDE,
            "https://claude.com/api/mcp/auth_callback",
            "https://chatgpt.com/connector_platform_oauth_redirect",
            "http://localhost:6274/oauth/callback",
            "http://127.0.0.1:33418/callback",
            "http://[::1]:8080/cb",
        ] {
            assert!(redirect_uri_ok(ok), "{ok}");
        }
        for bad in [
            "http://example.com/cb",
            "http://localhost.evil.com/cb",
            "https://a.example/cb#frag",
            "https://user@evil.example/cb",
            "javascript:alert(1)",
            "cursor://callback",
            "https://",
            "https://a.example/ cb",
        ] {
            assert!(!redirect_uri_ok(bad), "{bad}");
        }
        assert_eq!(redirect_host(CLAUDE), "claude.ai");
        assert_eq!(clean_name("  Claude\u{0007}\n "), "Claude");
        assert_eq!(clean_name(""), "MCP client");
    }

    #[test]
    fn metadata_document_clients() {
        let url = "https://claude.ai/oauth/claude-code-client-metadata";
        assert!(is_metadata_url(url));
        assert!(!is_metadata_url("https://claude.ai"), "needs a path");
        assert!(!is_metadata_url("mcp_0123"));
        let doc = serde_json::json!({ "client_id": url, "client_name": "Claude Code", "redirect_uris": ["http://localhost:3118/callback"] });
        let cl = client_from_metadata(url, &doc).unwrap();
        assert_eq!((cl.name.as_str(), cl.redirect_uris.len()), ("Claude Code", 1));
        let other = serde_json::json!({ "client_id": "https://evil.example/meta", "redirect_uris": ["http://localhost/cb"] });
        assert!(client_from_metadata(url, &other).is_err(), "the document must name its own URL");
        let bad = serde_json::json!({ "client_id": url, "redirect_uris": ["http://evil.example/cb"] });
        assert!(client_from_metadata(url, &bad).is_err());
        let c = db();
        save_client(&c, &cl).unwrap();
        save_client(&c, &cl).unwrap();
        let code = authorize(&c, "t1", url, "http://localhost:3118/callback", CHALLENGE, None).unwrap();
        assert!(exchange_code(&c, url, &code, "http://localhost:3118/callback", VERIFIER).unwrap().is_ok());
    }

    #[test]
    fn code_flow_and_reuse() {
        let c = db();
        assert!(register(&c, "x", &[]).is_err());
        assert!(register(&c, "x", &["http://evil.example/cb".into()]).is_err());
        let client = register(&c, "Claude", &[CLAUDE.into()]).unwrap();
        assert_eq!(super::client(&c, &client.id).unwrap().unwrap().redirect_uris, vec![CLAUDE.to_string()]);

        // Wrong verifier, wrong client, wrong redirect URI: refused, and the code stays usable.
        let code = code_for(&c, &client);
        assert_eq!(
            exchange_code(&c, &client.id, &code, CLAUDE, &VERIFIER.replace('0', "1"))
                .unwrap()
                .err()
                .unwrap()
                .error,
            "invalid_grant"
        );
        let other = register(&c, "Other", &[CLAUDE.into()]).unwrap();
        assert!(exchange_code(&c, &other.id, &code, CLAUDE, VERIFIER).unwrap().is_err());
        let e = exchange_code(&c, &client.id, &code, "https://claude.com/api/mcp/auth_callback", VERIFIER)
            .unwrap()
            .err()
            .unwrap();
        assert!(e.description.contains("redirect_uri"), "{e:?}");

        let t = exchange_code(&c, &client.id, &code, CLAUDE, VERIFIER).unwrap().ok().unwrap();
        assert!(t.access_token.starts_with(ACCESS_PREFIX) && t.expires_in == 3600);
        let hit = find_access(&c, &t.access_token).unwrap().unwrap();
        assert_eq!(hit.tenant_id, "t1");
        // Only hashes are stored.
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM oauth_tokens WHERE hash = ?", [&t.access_token], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        // The code works once; a replay is refused and ends the tokens issued from it.
        let e = exchange_code(&c, &client.id, &code, CLAUDE, VERIFIER).unwrap().err().unwrap();
        assert!(e.description.contains("already used"));
        assert!(find_access(&c, &t.access_token).unwrap().is_none(), "replay revokes the tokens");

        // Expired codes are refused.
        let code = code_for(&c, &client);
        c.execute("UPDATE oauth_codes SET expires_at = 0 WHERE hash = ?", [sha256_hex(&code)]).unwrap();
        assert!(exchange_code(&c, &client.id, &code, CLAUDE, VERIFIER)
            .unwrap()
            .err()
            .unwrap()
            .description
            .contains("expired"));
    }

    #[test]
    fn refresh_rotation_and_revocation() {
        let c = db();
        let client = register(&c, "Claude", &[CLAUDE.into()]).unwrap();
        let code = code_for(&c, &client);
        let first = exchange_code(&c, &client.id, &code, CLAUDE, VERIFIER).unwrap().ok().unwrap();

        let second = refresh(&c, &client.id, &first.refresh_token).unwrap().ok().unwrap();
        assert_ne!(second.refresh_token, first.refresh_token);
        assert!(find_access(&c, &second.access_token).unwrap().is_some());
        let e = refresh(&c, &client.id, &first.refresh_token).unwrap().err().unwrap();
        assert!(e.description.contains("already used"), "rotated tokens work once");
        let other = register(&c, "Other", &[CLAUDE.into()]).unwrap();
        assert!(refresh(&c, &other.id, &second.refresh_token).unwrap().is_err(), "bound to its client");
        assert!(refresh(&c, &client.id, "siar_rt_nope").unwrap().is_err());

        // Another tenant cannot revoke it; the owner can, and every token of the grant dies.
        let grant = grants(&c, "t1").unwrap();
        assert_eq!(grant.len(), 1);
        assert_eq!(grant[0].client_name, "Claude");
        assert!(!revoke_grant(&c, "t2", &grant[0].id).unwrap());
        assert!(find_access(&c, &second.access_token).unwrap().is_some());
        let pending = code_for(&c, &client);
        assert!(revoke_grant(&c, "t1", &grant[0].id).unwrap());
        assert!(find_access(&c, &second.access_token).unwrap().is_none());
        assert!(refresh(&c, &client.id, &second.refresh_token).unwrap().is_err());
        assert!(grants(&c, "t1").unwrap().is_empty());
        assert!(
            exchange_code(&c, &client.id, &pending, CLAUDE, VERIFIER).unwrap().is_err(),
            "a code issued before the revocation is dead too"
        );
        assert!(
            exchange_code(&c, &client.id, &code_for(&c, &client), CLAUDE, VERIFIER).unwrap().is_ok(),
            "a new consent makes a new grant"
        );

        // Expired access tokens and suspended tenants are refused.
        let t = exchange_code(&c, &client.id, &code_for(&c, &client), CLAUDE, VERIFIER).unwrap().ok().unwrap();
        c.execute("UPDATE tenants SET status = 'suspended' WHERE id = 't1'", []).unwrap();
        assert!(find_access(&c, &t.access_token).unwrap().is_none());
        c.execute("UPDATE tenants SET status = 'active' WHERE id = 't1'", []).unwrap();
        assert!(find_access(&c, &t.access_token).unwrap().is_some());
        c.execute("UPDATE oauth_tokens SET expires_at = 0 WHERE kind = 'access'", []).unwrap();
        assert!(find_access(&c, &t.access_token).unwrap().is_none());

        // RFC 7009 revocation of a refresh token.
        revoke_token(&c, &t.refresh_token).unwrap();
        assert!(refresh(&c, &client.id, &t.refresh_token).unwrap().is_err());
    }
}
