//! OAuth start and callback (src/oauth.ts).

use base64::Engine;
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::save_accounts;
use crate::config::{app_creds, env, redirect_uri};
use crate::db;
use crate::platforms;
use crate::providers;
use crate::util::{bail, now, query_string, random_bytes, Result};

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn start_oauth(platform: &str) -> Result<String> {
    let p = platforms::get(platform)?;
    if p.oauth_env().is_none() {
        bail!("{} connects with credentials, not OAuth", p.name);
    }
    if !p.ported() {
        return Err(p.not_ported());
    }
    let spec = providers::oauth_spec(platform)?;
    let (id, _) = app_creds(spec.env)?;
    let state = b64url(&random_bytes(18));
    let verifier = b64url(&random_bytes(48));
    let config_id = spec.config_id_env.and_then(env);
    if let (Some(e), None) = (spec.config_id_env, &config_id) {
        bail!("Set {e} (the login configuration id from the Meta app) to connect this platform");
    }
    db::with(|c| {
        c.execute("DELETE FROM oauth_states WHERE created_at < ?", [now() - 30 * 60_000])?;
        c.execute(
            "INSERT INTO oauth_states (state, platform, verifier, data, created_at) VALUES (?, ?, ?, ?, ?)",
            params![state, platform, verifier, "{}", now()],
        )?;
        Ok(())
    })?;
    let mut q: Vec<(&str, String)> = vec![("client_id", id), ("redirect_uri", redirect_uri(platform)), ("response_type", "code".into())];
    match config_id {
        Some(cid) => {
            q.push(("config_id", cid));
            q.push(("override_default_response_type", "true".into()));
        }
        None => q.push(("scope", spec.scopes.join(spec.scope_sep))),
    }
    q.push(("state", state));
    if spec.pkce {
        q.push(("code_challenge", b64url(&Sha256::digest(verifier.as_bytes()))));
        q.push(("code_challenge_method", "S256".into()));
    }
    for (k, v) in &spec.extra_auth_params {
        q.push((k, v.to_string()));
    }
    Ok(format!("{}?{}", spec.auth_url, query_string(&q)))
}

pub fn finish_oauth(platform: &str, code: &str, state: &str) -> Result<Vec<Value>> {
    let row = db::with(|c| {
        db::opt_row(
            c,
            "SELECT verifier FROM oauth_states WHERE state = ? AND platform = ?",
            params![state, platform],
            |r| r.get::<_, String>(0),
        )
    })?;
    let Some(verifier) = row else {
        bail!("Unknown or expired OAuth state. Start again with connect_account.")
    };
    db::with(|c| {
        c.execute("DELETE FROM oauth_states WHERE state = ?", [state])?;
        Ok(())
    })?;
    let p = platforms::get(platform)?;
    if p.oauth_env().is_none() {
        bail!("{} has no OAuth flow", p.name);
    }
    let tokens = providers::exchange(platform, code, &verifier)?;
    let accounts = providers::profile(platform, &tokens)?;
    save_accounts(platform, &accounts)
}
