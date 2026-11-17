//! X (src/providers/x.ts). Video chunks stream from disk.

use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};

use super::{expires_at, id_of, opt_string, or_zero, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::db::MediaRow;
use crate::media::{kind_of, local_file};
use crate::net::{self, Part, PartData};
use crate::util::{bail, js_string, str_of, truthy, Result};

const API: &str = "https://api.x.com/2";
const CHUNK: u64 = 4 * 1024 * 1024;

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://x.com/i/oauth2/authorize".into(),
        token_url: "https://api.x.com/2/oauth2/token".into(),
        scopes: vec!["tweet.read", "tweet.write", "users.read", "offline.access", "media.write"],
        env: "X",
        pkce: true,
        scope_sep: " ",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: true,
        client_id_param: "client_id",
    }
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let me = net::get(format!("{API}/users/me"))
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .query(&[("user.fields", Some("username,name,verified_type".into()))])
        .get_json()?["data"]
        .clone();
    let mut secret = Map::new();
    put(&mut secret, "access_token", t.get("access_token").cloned());
    put(&mut secret, "refresh_token", t.get("refresh_token").cloned());
    let mut meta = Map::new();
    put(&mut meta, "verified", me.get("verified_type").cloned());
    Ok(vec![Connected {
        external_id: id_of(&me),
        name: opt_string(me.get("name")).unwrap_or_default(),
        handle: opt_string(me.get("username")),
        secret: Value::Object(secret),
        meta: Some(Value::Object(meta)),
        expires_at: expires_at(t),
    }])
}

fn upload_media(token: &str, m: &MediaRow) -> Result<String> {
    let file = local_file(m)?;
    let video = kind_of(&m.mime) == "video";
    let category = if video {
        "tweet_video"
    } else if m.mime == "image/gif" {
        "tweet_gif"
    } else {
        "tweet_image"
    };
    let id = if category == "tweet_image" {
        let parts = vec![
            Part::file(
                "media",
                &m.filename,
                Some(&m.mime),
                PartData::Slice {
                    path: file.path.clone(),
                    offset: 0,
                    len: file.len,
                },
            ),
            Part::text("media_category", category),
        ];
        net::post(format!("{API}/media/upload"))
            .bearer(token)
            .multipart(parts)
            .get_json()?
            .pointer("/data/id")
            .map(js_string)
            .unwrap_or_default()
    } else {
        let init = net::post(format!("{API}/media/upload/initialize"))
            .bearer(token)
            .json(json!({ "media_type": m.mime, "total_bytes": file.len, "media_category": category }))
            .get_json()?;
        let id = init.pointer("/data/id").map(js_string).unwrap_or_default();
        let mut offset = 0u64;
        let mut seg = 0;
        while offset < file.len {
            let len = CHUNK.min(file.len - offset);
            let parts = vec![
                Part::file(
                    "media",
                    "chunk",
                    None,
                    PartData::Slice {
                        path: file.path.clone(),
                        offset,
                        len,
                    },
                ),
                Part::text("segment_index", seg.to_string()),
            ];
            net::post(format!("{API}/media/upload/{id}/append"))
                .bearer(token)
                .multipart(parts)
                .timeout_secs(300)
                .get_json()?;
            offset += len;
            seg += 1;
        }
        let mut info = net::post(format!("{API}/media/upload/{id}/finalize"))
            .bearer(token)
            .get_json()?
            .pointer("/data/processing_info")
            .cloned();
        let deadline = Instant::now() + Duration::from_secs(600);
        while let Some(i) = info.filter(|i| truthy(Some(i))) {
            match str_of(&i, "state") {
                Some("succeeded") => break,
                Some("failed") => bail!(
                    "X media processing failed: {}",
                    i.get("error").map(|e| e.to_string()).unwrap_or_else(|| "undefined".into())
                ),
                _ => {}
            }
            if Instant::now() > deadline {
                bail!("Timed out waiting for X video processing");
            }
            let wait = i.get("check_after_secs").and_then(|v| v.as_f64()).unwrap_or(2.0);
            std::thread::sleep(Duration::from_secs_f64(wait.max(0.0)));
            info = net::get(format!("{API}/media/upload"))
                .bearer(token)
                .query(&[("command", Some("STATUS".into())), ("media_id", Some(id.clone()))])
                .get_json()?
                .pointer("/data/processing_info")
                .cloned();
        }
        id
    };
    if let Some(alt) = m.alt.as_ref().filter(|a| !a.is_empty()) {
        let text: String = String::from_utf16_lossy(&alt.encode_utf16().take(1000).collect::<Vec<u16>>());
        net::post(format!("{API}/media/metadata"))
            .bearer(token)
            .json(json!({ "id": id, "metadata": { "alt_text": { "text": text } } }))
            .get_json()?;
    }
    Ok(id)
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let mut media_ids = vec![];
    for m in &ctx.media {
        media_ids.push(upload_media(&token, m)?);
    }
    let mut parts: Vec<Value> = vec![Value::from(ctx.text.clone())];
    parts.extend(ctx.opt("thread").and_then(|t| t.as_array()).cloned().unwrap_or_default());
    let mut first: Option<String> = None;
    let mut prev: Option<Value> = ctx.opt("reply_to").cloned();
    for (i, text) in parts.into_iter().enumerate() {
        let mut body = Map::new();
        body.insert("text".into(), text);
        if i == 0 && !media_ids.is_empty() {
            body.insert("media".into(), json!({ "media_ids": media_ids }));
        }
        if i == 0 && truthy(ctx.opt("quote")) {
            body.insert("quote_tweet_id".into(), ctx.opt("quote").cloned().unwrap_or_default());
        }
        if i == 0 && truthy(ctx.opt("reply_settings")) {
            body.insert("reply_settings".into(), ctx.opt("reply_settings").cloned().unwrap_or_default());
        }
        if let Some(p) = prev.as_ref().filter(|p| truthy(Some(p))) {
            body.insert("reply".into(), json!({ "in_reply_to_tweet_id": p }));
        }
        let r = net::post(format!("{API}/tweets")).bearer(&token).json(Value::Object(body)).get_json()?;
        let id = r.pointer("/data/id").map(js_string).unwrap_or_else(|| "undefined".into());
        if first.is_none() {
            first = Some(id.clone());
        }
        prev = Some(Value::from(id));
    }
    let first = first.unwrap_or_default();
    let handle = ctx.handle.clone().unwrap_or_else(|| "null".into());
    Ok(Published {
        url: Some(format!("https://x.com/{handle}/status/{first}")),
        id: first,
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{API}/tweets/{id}")).bearer(&ctx.token()).get_json()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let t = net::get(format!("{API}/tweets/{id}"))
        .bearer(&ctx.token())
        .query(&[("tweet.fields", Some("public_metrics".into()))])
        .get_json()?;
    let m = t.pointer("/data/public_metrics").cloned().unwrap_or_default();
    Ok(json!({
        "impressions": or_zero(m.get("impression_count")),
        "likes": or_zero(m.get("like_count")),
        "reposts": or_zero(m.get("retweet_count")),
        "replies": or_zero(m.get("reply_count")),
        "quotes": or_zero(m.get("quote_count")),
        "bookmarks": or_zero(m.get("bookmark_count")),
    }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{API}/users/me"))
        .bearer(&ctx.token())
        .query(&[("user.fields", Some("public_metrics".into()))])
        .get_json()?;
    let m = r.pointer("/data/public_metrics").cloned().unwrap_or_default();
    let mut o = Map::new();
    for (k, src) in [
        ("followers", "followers_count"),
        ("following", "following_count"),
        ("posts", "tweet_count"),
        ("likes", "like_count"),
    ] {
        put(&mut o, k, m.get(src).cloned());
    }
    Ok(Value::Object(o))
}
