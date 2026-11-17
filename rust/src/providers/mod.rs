//! Platform API calls for the ported platforms. Dispatch is a match on the platform id;
//! platforms.rs decides which capabilities exist and which are ported.

use serde_json::{Map, Value};
use std::time::{Duration, Instant};

use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, redirect_uri};
use crate::net;
use crate::platforms;
use crate::util::{bail, js_string, now, some, str_of, to_num, Error, Obj, Result};

pub mod instagram;
pub mod linkedin;
pub mod threads;
pub mod youtube;

pub struct Published {
    pub id: String,
    pub url: Option<String>,
}

/// A post on the account (also ones made in the platform app).
pub struct RemotePost {
    pub id: String,
    pub url: Option<String>,
    pub text: Option<String>,
    pub kind: Option<String>,
    pub username: Option<String>,
    pub created_at: i64,
    pub metrics: Option<Value>,
}

pub struct RemoteComment {
    pub id: String,
    pub post_id: String,
    pub parent_id: Option<String>,
    pub text: String,
    pub author_id: Option<String>,
    pub username: Option<String>,
    pub created_at: i64,
    pub hidden: bool,
    pub mine: bool,
}

pub struct OAuthSpec {
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<&'static str>,
    pub env: &'static str,
    pub pkce: bool,
    pub scope_sep: &'static str,
    pub extra_auth_params: Vec<(&'static str, &'static str)>,
    pub config_id_env: Option<&'static str>,
}

fn not_ported(platform: &str) -> Error {
    match platforms::get(platform) {
        Ok(p) => p.not_ported(),
        Err(e) => e,
    }
}

pub fn oauth_spec(platform: &str) -> Result<OAuthSpec> {
    Ok(match platform {
        "instagram" => instagram::spec(),
        "threads" => threads::spec(),
        "linkedin" => linkedin::personal_spec(),
        "linkedin-page" => linkedin::page_spec(),
        "youtube" => youtube::spec(),
        p => return Err(not_ported(p)),
    })
}

pub fn exchange(platform: &str, code: &str, verifier: &str) -> Result<Value> {
    match platform {
        "instagram" => instagram::exchange(code),
        "threads" => threads::exchange(code),
        p => exchange_code(&oauth_spec(p)?, p, code, verifier),
    }
}

pub fn profile(platform: &str, tokens: &Value) -> Result<Vec<Connected>> {
    match platform {
        "instagram" => instagram::profile(tokens),
        "threads" => threads::profile(tokens),
        "linkedin" => linkedin::profile(tokens),
        "linkedin-page" => linkedin::page_profile(tokens),
        "youtube" => youtube::profile(tokens),
        p => Err(not_ported(p)),
    }
}

/// Some((new secret, expires_at)) when refreshed; None when the platform's tokens never expire.
pub fn refresh(platform: &str, secret: &Value) -> Result<Option<(Value, Option<i64>)>> {
    match platform {
        "threads" => threads::refresh(secret).map(Some),
        "linkedin" => refresh_token(&linkedin::personal_spec(), secret),
        "linkedin-page" => refresh_token(&linkedin::page_spec(), secret),
        "youtube" => refresh_token(&youtube::spec(), secret),
        "instagram" => Ok(None),
        p => Err(not_ported(p)),
    }
}

pub fn publish(platform: &str, ctx: &Ctx) -> Result<Published> {
    match platform {
        "instagram" => instagram::publish(&instagram::host(), ctx),
        "threads" => threads::publish(ctx),
        "linkedin" | "linkedin-page" => linkedin::publish(ctx),
        "youtube" => youtube::publish(ctx),
        p => Err(not_ported(p)),
    }
}

pub fn remove(platform: &str, ctx: &Ctx, id: &str) -> Result<()> {
    match platform {
        "instagram" => instagram::remove(&instagram::host(), ctx, id),
        "threads" => threads::remove(ctx, id),
        "linkedin" | "linkedin-page" => linkedin::remove(ctx, id),
        "youtube" => youtube::remove(ctx, id),
        p => Err(not_ported(p)),
    }
}

pub fn metrics(platform: &str, ctx: &Ctx, id: &str) -> Result<Value> {
    match platform {
        "instagram" => instagram::media_insights(&instagram::host(), &ctx.token(), id, None),
        "threads" => threads::insights(&ctx.token(), id),
        "linkedin" => linkedin::social_metrics(ctx, id),
        "linkedin-page" => linkedin::page_metrics(ctx, id),
        "youtube" => youtube::metrics(ctx, id),
        p => Err(not_ported(p)),
    }
}

pub fn account_metrics(platform: &str, ctx: &Ctx) -> Result<Value> {
    match platform {
        "instagram" => instagram::account_metrics(&instagram::host(), ctx),
        "threads" => threads::account_metrics(ctx),
        "linkedin-page" => linkedin::page_account_metrics(ctx),
        "youtube" => youtube::account_metrics(ctx),
        p => Err(not_ported(p)),
    }
}

pub fn posts(platform: &str, ctx: &Ctx, since: i64, limit: usize, with_metrics: bool) -> Result<Vec<RemotePost>> {
    match platform {
        "instagram" => instagram::posts(&instagram::host(), ctx, since, limit, with_metrics),
        "threads" => threads::posts(ctx, since, limit, with_metrics),
        "youtube" => youtube::posts(ctx, since, limit),
        p => Err(not_ported(p)),
    }
}

pub fn publishing_limit(platform: &str, ctx: &Ctx) -> Result<Value> {
    match platform {
        "instagram" => instagram::publishing_limit(&instagram::host(), ctx),
        "threads" => threads::publishing_limit(ctx),
        p => Err(not_ported(p)),
    }
}

pub fn list_comments(platform: &str, ctx: &Ctx, post_id: &str) -> Result<Vec<RemoteComment>> {
    match platform {
        "instagram" => instagram::comments(&instagram::host(), ctx, post_id),
        "threads" => threads::comments(ctx, post_id),
        "youtube" => youtube::comments(ctx, post_id),
        p => Err(not_ported(p)),
    }
}

// ---------- shared helpers ----------

/// Poll `f` until it returns Some, with a cap (src/lib/http.ts poll).
pub fn poll<T>(every_ms: u64, max_ms: u64, what: &str, mut f: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    loop {
        if let Some(v) = f()? {
            return Ok(v);
        }
        if Instant::now() > deadline {
            bail!("Timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(every_ms));
    }
}

/// `Object.fromEntries(data.map(m => [m.name, Number(m.values?.[0]?.value ?? m.total_value?.value ?? 0)]))`
pub fn insight_map(data: Option<&Value>) -> Value {
    let mut out = Map::new();
    for m in data.and_then(|d| d.as_array()).into_iter().flatten() {
        let v = some(m.pointer("/values/0/value")).or(some(m.pointer("/total_value/value")));
        out.insert(js_string(m.get("name").unwrap_or(&Value::Null)), crate::util::jnum(to_num(v)));
    }
    Value::Object(out)
}

/// `a ?? 0` keeping the platform's value as is.
pub fn or_zero(v: Option<&Value>) -> Value {
    some(v).cloned().unwrap_or(Value::from(0))
}

pub fn id_of(v: &Value) -> String {
    v.get("id").map(js_string).unwrap_or_else(|| "undefined".into())
}

pub fn opt_string(v: Option<&Value>) -> Option<String> {
    some(v).map(js_string)
}

/// `Date.now() + Number(t.expires_in) * 1000` when the token response has expires_in.
pub fn expires_at(t: &Value) -> Option<i64> {
    if crate::util::truthy(t.get("expires_in")) {
        Some(now() + (to_num(t.get("expires_in")) * 1000.0) as i64)
    } else {
        None
    }
}

/// Insert defined (non-null) values; `None` / null are left out like undefined in JSON.stringify.
pub fn put(m: &mut Map<String, Value>, k: &str, v: Option<Value>) {
    match v {
        Some(Value::Null) | None => {}
        Some(v) => {
            m.insert(k.to_string(), v);
        }
    }
}

/// Token endpoint: authorization_code grant (src/grant.ts exchangeCode).
pub fn exchange_code(spec: &OAuthSpec, platform: &str, code: &str, verifier: &str) -> Result<Value> {
    let (id, secret) = app_creds(spec.env)?;
    let mut form: Vec<(&str, Option<String>)> = vec![("client_id", Some(id))];
    if !secret.is_empty() {
        form.push(("client_secret", Some(secret)));
    }
    form.push(("grant_type", Some("authorization_code".into())));
    form.push(("code", Some(code.to_string())));
    form.push(("redirect_uri", Some(redirect_uri(platform))));
    if spec.pkce {
        form.push(("code_verifier", Some(verifier.to_string())));
    }
    net::post(&spec.token_url).header("accept", "application/json").form(&form).get_json()
}

/// Standard refresh_token grant; keeps the old refresh token when the platform doesn't rotate it.
pub fn refresh_token(spec: &OAuthSpec, secret: &Value) -> Result<Option<(Value, Option<i64>)>> {
    let Some(rt) = str_of(secret, "refresh_token").filter(|s| !s.is_empty()).map(|s| s.to_string()) else {
        return Ok(None);
    };
    let (id, client_secret) = app_creds(spec.env)?;
    let mut form: Vec<(&str, Option<String>)> = vec![("client_id", Some(id))];
    if !client_secret.is_empty() {
        form.push(("client_secret", Some(client_secret)));
    }
    form.push(("grant_type", Some("refresh_token".into())));
    form.push(("refresh_token", Some(rt.clone())));
    let t = net::post(&spec.token_url).header("accept", "application/json").form(&form).get_json()?;
    let mut s = secret.as_object().cloned().unwrap_or_default();
    s.insert("access_token".into(), t.get("access_token").cloned().unwrap_or(Value::Null));
    s.insert("refresh_token".into(), some(t.get("refresh_token")).cloned().unwrap_or(Value::from(rt)));
    Ok(Some((Value::Object(s), expires_at(&t))))
}

pub fn published_json(p: &Published) -> Value {
    Obj::new().set("id", p.id.clone()).opt("url", p.url.clone()).build()
}
