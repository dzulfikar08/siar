//! Facebook Pages, and Instagram through a Facebook Page.

use serde_json::{json, Map, Value};

use super::{id_of, opt_string, or_zero, put, OAuthSpec, Published, RemoteComment, RemotePost};
use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, env, redirect_uri};
use crate::db::MediaRow;
use crate::media::{assert_public_url, kind_of, local_file};
use crate::net::{self, Part, PartData};
use crate::util::{date_parse, js_string, permanent, some, str_of, truthy, Result};

fn v() -> String {
    env("SIAR_GRAPH_VERSION").unwrap_or_else(|| "v24.0".into())
}

pub fn graph(path: &str) -> String {
    format!("https://graph.facebook.com/{}/{path}", v())
}

/// Host for the Instagram-via-Page calls.
pub fn fb_host() -> String {
    format!("https://graph.facebook.com/{}", v())
}

fn fb_spec(env: &'static str, scopes: Vec<&'static str>) -> OAuthSpec {
    OAuthSpec {
        auth_url: format!("https://www.facebook.com/{}/dialog/oauth", v()),
        token_url: graph("oauth/access_token"),
        scopes,
        env,
        pkce: false,
        scope_sep: ",",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: false,
        client_id_param: "client_id",
    }
}

pub fn facebook_spec() -> OAuthSpec {
    fb_spec(
        "FACEBOOK",
        vec![
            "pages_show_list",
            "pages_manage_posts",
            "pages_read_engagement",
            "pages_manage_engagement",
            "read_insights",
            "business_management",
        ],
    )
}

pub fn ig_spec() -> OAuthSpec {
    fb_spec(
        "FACEBOOK",
        vec![
            "instagram_basic",
            "instagram_content_publish",
            "instagram_manage_insights",
            "instagram_manage_comments",
            "instagram_manage_messages",
            "pages_show_list",
            "pages_read_engagement",
            "pages_manage_metadata",
            "business_management",
        ],
    )
}

/// Facebook Login code exchange, then a long-lived (60 day) user token. Page tokens derived from it never expire.
pub fn fb_exchange(env: &str, platform: &str, code: &str) -> Result<Value> {
    let (id, secret) = app_creds(env)?;
    let short = net::get(graph("oauth/access_token"))
        .query(&[
            ("client_id", Some(id.clone())),
            ("client_secret", Some(secret.clone())),
            ("redirect_uri", Some(redirect_uri(platform))),
            ("code", Some(code.into())),
        ])
        .get_json()?;
    net::get(graph("oauth/access_token"))
        .query(&[
            ("grant_type", Some("fb_exchange_token".into())),
            ("client_id", Some(id)),
            ("client_secret", Some(secret)),
            ("fb_exchange_token", opt_string(short.get("access_token"))),
        ])
        .get_json()
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let r = net::get(graph("me/accounts"))
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .query(&[("fields", Some("id,name,username,access_token".into())), ("limit", Some("100".into()))])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| Connected {
            external_id: id_of(p),
            name: opt_string(p.get("name")).unwrap_or_default(),
            handle: opt_string(p.get("username")),
            secret: Obj::from_pairs(&[("access_token", p.get("access_token"))]),
            meta: None,
            expires_at: None,
        })
        .collect())
}

/// Instagram professional accounts linked to the Pages the user picked.
pub fn ig_profile(t: &Value) -> Result<Vec<Connected>> {
    let r = net::get(graph("me/accounts"))
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .query(&[
            ("fields", Some("id,name,access_token,instagram_business_account{id,username,name}".into())),
            ("limit", Some("100".into())),
        ])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| truthy(p.get("instagram_business_account")))
        .map(|p| {
            let ig = &p["instagram_business_account"];
            Connected {
                external_id: id_of(ig),
                name: opt_string(ig.get("name")).or_else(|| opt_string(ig.get("username"))).unwrap_or_default(),
                handle: opt_string(ig.get("username")),
                secret: Obj::from_pairs(&[("access_token", p.get("access_token"))]),
                meta: Some(json!({ "pageId": p.get("id") })),
                expires_at: None,
            }
        })
        .collect())
}

struct Obj;
impl Obj {
    /// `{k: v}` leaving out undefined values.
    fn from_pairs(pairs: &[(&str, Option<&Value>)]) -> Value {
        let mut m = Map::new();
        for (k, v) in pairs {
            put(&mut m, k, v.cloned());
        }
        Value::Object(m)
    }
}

fn photo(page: &str, token: &str, m: &MediaRow, published: bool, text: &str) -> Result<Value> {
    let f = local_file(m)?;
    let mut parts = vec![
        Part::file("source", &m.filename, Some(&m.mime), PartData::Local(f)),
        Part::text("published", published.to_string()),
    ];
    if published {
        parts.push(Part::text("message", text));
    }
    if let Some(alt) = m.alt.as_ref().filter(|a| !a.is_empty()) {
        parts.push(Part::text("alt_text_custom", alt.clone()));
    }
    net::post(graph(&format!("{page}/photos"))).bearer(token).multipart(parts).get_json()
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let page = ctx.external_id.clone();
    let token = ctx.token();
    let video = ctx.media.iter().find(|m| kind_of(&m.mime) == "video");
    let res = if let Some(video) = video {
        if ctx.media.len() > 1 {
            permanent!("Facebook takes one video per post");
        }
        if ctx.opt_str("video_as").unwrap_or_else(|| "reel".into()) == "reel" {
            assert_public_url(video, "Facebook Reels")?;
            let start = net::post(graph(&format!("{page}/video_reels")))
                .bearer(&token)
                .json(json!({ "upload_phase": "start" }))
                .get_json()?;
            net::post(start.get("upload_url").map(js_string).unwrap_or_default())
                .header("authorization", format!("OAuth {token}"))
                .header("file_url", video.url.clone())
                .get_json()?;
            net::post(graph(&format!("{page}/video_reels")))
                .bearer(&token)
                .json(json!({ "upload_phase": "finish", "video_id": start.get("video_id"), "video_state": "PUBLISHED", "description": ctx.text }))
                .get_json()?;
            let vid = start.get("video_id").map(js_string).unwrap_or_default();
            Published {
                url: Some(format!("https://www.facebook.com/reel/{vid}")),
                id: vid,
            }
        } else {
            let f = local_file(video)?;
            let mut parts = vec![
                Part::file("source", &video.filename, Some(&video.mime), PartData::Local(f)),
                Part::text("description", ctx.text.clone()),
            ];
            if truthy(ctx.opt("video_title")) {
                parts.push(Part::text("title", ctx.opt_str("video_title").unwrap_or_default()));
            }
            let r = net::post(format!("https://graph-video.facebook.com/{}/{page}/videos", v()))
                .bearer(&token)
                .multipart(parts)
                .timeout_secs(1200)
                .get_json()?;
            let id = id_of(&r);
            Published {
                url: Some(format!("https://www.facebook.com/{page}/videos/{id}")),
                id,
            }
        }
    } else if ctx.media.len() == 1 {
        let r = photo(&page, &token, &ctx.media[0], true, &ctx.text)?;
        let id = some(r.get("post_id")).or(r.get("id")).map(js_string).unwrap_or_else(|| "undefined".into());
        Published {
            url: Some(format!("https://www.facebook.com/{id}")),
            id,
        }
    } else {
        let mut body = Map::new();
        body.insert("message".into(), ctx.text.clone().into());
        if !ctx.media.is_empty() {
            let mut ids = vec![];
            for m in &ctx.media {
                ids.push(json!({ "media_fbid": photo(&page, &token, m, false, &ctx.text)?.get("id") }));
            }
            body.insert("attached_media".into(), Value::Array(ids));
        } else if truthy(ctx.opt("link")) {
            body.insert("link".into(), ctx.opt("link").cloned().unwrap_or_default());
        }
        let r = net::post(graph(&format!("{page}/feed"))).bearer(&token).json(Value::Object(body)).get_json()?;
        let id = id_of(&r);
        Published {
            url: Some(format!("https://www.facebook.com/{id}")),
            id,
        }
    };
    if truthy(ctx.opt("first_comment")) {
        let _ = net::post(graph(&format!("{}/comments", res.id)))
            .bearer(&token)
            .json(json!({ "message": ctx.opt("first_comment") }))
            .send();
    }
    Ok(res)
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", graph(id)).bearer(&ctx.token()).send()?;
    Ok(())
}

fn counts(r: &Value) -> Value {
    json!({
        "reactions": or_zero(r.pointer("/reactions/summary/total_count")),
        "comments": or_zero(r.pointer("/comments/summary/total_count")),
        "shares": or_zero(r.pointer("/shares/count")),
    })
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let r = net::get(graph(id))
        .bearer(&ctx.token())
        .query(&[(
            "fields",
            Some("reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares".into()),
        )])
        .get_json()?;
    Ok(counts(&r))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let r = net::get(graph(&ctx.external_id))
        .bearer(&ctx.token())
        .query(&[("fields", Some("followers_count,fan_count".into()))])
        .get_json()?;
    Ok(json!({ "followers": or_zero(r.get("followers_count")), "fans": or_zero(r.get("fan_count")) }))
}

pub fn posts(ctx: &Ctx, since: i64, limit: usize) -> Result<Vec<RemotePost>> {
    let r = net::get(graph(&format!("{}/posts", ctx.external_id)))
        .bearer(&ctx.token())
        .query(&[
            (
                "fields",
                Some("id,message,permalink_url,created_time,reactions.summary(total_count).limit(0),comments.summary(total_count).limit(0),shares".into()),
            ),
            ("since", Some((since / 1000).to_string())),
            ("limit", Some(limit.min(100).to_string())),
        ])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| RemotePost {
            id: id_of(p),
            url: opt_string(p.get("permalink_url")),
            text: opt_string(p.get("message")),
            kind: Some("post".into()),
            username: None,
            created_at: date_parse(p.get("created_time")),
            metrics: Some(counts(p)),
        })
        .collect())
}

pub fn comments(ctx: &Ctx, post_id: &str) -> Result<Vec<RemoteComment>> {
    let r = net::get(graph(&format!("{post_id}/comments")))
        .bearer(&ctx.token())
        .query(&[
            ("fields", Some("id,message,created_time,from,is_hidden,like_count,parent{id}".into())),
            ("filter", Some("stream".into())),
            ("limit", Some("100".into())),
        ])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| RemoteComment {
            id: id_of(c),
            post_id: post_id.to_string(),
            parent_id: opt_string(c.pointer("/parent/id")),
            text: opt_string(c.get("message")).unwrap_or_default(),
            author_id: opt_string(c.pointer("/from/id")),
            username: opt_string(c.pointer("/from/name")),
            created_at: date_parse(c.get("created_time")),
            hidden: truthy(c.get("is_hidden")),
            mine: opt_string(c.pointer("/from/id")).as_deref() == Some(ctx.external_id.as_str()),
        })
        .collect())
}

pub fn comment_reply(ctx: &Ctx, comment_id: &str, text: &str) -> Result<Value> {
    net::post(graph(&format!("{comment_id}/comments")))
        .bearer(&ctx.token())
        .json(json!({ "message": text }))
        .get_json()
}

pub fn comment_hide(ctx: &Ctx, comment_id: &str, hidden: bool) -> Result<()> {
    net::post(graph(comment_id)).bearer(&ctx.token()).json(json!({ "is_hidden": hidden })).send()?;
    Ok(())
}

pub fn comment_like(ctx: &Ctx, comment_id: &str) -> Result<()> {
    net::post(graph(&format!("{comment_id}/likes"))).bearer(&ctx.token()).get_json()?;
    Ok(())
}

pub fn comment_remove(ctx: &Ctx, comment_id: &str) -> Result<()> {
    net::req("DELETE", graph(comment_id)).bearer(&ctx.token()).send()?;
    Ok(())
}
