//! Mastodon (src/providers/mastodon.ts).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::{poll, put, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::local_file;
use crate::net::{self, Part, PartData};
use crate::util::{hex, js_string, str_of, truthy, Result};

fn base(s: &Value) -> String {
    let raw = s.get("instance").map(js_string).unwrap_or_else(|| "undefined".into());
    let b = raw.strip_suffix('/').unwrap_or(&raw);
    if b.starts_with("http://") || b.starts_with("https://") {
        b.to_string()
    } else {
        format!("https://{b}")
    }
}

/// `new URL(base).host` (host with a non-default port).
fn host_with_port(url: &str) -> String {
    crate::util::host_path(url).0.to_ascii_lowercase()
}

pub fn verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let fv = Value::Object(f.clone());
    let b = base(&fv);
    let token = str_of(&fv, "access_token").unwrap_or_default();
    let me = net::get(format!("{b}/api/v1/accounts/verify_credentials")).bearer(token).get_json()?;
    let host = host_with_port(&b);
    let id = me.get("id").map(js_string).unwrap_or_default();
    let username = me.get("username").map(js_string).unwrap_or_default();
    let name = [me.get("display_name"), me.get("username")]
        .into_iter()
        .find(|v| truthy(*v))
        .flatten()
        .map(js_string)
        .unwrap_or_default();
    Ok(vec![Connected {
        external_id: format!("{host}:{id}"),
        name,
        handle: Some(format!("{username}@{host}")),
        secret: json!({ "instance": b, "access_token": f.get("access_token") }),
        meta: Some(json!({ "accountId": me.get("id") })),
        expires_at: None,
    }])
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let b = base(&ctx.secret);
    let t = ctx.token();
    let mut media_ids = vec![];
    for m in &ctx.media {
        let mut parts = vec![Part::file("file", &m.filename, Some(&m.mime), PartData::Local(local_file(m)?))];
        if let Some(alt) = m.alt.as_ref().filter(|a| !a.is_empty()) {
            parts.push(Part::text("description", alt.clone()));
        }
        let up = net::post(format!("{b}/api/v2/media"))
            .bearer(&t)
            .multipart(parts)
            .timeout_secs(600)
            .get_json()?;
        let id = up.get("id").map(js_string).unwrap_or_default();
        if !truthy(up.get("url")) {
            poll(3000, 10 * 60_000, "Mastodon media processing", || {
                let r = net::get(format!("{b}/api/v1/media/{id}")).bearer(&t).get_json()?;
                Ok(if truthy(r.get("url")) { Some(()) } else { None })
            })?;
        }
        media_ids.push(Value::from(id));
    }
    let mut parts: Vec<Value> = vec![Value::from(ctx.text.clone())];
    parts.extend(ctx.opt("thread").and_then(|t| t.as_array()).cloned().unwrap_or_default());
    let mut first: Option<Value> = None;
    let mut prev: Option<Value> = None;
    for (i, status) in parts.iter().enumerate() {
        // Same purpose as the TS idempotency key (a hash of the text and position); the hash differs.
        let key = format!(
            "{}-{}",
            ctx.account_id,
            &hex(&Sha256::digest(format!("{}{i}", js_string(status)).as_bytes()))[..16]
        );
        let mut body = Map::new();
        body.insert("status".into(), status.clone());
        body.insert("media_ids".into(), Value::Array(if i == 0 { media_ids.clone() } else { vec![] }));
        put(&mut body, "in_reply_to_id", prev.clone());
        body.insert(
            "visibility".into(),
            ctx.opt("visibility")
                .cloned()
                .unwrap_or(Value::from(if i == 0 { "public" } else { "unlisted" })),
        );
        put(&mut body, "spoiler_text", ctx.opt("spoiler_text").cloned());
        put(&mut body, "language", ctx.opt("language").cloned());
        let s = net::post(format!("{b}/api/v1/statuses"))
            .bearer(&t)
            .header("idempotency-key", key)
            .json(Value::Object(body))
            .get_json()?;
        prev = s.get("id").cloned();
        if first.is_none() {
            first = Some(s);
        }
    }
    let first = first.unwrap_or_default();
    Ok(Published {
        id: first.get("id").map(js_string).unwrap_or_default(),
        url: super::opt_string(first.get("url")),
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{}/api/v1/statuses/{id}", base(&ctx.secret)))
        .bearer(&ctx.token())
        .get_json()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let s = net::get(format!("{}/api/v1/statuses/{id}", base(&ctx.secret)))
        .bearer(&ctx.token())
        .get_json()?;
    let mut o = Map::new();
    put(&mut o, "favourites", s.get("favourites_count").cloned());
    put(&mut o, "reblogs", s.get("reblogs_count").cloned());
    put(&mut o, "replies", s.get("replies_count").cloned());
    Ok(Value::Object(o))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let me = net::get(format!("{}/api/v1/accounts/verify_credentials", base(&ctx.secret)))
        .bearer(&ctx.token())
        .get_json()?;
    let mut o = Map::new();
    put(&mut o, "followers", me.get("followers_count").cloned());
    put(&mut o, "following", me.get("following_count").cloned());
    put(&mut o, "posts", me.get("statuses_count").cloned());
    Ok(Value::Object(o))
}
