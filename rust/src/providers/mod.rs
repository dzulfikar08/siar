//! Platform API calls. Dispatch is a match on the platform id; platforms.rs (generated from the
//! TS providers) says which capabilities each platform has, and callers check it first.

use serde_json::{Map, Value};
use std::time::{Duration, Instant};

use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, redirect_uri};
use crate::net;
use crate::util::{bail, js_string, now, some, str_of, to_num, Result};

pub mod blogs;
pub mod bluesky;
pub mod chat;
pub mod decentral;
pub mod facebook;
pub mod gbp;
pub mod instagram;
pub mod linkedin;
pub mod mastodon;
pub mod pinterest;
pub mod reddit;
pub mod threads;
pub mod tiktok;
pub mod x;
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

#[derive(Clone)]
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

pub struct Conversation {
    pub id: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub updated_at: i64,
    pub last_message: Option<String>,
}

pub struct DirectMessage {
    pub id: String,
    pub from_id: Option<String>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub created_at: i64,
    pub mine: bool,
    /// `{kind: "reply" | "mention", url?}`
    pub story: Option<Value>,
    pub attachments: Vec<String>,
}

#[derive(Clone)]
pub struct Button {
    pub title: String,
    pub url: Option<String>,
    pub payload: Option<String>,
}

#[derive(Clone)]
pub struct DmPayload {
    pub text: String,
    pub buttons: Vec<Button>,
}

impl DmPayload {
    /// From the JSON stored in jobs (`{text, buttons: [{title, url?, payload?}]}`).
    pub fn from_json(v: &Value) -> DmPayload {
        DmPayload {
            text: v.get("text").map(js_string).unwrap_or_default(),
            buttons: v
                .get("buttons")
                .and_then(|b| b.as_array())
                .into_iter()
                .flatten()
                .map(|b| Button {
                    title: b.get("title").map(js_string).unwrap_or_default(),
                    url: opt_string(b.get("url")),
                    payload: opt_string(b.get("payload")),
                })
                .collect(),
        }
    }
    pub fn to_json(&self) -> Value {
        let buttons: Vec<Value> = self
            .buttons
            .iter()
            .map(|b| {
                let mut o = Map::new();
                o.insert("title".into(), b.title.clone().into());
                put(&mut o, "url", b.url.clone().map(Value::from));
                put(&mut o, "payload", b.payload.clone().map(Value::from));
                Value::Object(o)
            })
            .collect();
        let mut o = Map::new();
        o.insert("text".into(), self.text.clone().into());
        o.insert("buttons".into(), Value::Array(buttons));
        Value::Object(o)
    }
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
    /// Client credentials as HTTP basic auth instead of form fields.
    pub client_auth_basic: bool,
    /// Name of the client id parameter (TikTok calls it client_key).
    pub client_id_param: &'static str,
}

/// A capability platforms.rs says exists but this dispatch has no arm for (a bug).
fn missing(platform: &str, what: &str) -> crate::util::Error {
    crate::util::Error::Msg(format!("{platform} has no {what} implementation"))
}

/// The Instagram host for each Instagram variant, and whether DMs go through the Page.
fn ig(platform: &str) -> Option<(String, bool)> {
    match platform {
        "instagram" => Some((instagram::host(), false)),
        "instagram-login" => Some((instagram::login_host(), false)),
        "instagram-facebook" => Some((facebook::fb_host(), true)),
        _ => None,
    }
}

pub fn oauth_spec(platform: &str) -> Result<OAuthSpec> {
    Ok(match platform {
        "instagram" => instagram::spec(),
        "instagram-login" => instagram::login_spec(),
        "instagram-facebook" => facebook::ig_spec(),
        "facebook" => facebook::facebook_spec(),
        "threads" => threads::spec(),
        "linkedin" => linkedin::personal_spec(),
        "linkedin-page" => linkedin::page_spec(),
        "youtube" => youtube::spec(),
        "google-business" => gbp::spec(),
        "x" => x::spec(),
        "tiktok" => tiktok::spec(),
        "pinterest" => pinterest::spec(),
        "reddit" => reddit::spec(),
        p => return Err(missing(p, "OAuth")),
    })
}

pub fn exchange(platform: &str, code: &str, verifier: &str) -> Result<Value> {
    match platform {
        "instagram" => instagram::exchange(code),
        "instagram-login" => instagram::login_exchange(code),
        "threads" => threads::exchange(code),
        "facebook" => facebook::fb_exchange("FACEBOOK", "facebook", code),
        "instagram-facebook" => facebook::fb_exchange("FACEBOOK", "instagram-facebook", code),
        p => exchange_code(&oauth_spec(p)?, p, code, verifier),
    }
}

pub fn profile(platform: &str, tokens: &Value) -> Result<Vec<Connected>> {
    match platform {
        "instagram" => instagram::profile(tokens),
        "instagram-login" => instagram::login_profile(tokens),
        "instagram-facebook" => facebook::ig_profile(tokens),
        "facebook" => facebook::profile(tokens),
        "threads" => threads::profile(tokens),
        "linkedin" => linkedin::profile(tokens),
        "linkedin-page" => linkedin::page_profile(tokens),
        "youtube" => youtube::profile(tokens),
        "google-business" => gbp::profile(tokens),
        "x" => x::profile(tokens),
        "tiktok" => tiktok::profile(tokens),
        "pinterest" => pinterest::profile(tokens),
        "reddit" => reddit::profile(tokens),
        p => Err(missing(p, "OAuth profile")),
    }
}

/// Credential platforms: check the fields against the platform and describe the account.
pub fn verify(platform: &str, fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    match platform {
        "bluesky" => bluesky::verify(fields),
        "mastodon" => mastodon::verify(fields),
        "nostr" => decentral::nostr_verify(fields),
        "farcaster" => decentral::farcaster_verify(fields),
        "lemmy" => blogs::lemmy_verify(fields),
        "devto" => blogs::devto_verify(fields),
        "hashnode" => blogs::hashnode_verify(fields),
        "medium" => blogs::medium_verify(fields),
        "wordpress" => blogs::wordpress_verify(fields),
        "telegram" => chat::telegram_verify(fields),
        "discord" => chat::discord_verify(fields),
        "slack" => chat::slack_verify(fields),
        p => Err(missing(p, "credential check")),
    }
}

/// Some((new secret, expires_at)) when refreshed; None when there is nothing to refresh.
pub fn refresh(platform: &str, secret: &Value) -> Result<Option<(Value, Option<i64>)>> {
    match platform {
        "threads" => threads::refresh(secret).map(Some),
        "instagram-login" => instagram::login_refresh(secret).map(Some),
        "linkedin" | "linkedin-page" | "youtube" | "google-business" | "x" | "tiktok" | "pinterest" | "reddit" => refresh_token(&oauth_spec(platform)?, secret),
        _ => Ok(None),
    }
}

pub fn publish(platform: &str, ctx: &Ctx) -> Result<Published> {
    if let Some((host, _)) = ig(platform) {
        return instagram::publish(&host, ctx);
    }
    match platform {
        "facebook" => facebook::publish(ctx),
        "threads" => threads::publish(ctx),
        "linkedin" | "linkedin-page" => linkedin::publish(ctx),
        "youtube" => youtube::publish(ctx),
        "google-business" => gbp::publish(ctx),
        "x" => x::publish(ctx),
        "tiktok" => tiktok::publish(ctx),
        "pinterest" => pinterest::publish(ctx),
        "reddit" => reddit::publish(ctx),
        "bluesky" => bluesky::publish(ctx),
        "mastodon" => mastodon::publish(ctx),
        "nostr" => decentral::nostr_publish(ctx),
        "farcaster" => decentral::farcaster_publish(ctx),
        "lemmy" => blogs::lemmy_publish(ctx),
        "devto" => blogs::devto_publish(ctx),
        "hashnode" => blogs::hashnode_publish(ctx),
        "medium" => blogs::medium_publish(ctx),
        "wordpress" => blogs::wordpress_publish(ctx),
        "telegram" => chat::telegram_publish(ctx),
        "discord" => chat::discord_publish(ctx),
        "slack" => chat::slack_publish(ctx),
        p => Err(missing(p, "publish")),
    }
}

pub fn remove(platform: &str, ctx: &Ctx, id: &str) -> Result<()> {
    match platform {
        "instagram" | "instagram-login" => instagram::remove(&ig(platform).map(|x| x.0).unwrap_or_default(), ctx, id),
        "facebook" => facebook::remove(ctx, id),
        "threads" => threads::remove(ctx, id),
        "linkedin" | "linkedin-page" => linkedin::remove(ctx, id),
        "youtube" => youtube::remove(ctx, id),
        "google-business" => gbp::remove(ctx, id),
        "x" => x::remove(ctx, id),
        "pinterest" => pinterest::remove(ctx, id),
        "reddit" => reddit::remove(ctx, id),
        "bluesky" => bluesky::remove(ctx, id),
        "mastodon" => mastodon::remove(ctx, id),
        "nostr" => decentral::nostr_remove(ctx, id),
        "farcaster" => decentral::farcaster_remove(ctx, id),
        "wordpress" => blogs::wordpress_remove(ctx, id),
        "telegram" => chat::telegram_remove(ctx, id),
        "discord" => chat::discord_remove(ctx, id),
        p => Err(missing(p, "delete")),
    }
}

pub fn metrics(platform: &str, ctx: &Ctx, id: &str) -> Result<Value> {
    if let Some((host, _)) = ig(platform) {
        return instagram::media_insights(&host, &ctx.token(), id, None);
    }
    match platform {
        "facebook" => facebook::metrics(ctx, id),
        "threads" => threads::insights(&ctx.token(), id),
        "linkedin" => linkedin::social_metrics(ctx, id),
        "linkedin-page" => linkedin::page_metrics(ctx, id),
        "youtube" => youtube::metrics(ctx, id),
        "x" => x::metrics(ctx, id),
        "tiktok" => tiktok::metrics(ctx, id),
        "pinterest" => pinterest::metrics(ctx, id),
        "reddit" => reddit::metrics(ctx, id),
        "bluesky" => bluesky::metrics(ctx, id),
        "mastodon" => mastodon::metrics(ctx, id),
        "farcaster" => decentral::farcaster_metrics(ctx, id),
        "lemmy" => blogs::lemmy_metrics(ctx, id),
        "devto" => blogs::devto_metrics(ctx, id),
        "hashnode" => blogs::hashnode_metrics(ctx, id),
        p => Err(missing(p, "metrics")),
    }
}

pub fn account_metrics(platform: &str, ctx: &Ctx) -> Result<Value> {
    if let Some((host, _)) = ig(platform) {
        return instagram::account_metrics(&host, ctx);
    }
    match platform {
        "facebook" => facebook::account_metrics(ctx),
        "threads" => threads::account_metrics(ctx),
        "linkedin-page" => linkedin::page_account_metrics(ctx),
        "youtube" => youtube::account_metrics(ctx),
        "x" => x::account_metrics(ctx),
        "tiktok" => tiktok::account_metrics(ctx),
        "pinterest" => pinterest::account_metrics(ctx),
        "reddit" => reddit::account_metrics(ctx),
        "bluesky" => bluesky::account_metrics(ctx),
        "mastodon" => mastodon::account_metrics(ctx),
        "telegram" => chat::telegram_account_metrics(ctx),
        p => Err(missing(p, "account metrics")),
    }
}

pub fn info(platform: &str, ctx: &Ctx) -> Result<Value> {
    match platform {
        "tiktok" => tiktok::info(ctx),
        "pinterest" => pinterest::info(ctx),
        p => Err(missing(p, "info")),
    }
}

pub fn posts(platform: &str, ctx: &Ctx, since: i64, limit: usize, with_metrics: bool) -> Result<Vec<RemotePost>> {
    if let Some((host, _)) = ig(platform) {
        return instagram::posts(&host, ctx, since, limit, with_metrics);
    }
    match platform {
        "facebook" => facebook::posts(ctx, since, limit),
        "threads" => threads::posts(ctx, since, limit, with_metrics),
        "youtube" => youtube::posts(ctx, since, limit),
        p => Err(missing(p, "post list")),
    }
}

pub fn stories(platform: &str, ctx: &Ctx) -> Result<Vec<RemotePost>> {
    match ig(platform) {
        Some((host, _)) => instagram::stories(&host, ctx),
        None => Err(missing(platform, "stories")),
    }
}

pub fn publishing_limit(platform: &str, ctx: &Ctx) -> Result<Value> {
    if let Some((host, _)) = ig(platform) {
        return instagram::publishing_limit(&host, ctx);
    }
    match platform {
        "threads" => threads::publishing_limit(ctx),
        p => Err(missing(p, "publishing limit")),
    }
}

pub fn list_comments(platform: &str, ctx: &Ctx, post_id: &str) -> Result<Vec<RemoteComment>> {
    if let Some((host, _)) = ig(platform) {
        return instagram::comments(&host, ctx, post_id);
    }
    match platform {
        "facebook" => facebook::comments(ctx, post_id),
        "threads" => threads::comments(ctx, post_id),
        "youtube" => youtube::comments(ctx, post_id),
        p => Err(missing(p, "comments")),
    }
}

/// `{id}` of the reply.
pub fn comment_reply(platform: &str, ctx: &Ctx, comment_id: &str, text: &str) -> Result<Value> {
    if let Some((host, _)) = ig(platform) {
        return instagram::comment_reply(&host, ctx, comment_id, text);
    }
    match platform {
        "facebook" => facebook::comment_reply(ctx, comment_id, text),
        "threads" => threads::comment_reply(ctx, comment_id, text),
        "youtube" => youtube::comment_reply(ctx, comment_id, text),
        p => Err(missing(p, "comment replies")),
    }
}

/// Platforms with a hide / like / delete call for comments (TS comments.hide / .like / .remove).
pub fn comment_can(platform: &str, action: &str) -> bool {
    let ig = platform.starts_with("instagram");
    match action {
        "hide" | "remove" => ig || matches!(platform, "facebook" | "threads" | "youtube"),
        "like" => platform == "facebook",
        _ => false,
    }
}

pub fn comment_hide(platform: &str, ctx: &Ctx, comment_id: &str, hidden: bool) -> Result<()> {
    if let Some((host, _)) = ig(platform) {
        return instagram::comment_hide(&host, ctx, comment_id, hidden);
    }
    match platform {
        "facebook" => facebook::comment_hide(ctx, comment_id, hidden),
        "threads" => threads::comment_hide(ctx, comment_id, hidden),
        "youtube" => youtube::comment_hide(ctx, comment_id, hidden),
        p => Err(missing(p, "comment hiding")),
    }
}

pub fn comment_like(platform: &str, ctx: &Ctx, comment_id: &str) -> Result<()> {
    match platform {
        "facebook" => facebook::comment_like(ctx, comment_id),
        p => Err(missing(p, "comment likes")),
    }
}

pub fn comment_remove(platform: &str, ctx: &Ctx, comment_id: &str) -> Result<()> {
    if let Some((host, _)) = ig(platform) {
        return instagram::comment_remove(&host, ctx, comment_id);
    }
    match platform {
        "facebook" => facebook::comment_remove(ctx, comment_id),
        "threads" => threads::comment_remove(ctx, comment_id),
        "youtube" => youtube::comment_remove(ctx, comment_id),
        p => Err(missing(p, "comment deletion")),
    }
}

// ---------- DMs (Instagram variants) ----------

fn dm(platform: &str) -> Result<(String, bool)> {
    ig(platform).ok_or_else(|| missing(platform, "DMs"))
}

pub fn dm_conversations(platform: &str, ctx: &Ctx, limit: i64) -> Result<Vec<Conversation>> {
    let (host, page) = dm(platform)?;
    instagram::conversations(&host, ctx, page, limit)
}

pub fn dm_messages(platform: &str, ctx: &Ctx, conversation_id: &str, limit: i64) -> Result<Vec<DirectMessage>> {
    let (host, _) = dm(platform)?;
    instagram::messages(&host, ctx, conversation_id, limit)
}

/// `{id, recipientId}`
pub fn dm_send(platform: &str, ctx: &Ctx, to_id: Option<&str>, comment_id: Option<&str>, msg: &DmPayload) -> Result<Value> {
    let (host, page) = dm(platform)?;
    instagram::send(&host, ctx, page, to_id, comment_id, msg)
}

pub fn dm_react(platform: &str, ctx: &Ctx, user_id: &str, message_id: &str) -> Result<()> {
    let (host, page) = dm(platform)?;
    instagram::react(&host, ctx, page, user_id, message_id)
}

pub fn dm_follows(platform: &str, ctx: &Ctx, user_id: &str) -> Result<Option<bool>> {
    let (host, _) = dm(platform)?;
    instagram::follows(&host, ctx, user_id)
}

pub fn dm_profile(platform: &str, ctx: &Ctx, user_id: &str) -> Result<(Option<String>, Option<String>, Option<bool>)> {
    let (host, _) = dm(platform)?;
    instagram::user_profile(&host, ctx, user_id)
}

// ---------- Threads discovery ----------

pub fn search(platform: &str, ctx: &Ctx, q: &str, kind: Option<&str>, limit: i64) -> Result<Vec<RemotePost>> {
    match platform {
        "threads" => threads::search(ctx, q, kind, limit),
        p => Err(missing(p, "search")),
    }
}

pub fn context(platform: &str, ctx: &Ctx, id: &str) -> Result<(RemotePost, Vec<RemoteComment>)> {
    match platform {
        "threads" => threads::context(ctx, id),
        p => Err(missing(p, "post context")),
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

/// The token request with the client credentials, as basic auth or in the form (src/grant.ts clientParams).
type TokenForm = Vec<(&'static str, Option<String>)>;

fn client_params(spec: &OAuthSpec) -> Result<(net::Req, TokenForm)> {
    let (id, secret) = app_creds(spec.env)?;
    let mut rq = net::post(&spec.token_url).header("accept", "application/json");
    let mut form = vec![];
    if spec.client_auth_basic {
        rq = rq.basic(&id, &secret);
    } else {
        form.push((spec.client_id_param, Some(id)));
        if !secret.is_empty() {
            form.push(("client_secret", Some(secret)));
        }
    }
    Ok((rq, form))
}

/// Token endpoint: authorization_code grant (src/grant.ts exchangeCode).
pub fn exchange_code(spec: &OAuthSpec, platform: &str, code: &str, verifier: &str) -> Result<Value> {
    let (rq, mut form) = client_params(spec)?;
    form.push(("grant_type", Some("authorization_code".into())));
    form.push(("code", Some(code.to_string())));
    form.push(("redirect_uri", Some(redirect_uri(platform))));
    if spec.pkce {
        form.push(("code_verifier", Some(verifier.to_string())));
    }
    rq.form(&form).get_json()
}

/// Standard refresh_token grant; keeps the old refresh token when the platform doesn't rotate it.
pub fn refresh_token(spec: &OAuthSpec, secret: &Value) -> Result<Option<(Value, Option<i64>)>> {
    let Some(rt) = str_of(secret, "refresh_token").filter(|s| !s.is_empty()).map(|s| s.to_string()) else {
        return Ok(None);
    };
    let (rq, mut form) = client_params(spec)?;
    form.push(("grant_type", Some("refresh_token".into())));
    form.push(("refresh_token", Some(rt.clone())));
    let t = rq.form(&form).get_json()?;
    let mut s = secret.as_object().cloned().unwrap_or_default();
    s.insert("access_token".into(), t.get("access_token").cloned().unwrap_or(Value::Null));
    s.insert("refresh_token".into(), some(t.get("refresh_token")).cloned().unwrap_or(Value::from(rt)));
    Ok(Some((Value::Object(s), expires_at(&t))))
}
