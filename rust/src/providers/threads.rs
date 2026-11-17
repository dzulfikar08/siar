//! Threads (src/providers/meta.ts, Threads part).

use serde_json::{json, Map, Value};

use super::{id_of, insight_map, opt_string, poll, put, OAuthSpec, Published, RemoteComment, RemotePost};
use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, env, redirect_uri};
use crate::media::{assert_public_url, get_media, kind_of};
use crate::net;
use crate::util::{date_parse, jnum, js_string, now, permanent, some, str_of, to_num, truthy, Result};

fn th() -> String {
    env("SIAR_THREADS_BASE").unwrap_or_else(|| "https://graph.threads.net/v1.0".into())
}

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://threads.net/oauth/authorize".into(),
        token_url: "https://graph.threads.net/oauth/access_token".into(),
        scopes: vec![
            "threads_basic",
            "threads_content_publish",
            "threads_manage_insights",
            "threads_manage_replies",
            "threads_read_replies",
            "threads_keyword_search",
        ],
        env: "THREADS",
        pkce: false,
        scope_sep: ",",
        extra_auth_params: vec![],
        config_id_env: None,
    }
}

const SIXTY_DAYS_S: f64 = 5_184_000.0;

fn expires_in(t: &Value) -> f64 {
    match some(t.get("expires_in")) {
        Some(v) => to_num(Some(v)),
        None => SIXTY_DAYS_S,
    }
}

pub fn exchange(code: &str) -> Result<Value> {
    let (id, secret) = app_creds("THREADS")?;
    let short = net::post(spec().token_url)
        .form(&[
            ("client_id", Some(id)),
            ("client_secret", Some(secret.clone())),
            ("grant_type", Some("authorization_code".into())),
            ("redirect_uri", Some(redirect_uri("threads"))),
            ("code", Some(code.into())),
        ])
        .get_json()?;
    net::get("https://graph.threads.net/access_token")
        .query(&[
            ("grant_type", Some("th_exchange_token".into())),
            ("client_secret", Some(secret)),
            ("access_token", opt_string(short.get("access_token"))),
        ])
        .get_json()
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default().to_string();
    let me = net::get(format!("{}/me", th()))
        .bearer(&token)
        .query(&[("fields", Some("id,username,name".into()))])
        .get_json()?;
    Ok(vec![Connected {
        external_id: id_of(&me),
        name: opt_string(me.get("name")).or_else(|| opt_string(me.get("username"))).unwrap_or_default(),
        handle: opt_string(me.get("username")),
        secret: json!({ "access_token": token }),
        meta: None,
        expires_at: Some(now() + (expires_in(t) * 1000.0) as i64),
    }])
}

pub fn refresh(s: &Value) -> crate::util::Result<(Value, Option<i64>)> {
    let r = net::get("https://graph.threads.net/refresh_access_token")
        .query(&[
            ("grant_type", Some("th_refresh_token".into())),
            ("access_token", opt_string(s.get("access_token"))),
        ])
        .get_json()?;
    let mut secret = s.as_object().cloned().unwrap_or_default();
    secret.insert("access_token".into(), r.get("access_token").cloned().unwrap_or(Value::Null));
    Ok((Value::Object(secret), Some(now() + (expires_in(&r) * 1000.0) as i64)))
}

fn wait(id: &str, token: &str) -> Result<()> {
    poll(3000, 15 * 60_000, "Threads media processing", || {
        let r = net::get(format!("{}/{id}", th()))
            .bearer(token)
            .query(&[("fields", Some("status,error_message".into()))])
            .get_json()?;
        let st = opt_string(r.get("status")).unwrap_or_default();
        if st == "FINISHED" || st == "PUBLISHED" {
            return Ok(Some(()));
        }
        if st == "ERROR" || st == "EXPIRED" {
            permanent!("Threads container {st}: {}", opt_string(r.get("error_message")).unwrap_or_default());
        }
        Ok(None)
    })
}

fn media_body(kind: &str, url: &str, alt: Option<&str>) -> Map<String, Value> {
    let mut o = Map::new();
    if kind == "video" {
        o.insert("media_type".into(), "VIDEO".into());
        o.insert("video_url".into(), url.into());
    } else {
        o.insert("media_type".into(), "IMAGE".into());
        o.insert("image_url".into(), url.into());
    }
    put(&mut o, "alt_text", alt.map(Value::from));
    o
}

fn is_video_url(u: &str) -> bool {
    let l = u.to_lowercase();
    [".mp4", ".mov", ".webm"].iter().any(|e| l.ends_with(e) || l.contains(&format!("{e}?")))
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let base = th();
    for m in &ctx.media {
        assert_public_url(m, "Threads")?;
    }
    let create = |body: Map<String, Value>| -> Result<String> {
        Ok(id_of(
            &net::post(format!("{base}/me/threads")).bearer(&token).json(Value::Object(body)).get_json()?,
        ))
    };
    let publish_one = |creation: &str| -> Result<String> {
        wait(creation, &token)?;
        Ok(id_of(
            &net::post(format!("{base}/me/threads_publish"))
                .bearer(&token)
                .json(json!({ "creation_id": creation }))
                .get_json()?,
        ))
    };
    let text_only = ctx.media.is_empty();
    if !text_only && (truthy(ctx.opt("poll")) || truthy(ctx.opt("link_attachment")) || truthy(ctx.opt("text_attachment"))) {
        permanent!("Threads polls, link cards and text attachments need a post without media");
    }
    let flag = |k: &str| if truthy(ctx.opt(k)) { ctx.opt(k).cloned() } else { None };
    let mut first = Map::new();
    put(&mut first, "text", if ctx.text.is_empty() { None } else { Some(ctx.text.clone().into()) });
    for k in ["topic_tag", "reply_control", "reply_to_id", "quote_post_id"] {
        put(&mut first, k, ctx.opt(k).cloned());
    }
    put(&mut first, "is_made_with_ai", flag("is_made_with_ai"));
    put(&mut first, "is_spoiler_media", flag("is_spoiler_media"));
    if truthy(ctx.opt("poll")) {
        let opts: Vec<String> = ctx
            .opt("poll")
            .and_then(|p| p.get("options"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .map(|s| js_string(s).chars().take(25).collect())
            .collect();
        if opts.len() < 2 || opts.len() > 4 {
            permanent!("A Threads poll takes 2-4 options");
        }
        let mut poll = Map::new();
        for (i, s) in opts.iter().enumerate() {
            poll.insert(format!("option_{}", ["a", "b", "c", "d"][i]), s.clone().into());
        }
        first.insert("poll_attachment".into(), Value::Object(poll));
    }
    let link = some(ctx.opt("link_attachment")).or(ctx.opt("link")).cloned();
    if truthy(link.as_ref()) {
        first.insert("link_attachment".into(), link.unwrap_or_default());
    }
    if truthy(ctx.opt("text_attachment")) {
        let ta = ctx.opt("text_attachment").cloned().unwrap_or_default();
        let mut o = Map::new();
        put(&mut o, "plaintext", ta.get("text").cloned());
        put(&mut o, "link_attachment_url", ta.get("link_url").cloned());
        first.insert("text_attachment".into(), Value::Object(o));
    }
    let creation = if text_only {
        let mut b = Map::new();
        b.insert("media_type".into(), "TEXT".into());
        b.extend(first);
        create(b)?
    } else if ctx.media.len() == 1 {
        let m = &ctx.media[0];
        let mut b = media_body(kind_of(&m.mime), &m.url, m.alt.as_deref());
        b.extend(first);
        create(b)?
    } else {
        let mut children = vec![];
        for m in &ctx.media {
            let mut b = media_body(kind_of(&m.mime), &m.url, m.alt.as_deref());
            b.insert("is_carousel_item".into(), true.into());
            children.push(create(b)?);
        }
        for c in &children {
            wait(c, &token)?;
        }
        let mut b = Map::new();
        b.insert("media_type".into(), "CAROUSEL".into());
        b.insert("children".into(), children.join(",").into());
        b.extend(first);
        create(b)?
    };
    let first_id = publish_one(&creation)?;
    let mut prev = first_id.clone();
    for part in ctx.opt("thread").and_then(|v| v.as_array()).into_iter().flatten() {
        let p = match part {
            Value::String(s) => json!({ "text": s }),
            other => other.clone(),
        };
        let media: Option<(String, String, Option<String>)> = if let Some(mid) = p.pointer("/media_ids/0").filter(|v| truthy(Some(v))) {
            let m = get_media(&js_string(mid))?;
            assert_public_url(&m, "Threads")?;
            Some((kind_of(&m.mime).to_string(), m.url.clone(), m.alt.clone()))
        } else {
            p.get("media_url")
                .filter(|v| truthy(Some(v)))
                .map(js_string)
                .map(|u| ((if is_video_url(&u) { "video" } else { "image" }).to_string(), u, None))
        };
        let mut b = match &media {
            Some((k, u, alt)) => media_body(k, u, alt.as_deref()),
            None => {
                let mut b = Map::new();
                b.insert("media_type".into(), "TEXT".into());
                b
            }
        };
        put(&mut b, "text", p.get("text").cloned());
        b.insert("reply_to_id".into(), prev.clone().into());
        prev = publish_one(&create(b)?)?;
    }
    let info = net::get(format!("{base}/{first_id}"))
        .bearer(&token)
        .query(&[("fields", Some("permalink".into()))])
        .get_json()
        .unwrap_or_default();
    Ok(Published {
        id: first_id,
        url: opt_string(info.get("permalink")),
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{}/{id}", th())).bearer(&ctx.token()).send()?;
    Ok(())
}

pub fn insights(token: &str, id: &str) -> Result<Value> {
    let r = net::get(format!("{}/{id}/insights", th()))
        .bearer(token)
        .query(&[("metric", Some("views,likes,replies,reposts,quotes,shares".into()))])
        .get_json()?;
    Ok(insight_map(r.get("data")))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{}/me/threads_insights", th()))
        .bearer(&ctx.token())
        .query(&[("metric", Some("followers_count,views,likes,replies,reposts,quotes".into()))])
        .get_json()?;
    let mut out = Map::new();
    for m in r["data"].as_array().into_iter().flatten() {
        let name = m.get("name").map(js_string).unwrap_or_default();
        let key = if name == "followers_count" { "followers".to_string() } else { name };
        let v = if let Some(t) = some(m.pointer("/total_value/value")) {
            to_num(Some(t))
        } else if let Some(vals) = m.get("values").and_then(|v| v.as_array()) {
            vals.iter().map(|v| to_num(v.get("value"))).sum()
        } else {
            0.0
        };
        out.insert(key, jnum(v));
    }
    Ok(Value::Object(out))
}

fn map_post(p: &Value) -> RemotePost {
    RemotePost {
        id: id_of(p),
        url: opt_string(p.get("permalink")),
        text: opt_string(p.get("text")),
        kind: Some(some(p.get("media_type")).map(js_string).unwrap_or_default().to_lowercase()),
        username: opt_string(p.get("username")),
        created_at: date_parse(p.get("timestamp")),
        metrics: None,
    }
}

pub fn posts(ctx: &Ctx, since: i64, limit: usize, with_metrics: bool) -> Result<Vec<RemotePost>> {
    let token = ctx.token();
    let mut out = vec![];
    let mut url = Some(format!("{}/me/threads", th()));
    let mut first = true;
    while let Some(u) = url.take() {
        if out.len() >= limit {
            break;
        }
        let mut rq = net::get(u).bearer(&token);
        if first {
            rq = rq.query(&[
                ("fields", Some("id,text,media_type,permalink,timestamp,username,is_quote_post".into())),
                ("since", Some((since / 1000).to_string())),
                ("limit", Some("50".into())),
            ]);
            first = false;
        }
        let r = rq.get_json()?;
        for p in r["data"].as_array().into_iter().flatten() {
            if out.len() >= limit {
                break;
            }
            let mut post = map_post(p);
            if with_metrics {
                post.metrics = Some(insights(&token, &id_of(p)).unwrap_or_else(|_| json!({})));
            }
            out.push(post);
        }
        url = opt_string(r.pointer("/paging/next"));
    }
    Ok(out)
}

pub fn publishing_limit(ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{}/me/threads_publishing_limit", th()))
        .bearer(&ctx.token())
        .query(&[("fields", Some("quota_usage,config,reply_quota_usage,reply_config".into()))])
        .get_json()?;
    let d = r.pointer("/data/0").cloned().unwrap_or_default();
    let mut o = Map::new();
    put(&mut o, "posts_used_24h", d.get("quota_usage").cloned());
    put(&mut o, "posts_limit_24h", d.pointer("/config/quota_total").cloned());
    put(&mut o, "replies_used_24h", d.get("reply_quota_usage").cloned());
    put(&mut o, "replies_limit_24h", d.pointer("/reply_config/quota_total").cloned());
    Ok(Value::Object(o))
}

pub fn comments(ctx: &Ctx, post_id: &str) -> Result<Vec<RemoteComment>> {
    let r = net::get(format!("{}/{post_id}/conversation", th()))
        .bearer(&ctx.token())
        .query(&[
            (
                "fields",
                Some("id,text,username,timestamp,hide_status,replied_to{id},is_reply_owned_by_me,root_post{id}".into()),
            ),
            ("reverse", Some("false".into())),
            ("limit", Some("100".into())),
        ])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let replied = opt_string(c.pointer("/replied_to/id")).filter(|s| !s.is_empty());
            let username = opt_string(c.get("username"));
            RemoteComment {
                id: id_of(c),
                post_id: post_id.to_string(),
                parent_id: replied.filter(|r| r != post_id),
                text: opt_string(c.get("text")).unwrap_or_default(),
                author_id: username.clone(),
                mine: truthy(c.get("is_reply_owned_by_me")) || (username.is_some() && username == ctx.handle),
                username,
                created_at: date_parse(c.get("timestamp")),
                hidden: str_of(c, "hide_status") == Some("HIDDEN"),
            }
        })
        .collect())
}
