//! Instagram API with Unified Login (Facebook Login for Business), on graph.facebook.com
//! (src/providers/instagram.ts). The token never expires; no Facebook Page is needed.

use serde_json::{json, Map, Value};

use super::{id_of, insight_map, opt_string, or_zero, poll, put, OAuthSpec, Published, RemoteComment, RemotePost};
use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, env, redirect_uri};
use crate::db::MediaRow;
use crate::media::{assert_public_url, get_media, kind_of};
use crate::net;
use crate::util::{bail, date_parse, js_string, permanent, some, str_of, truthy, Result};

pub fn ig_version() -> String {
    env("SIAR_IG_VERSION").or_else(|| env("SIAR_GRAPH_VERSION")).unwrap_or_else(|| "v24.0".into())
}

/// SIAR_IG_BASE overrides the host (tests).
pub fn host() -> String {
    env("SIAR_IG_BASE").unwrap_or_else(|| format!("https://graph.facebook.com/{}", ig_version()))
}

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: format!("https://www.facebook.com/{}/dialog/oauth", ig_version()),
        token_url: format!("https://graph.facebook.com/{}/oauth/access_token", ig_version()),
        scopes: vec![],
        env: "INSTAGRAM",
        pkce: false,
        scope_sep: " ",
        extra_auth_params: vec![],
        config_id_env: Some("INSTAGRAM_CONFIG_ID"),
    }
}

pub fn exchange(code: &str) -> Result<Value> {
    let (id, secret) = app_creds("INSTAGRAM")?;
    net::get(spec().token_url)
        .query(&[
            ("client_id", Some(id)),
            ("client_secret", Some(secret)),
            ("redirect_uri", Some(redirect_uri("instagram"))),
            ("code", Some(code.into())),
        ])
        .get_json()
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default().to_string();
    let fields = "id,username,name";
    let h = host();
    let mut list: Vec<Value> = net::get(format!("{h}/me/assigned_instagram_accounts"))
        .bearer(&token)
        .query(&[("fields", Some(fields.into()))])
        .get_json()
        .ok()
        .and_then(|r| r.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();
    if list.is_empty() {
        // Fallback: Instagram accounts linked to Pages the token can see.
        let pages = net::get(format!("{h}/me/accounts"))
            .bearer(&token)
            .query(&[
                ("fields", Some("instagram_business_account{id,username,name}".into())),
                ("limit", Some("100".into())),
            ])
            .get_json()
            .unwrap_or_default();
        list = pages["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.get("instagram_business_account").filter(|x| truthy(Some(x))).cloned())
            .collect();
    }
    if list.is_empty() {
        bail!("The login gave access to no Instagram account. Reconnect and tick the Instagram accounts on the asset screen.");
    }
    let mut out = vec![];
    for a in list {
        let d = if truthy(a.get("username")) {
            a.clone()
        } else {
            net::get(format!("{h}/{}", id_of(&a)))
                .bearer(&token)
                .query(&[("fields", Some(fields.into()))])
                .get_json()
                .unwrap_or(a.clone())
        };
        let id = id_of(&d);
        let name = [d.get("name"), d.get("username")]
            .into_iter()
            .find(|v| truthy(*v))
            .flatten()
            .map(js_string)
            .unwrap_or_else(|| id.clone());
        out.push(Connected {
            external_id: id,
            name,
            handle: opt_string(d.get("username")),
            secret: json!({ "access_token": token }),
            meta: None,
            expires_at: None,
        });
    }
    Ok(out)
}

fn wait_container(host: &str, id: &str, token: &str) -> Result<()> {
    poll(4000, 15 * 60_000, "Instagram media processing", || {
        let r = net::get(format!("{host}/{id}"))
            .bearer(token)
            .query(&[("fields", Some("status_code,status".into()))])
            .get_json()?;
        let code = some(r.get("status_code")).or(some(r.get("status"))).map(js_string).unwrap_or_default();
        if code == "FINISHED" || code == "PUBLISHED" {
            return Ok(Some(()));
        }
        if code == "ERROR" || code == "EXPIRED" {
            permanent!("Instagram media container {code}: {}", opt_string(r.get("status")).unwrap_or_default());
        }
        Ok(None)
    })
}

/// Publish to an Instagram professional account. `host` is graph.instagram.com/vX or graph.facebook.com/vX.
pub fn publish(host: &str, ctx: &Ctx) -> Result<Published> {
    let ig = &ctx.external_id;
    let token = ctx.token();
    for m in &ctx.media {
        assert_public_url(m, "Instagram")?;
    }
    let story = ctx.opt_str("format").as_deref() == Some("story") || ctx.opt_str("type").as_deref() == Some("story");
    let item = |m: &MediaRow, extra: Map<String, Value>| -> Value {
        let mut o = Map::new();
        if kind_of(&m.mime) == "video" {
            o.insert("video_url".into(), m.url.clone().into());
        } else {
            o.insert("image_url".into(), m.url.clone().into());
            if !story {
                put(&mut o, "alt_text", m.alt.clone().map(Value::from));
            }
        }
        for (k, v) in extra {
            o.insert(k, v);
        }
        Value::Object(o)
    };
    let create = |body: Value| -> Result<String> { Ok(id_of(&net::post(format!("{host}/{ig}/media")).bearer(&token).json(body).get_json()?)) };
    let ai = if truthy(ctx.opt("is_ai_generated")) {
        ctx.opt("is_ai_generated").cloned()
    } else {
        None
    };
    let mut common = Map::new();
    put(&mut common, "caption", if ctx.text.is_empty() { None } else { Some(ctx.text.clone().into()) });
    put(
        &mut common,
        "collaborators",
        ctx.opt("collaborators")
            .and_then(|v| v.as_array())
            .map(|a| Value::from(a.iter().map(|u| js_string(u).trim_start_matches('@').to_string()).collect::<Vec<_>>())),
    );
    put(&mut common, "location_id", ctx.opt("location_id").cloned());
    put(&mut common, "is_ai_generated", ai.clone());
    let creation;
    if story {
        if ctx.media.len() != 1 {
            permanent!("An Instagram story takes exactly one image or video");
        }
        let mut extra = Map::new();
        extra.insert("media_type".into(), "STORIES".into());
        put(&mut extra, "is_ai_generated", ai);
        creation = create(item(&ctx.media[0], extra))?;
    } else if ctx.media.len() == 1 {
        let m = &ctx.media[0];
        let mut extra = common.clone();
        put(&mut extra, "user_tags", ctx.opt("user_tags").cloned());
        if kind_of(&m.mime) == "video" {
            let mut cover_url = ctx.opt("cover_url").cloned();
            if let Some(cid) = ctx.opt_str("cover_media_id").filter(|s| !s.is_empty()) {
                let c = get_media(&cid)?;
                assert_public_url(&c, "Instagram")?;
                cover_url = Some(c.url.into());
            }
            extra.insert("media_type".into(), "REELS".into());
            extra.insert("share_to_feed".into(), ctx.opt("share_to_feed").cloned().unwrap_or(Value::Bool(true)));
            let has_cover = truthy(cover_url.as_ref());
            put(&mut extra, "cover_url", cover_url);
            if !has_cover {
                put(&mut extra, "thumb_offset", ctx.opt("thumb_offset_ms").cloned());
            }
            if truthy(ctx.opt("trial")) {
                let g = if ctx.opt_str("trial").as_deref() == Some("auto") {
                    "SS_PERFORMANCE"
                } else {
                    "MANUAL"
                };
                extra.insert("trial_params".into(), json!({ "graduation_strategy": g }));
            }
            extra.remove("user_tags");
        }
        creation = create(item(m, extra))?;
    } else {
        let mut children = vec![];
        for m in &ctx.media {
            let mut extra = Map::new();
            extra.insert("is_carousel_item".into(), true.into());
            let video = kind_of(&m.mime) == "video";
            if video {
                extra.insert("media_type".into(), "VIDEO".into());
            }
            let c = create(item(m, extra))?;
            if video {
                wait_container(host, &c, &token)?;
            }
            children.push(c);
        }
        let mut body = Map::new();
        body.insert("media_type".into(), "CAROUSEL".into());
        body.insert("children".into(), children.join(",").into());
        for (k, v) in common {
            body.insert(k, v);
        }
        creation = create(Value::Object(body))?;
    }
    wait_container(host, &creation, &token)?;
    let publ = net::post(format!("{host}/{ig}/media_publish"))
        .bearer(&token)
        .json(json!({ "creation_id": creation }))
        .get_json()?;
    let pid = id_of(&publ);
    let info = net::get(format!("{host}/{pid}"))
        .bearer(&token)
        .query(&[("fields", Some("permalink".into()))])
        .get_json()
        .unwrap_or_default();
    if truthy(ctx.opt("first_comment")) && !story {
        let _ = net::post(format!("{host}/{pid}/comments"))
            .bearer(&token)
            .json(json!({ "message": ctx.opt("first_comment") }))
            .send();
    }
    Ok(Published {
        id: pid,
        url: opt_string(info.get("permalink")),
    })
}

pub fn remove(host: &str, ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{host}/{id}")).bearer(&ctx.token()).send()?;
    Ok(())
}

const INSIGHT_METRICS: &str = "views,reach,likes,comments,shares,saved,total_interactions";

pub fn media_insights(host: &str, token: &str, id: &str, fallback: Option<&Value>) -> Result<Value> {
    let r = net::get(format!("{host}/{id}/insights"))
        .bearer(token)
        .query(&[("metric", Some(INSIGHT_METRICS.into()))])
        .get_json()
        .ok();
    if let Some(data) = r.as_ref().and_then(|r| r.get("data")).filter(|d| truthy(Some(d))) {
        return Ok(insight_map(Some(data)));
    }
    let f = match fallback {
        Some(f) => f.clone(),
        None => net::get(format!("{host}/{id}"))
            .bearer(token)
            .query(&[("fields", Some("like_count,comments_count".into()))])
            .get_json()?,
    };
    Ok(json!({ "likes": or_zero(f.get("like_count")), "comments": or_zero(f.get("comments_count")) }))
}

pub fn posts(host: &str, ctx: &Ctx, since: i64, limit: usize, with_metrics: bool) -> Result<Vec<RemotePost>> {
    let token = ctx.token();
    let mut out = vec![];
    let mut url = Some(format!("{host}/{}/media", ctx.external_id));
    let mut first = true;
    while let Some(u) = url.take() {
        if out.len() >= limit {
            break;
        }
        let mut rq = net::get(u).bearer(&token);
        if first {
            rq = rq.query(&[
                (
                    "fields",
                    Some("id,caption,media_type,media_product_type,permalink,timestamp,like_count,comments_count".into()),
                ),
                ("limit", Some("50".into())),
            ]);
            first = false;
        }
        let r = rq.get_json()?;
        for m in r["data"].as_array().into_iter().flatten() {
            let created = date_parse(m.get("timestamp"));
            if created < since {
                return Ok(out);
            }
            let kind = if str_of(m, "media_product_type") == Some("REELS") {
                "reel".to_string()
            } else {
                m.get("media_type").map(js_string).unwrap_or_else(|| "undefined".into()).to_lowercase()
            };
            let metrics = if with_metrics {
                media_insights(host, &token, &id_of(m), Some(m))?
            } else {
                json!({ "likes": or_zero(m.get("like_count")), "comments": or_zero(m.get("comments_count")) })
            };
            out.push(RemotePost {
                id: id_of(m),
                url: opt_string(m.get("permalink")),
                text: opt_string(m.get("caption")),
                kind: Some(kind),
                username: None,
                created_at: created,
                metrics: Some(metrics),
            });
            if out.len() >= limit {
                break;
            }
        }
        url = opt_string(r.pointer("/paging/next"));
    }
    Ok(out)
}

pub fn comments(host: &str, ctx: &Ctx, post_id: &str) -> Result<Vec<RemoteComment>> {
    let token = ctx.token();
    let me = ctx.handle.as_ref().map(|h| h.to_lowercase());
    let map = |c: &Value, parent: Option<String>| {
        let username = opt_string(c.get("username")).or_else(|| opt_string(c.pointer("/from/username")));
        RemoteComment {
            id: id_of(c),
            post_id: post_id.to_string(),
            parent_id: parent,
            text: opt_string(c.get("text")).unwrap_or_default(),
            author_id: opt_string(c.pointer("/from/id")),
            mine: username.as_ref().map(|u| u.to_lowercase()) == me,
            username,
            created_at: date_parse(c.get("timestamp")),
            hidden: truthy(c.get("hidden")),
        }
    };
    let mut out = vec![];
    let mut url = Some(format!("{host}/{post_id}/comments"));
    let mut first = true;
    while let Some(u) = url.take() {
        if out.len() >= 500 {
            break;
        }
        let mut rq = net::get(u).bearer(&token);
        if first {
            rq = rq.query(&[
                (
                    "fields",
                    Some("id,text,timestamp,username,from,hidden,like_count,replies{id,text,timestamp,username,from,hidden,like_count}".into()),
                ),
                ("limit", Some("50".into())),
            ]);
            first = false;
        }
        let r = rq.get_json()?;
        for c in r["data"].as_array().into_iter().flatten() {
            out.push(map(c, None));
            for rep in c.pointer("/replies/data").and_then(|d| d.as_array()).into_iter().flatten() {
                out.push(map(rep, Some(id_of(c))));
            }
        }
        url = opt_string(r.pointer("/paging/next"));
    }
    Ok(out)
}

pub fn publishing_limit(host: &str, ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{host}/{}/content_publishing_limit", ctx.external_id))
        .bearer(&ctx.token())
        .query(&[("fields", Some("quota_usage,config".into()))])
        .get_json()?;
    let d = r.pointer("/data/0").cloned().unwrap_or_default();
    Ok(json!({
        "used_24h": or_zero(d.get("quota_usage")),
        "limit_24h": some(d.pointer("/config/quota_total")).cloned().unwrap_or(Value::from(100)),
    }))
}

pub fn account_metrics(host: &str, ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{host}/{}", ctx.external_id))
        .bearer(&ctx.token())
        .query(&[("fields", Some("followers_count,follows_count,media_count".into()))])
        .get_json()?;
    Ok(json!({
        "followers": or_zero(r.get("followers_count")),
        "following": or_zero(r.get("follows_count")),
        "posts": or_zero(r.get("media_count")),
    }))
}
