//! Instagram API with Unified Login (Facebook Login for Business), on graph.facebook.com
//! (src/providers/instagram.ts). The token never expires; no Facebook Page is needed.

use serde_json::{json, Map, Value};

use super::{id_of, insight_map, opt_string, or_zero, poll, put, Conversation, DirectMessage, DmPayload, OAuthSpec, Published, RemoteComment, RemotePost};
use crate::accounts::{Connected, Ctx};
use crate::config::{app_creds, env, redirect_uri};
use crate::db::MediaRow;
use crate::media::{assert_public_url, get_media, kind_of};
use crate::net;
use crate::util::{bail, date_parse, js_string, permanent, some, str_of, truthy, Obj, Result};

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
        client_auth_basic: false,
        client_id_param: "client_id",
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
    let creation = if story {
        if ctx.media.len() != 1 {
            permanent!("An Instagram story takes exactly one image or video");
        }
        let mut extra = Map::new();
        extra.insert("media_type".into(), "STORIES".into());
        put(&mut extra, "is_ai_generated", ai);
        create(item(&ctx.media[0], extra))?
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
        create(item(m, extra))?
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
        create(Value::Object(body))?
    };
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

pub fn stories(host: &str, ctx: &Ctx) -> Result<Vec<RemotePost>> {
    let token = ctx.token();
    let r = net::get(format!("{host}/{}/stories", ctx.external_id))
        .bearer(&token)
        .query(&[("fields", Some("id,media_type,permalink,timestamp".into()))])
        .get_json()?;
    let mut out = vec![];
    for s in r["data"].as_array().into_iter().flatten() {
        let metrics = net::get(format!("{host}/{}/insights", id_of(s)))
            .bearer(&token)
            .query(&[("metric", Some("views,reach,replies,navigation".into()))])
            .get_json()
            .ok()
            .filter(|i| i.get("data").map(|d| d.is_array()).unwrap_or(false))
            .map(|i| insight_map(i.get("data")))
            .unwrap_or_else(|| json!({}));
        out.push(RemotePost {
            id: id_of(s),
            url: opt_string(s.get("permalink")),
            text: None,
            kind: Some(format!(
                "story_{}",
                s.get("media_type").map(js_string).unwrap_or_else(|| "undefined".into()).to_lowercase()
            )),
            username: None,
            created_at: date_parse(s.get("timestamp")),
            metrics: Some(metrics),
        });
    }
    Ok(out)
}

pub fn comment_reply(host: &str, ctx: &Ctx, comment_id: &str, text: &str) -> Result<Value> {
    net::post(format!("{host}/{comment_id}/replies"))
        .bearer(&ctx.token())
        .json(json!({ "message": text }))
        .get_json()
}

pub fn comment_hide(host: &str, ctx: &Ctx, comment_id: &str, hidden: bool) -> Result<()> {
    net::post(format!("{host}/{comment_id}"))
        .bearer(&ctx.token())
        .json(json!({ "hide": hidden }))
        .send()?;
    Ok(())
}

pub fn comment_remove(host: &str, ctx: &Ctx, comment_id: &str) -> Result<()> {
    net::req("DELETE", format!("{host}/{comment_id}")).bearer(&ctx.token()).send()?;
    Ok(())
}

// ---------- DMs (Instagram messaging; the Facebook-Page variant sends as the Page) ----------

fn dm_me(ctx: &Ctx, page: bool) -> String {
    if page {
        str_of(&ctx.meta, "pageId").map(|s| s.to_string()).unwrap_or_else(|| ctx.external_id.clone())
    } else {
        ctx.external_id.clone()
    }
}

pub fn conversations(host: &str, ctx: &Ctx, page: bool, limit: i64) -> Result<Vec<Conversation>> {
    let r = net::get(format!("{host}/{}/conversations", dm_me(ctx, page)))
        .bearer(&ctx.token())
        .query(&[
            ("platform", Some("instagram".into())),
            ("fields", Some("id,updated_time,participants,messages.limit(1){message,from}".into())),
            ("limit", Some(limit.min(50).to_string())),
        ])
        .get_json()?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let other = c.pointer("/participants/data").and_then(|d| d.as_array()).and_then(|ps| {
                ps.iter().find(|p| {
                    let u = opt_string(p.get("username"));
                    opt_string(p.get("id")).as_deref() != Some(ctx.external_id.as_str()) && !(u.is_some() && u == ctx.handle)
                })
            });
            Conversation {
                id: id_of(c),
                user_id: other.and_then(|o| opt_string(o.get("id"))),
                username: other.and_then(|o| opt_string(o.get("username"))),
                updated_at: date_parse(c.get("updated_time")),
                last_message: opt_string(c.pointer("/messages/data/0/message")),
            }
        })
        .collect())
}

pub fn messages(host: &str, ctx: &Ctx, conversation_id: &str, limit: i64) -> Result<Vec<DirectMessage>> {
    let fields = format!(
        "messages.limit({}){{id,created_time,from,message,story,attachments,is_unsupported}}",
        limit.min(50)
    );
    let r = net::get(format!("{host}/{conversation_id}"))
        .bearer(&ctx.token())
        .query(&[("fields", Some(fields))])
        .get_json()?;
    Ok(r.pointer("/messages/data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .map(|m| {
            let from_id = opt_string(m.pointer("/from/id"));
            let from_user = opt_string(m.pointer("/from/username"));
            let mine = from_id.as_deref() == Some(ctx.external_id.as_str()) || (from_user.is_some() && from_user == ctx.handle);
            let story = if truthy(m.pointer("/story/mention")) {
                Some(Obj::new().set("kind", "mention").defined("url", m.pointer("/story/mention/link")).build())
            } else if truthy(m.pointer("/story/reply_to")) {
                Some(Obj::new().set("kind", "reply").defined("url", m.pointer("/story/reply_to/link")).build())
            } else {
                None
            };
            let attachments = m
                .pointer("/attachments/data")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    [a.pointer("/image_data/url"), a.pointer("/video_data/url"), a.get("file_url")]
                        .into_iter()
                        .find_map(|v| some(v).filter(|v| truthy(Some(v))))
                        .map(js_string)
                })
                .collect();
            DirectMessage {
                id: id_of(m),
                from_id,
                username: from_user,
                text: opt_string(m.get("message")),
                created_at: date_parse(m.get("created_time")),
                mine,
                story,
                attachments,
            }
        })
        .collect())
}

fn dm_message(msg: &DmPayload) -> Value {
    if msg.buttons.is_empty() {
        return json!({ "text": msg.text });
    }
    let buttons: Vec<Value> = msg
        .buttons
        .iter()
        .take(3)
        .map(|b| match &b.url {
            Some(u) => json!({ "type": "web_url", "url": u, "title": b.title }),
            None => {
                let mut o = Map::new();
                o.insert("type".into(), "postback".into());
                put(&mut o, "payload", b.payload.clone().map(Value::from));
                o.insert("title".into(), b.title.clone().into());
                Value::Object(o)
            }
        })
        .collect();
    let text = String::from_utf16_lossy(&msg.text.encode_utf16().take(640).collect::<Vec<u16>>());
    json!({ "attachment": { "type": "template", "payload": { "template_type": "button", "text": text, "buttons": buttons } } })
}

pub fn send(host: &str, ctx: &Ctx, page: bool, to_id: Option<&str>, comment_id: Option<&str>, msg: &DmPayload) -> Result<Value> {
    let recipient = match comment_id {
        Some(c) => json!({ "comment_id": c }),
        None => {
            let mut o = Map::new();
            put(&mut o, "id", to_id.map(Value::from));
            Value::Object(o)
        }
    };
    let url = format!("{host}/{}/messages", dm_me(ctx, page));
    let out = |r: Value| {
        Obj::new()
            .defined("id", r.get("message_id"))
            .defined("recipientId", r.get("recipient_id"))
            .build()
    };
    match net::post(&url)
        .bearer(&ctx.token())
        .json(json!({ "recipient": recipient, "message": dm_message(msg) }))
        .get_json()
    {
        Ok(r) => Ok(out(r)),
        Err(e) => {
            // Some contexts (old clients, private replies) reject templates: plain text with the links inline.
            if msg.buttons.is_empty() {
                return Err(e);
            }
            let mut parts = vec![msg.text.clone()];
            parts.extend(msg.buttons.iter().filter_map(|b| b.url.as_ref().map(|u| format!("{}: {u}", b.title))));
            let text: String = String::from_utf16_lossy(&parts.join("\n\n").encode_utf16().take(1000).collect::<Vec<u16>>());
            let r = net::post(&url)
                .bearer(&ctx.token())
                .json(json!({ "recipient": recipient, "message": { "text": text } }))
                .get_json()?;
            Ok(out(r))
        }
    }
}

pub fn react(host: &str, ctx: &Ctx, page: bool, user_id: &str, message_id: &str) -> Result<()> {
    net::post(format!("{host}/{}/messages", dm_me(ctx, page)))
        .bearer(&ctx.token())
        .json(json!({ "recipient": { "id": user_id }, "sender_action": "react", "payload": { "message_id": message_id, "reaction": "love" } }))
        .send()?;
    Ok(())
}

/// (username, name, follows)
pub fn user_profile(host: &str, ctx: &Ctx, user_id: &str) -> Result<(Option<String>, Option<String>, Option<bool>)> {
    let p = net::get(format!("{host}/{user_id}"))
        .bearer(&ctx.token())
        .query(&[("fields", Some("username,name,is_user_follow_business".into()))])
        .get_json()?;
    Ok((
        opt_string(p.get("username")),
        opt_string(p.get("name")),
        p.get("is_user_follow_business").and_then(|v| v.as_bool()),
    ))
}

pub fn follows(host: &str, ctx: &Ctx, user_id: &str) -> Result<Option<bool>> {
    let p = net::get(format!("{host}/{user_id}"))
        .bearer(&ctx.token())
        .query(&[("fields", Some("is_user_follow_business".into()))])
        .get_json()?;
    Ok(p.get("is_user_follow_business").and_then(|v| v.as_bool()))
}

// ---------- Instagram API with Instagram Login (older apps) ----------

/// graph.instagram.com, or SIAR_IG_BASE.
pub fn login_host() -> String {
    env("SIAR_IG_BASE").unwrap_or_else(|| format!("https://graph.instagram.com/{}", ig_version()))
}

pub fn login_spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://www.instagram.com/oauth/authorize".into(),
        token_url: "https://api.instagram.com/oauth/access_token".into(),
        scopes: vec![
            "instagram_business_basic",
            "instagram_business_content_publish",
            "instagram_business_manage_comments",
            "instagram_business_manage_messages",
            "instagram_business_manage_insights",
        ],
        env: "INSTAGRAM_LOGIN",
        pkce: false,
        scope_sep: ",",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: false,
        client_id_param: "client_id",
    }
}

pub fn login_exchange(code: &str) -> Result<Value> {
    let (id, secret) = app_creds("INSTAGRAM")?;
    let code = code.strip_suffix("#_").unwrap_or(code);
    let short = net::post(login_spec().token_url)
        .form(&[
            ("client_id", Some(id)),
            ("client_secret", Some(secret.clone())),
            ("grant_type", Some("authorization_code".into())),
            ("redirect_uri", Some(redirect_uri("instagram-login"))),
            ("code", Some(code.into())),
        ])
        .get_json()?;
    let short_token = some(short.get("access_token")).or(short.pointer("/data/0/access_token")).map(js_string);
    net::get("https://graph.instagram.com/access_token")
        .query(&[
            ("grant_type", Some("ig_exchange_token".into())),
            ("client_secret", Some(secret)),
            ("access_token", short_token),
        ])
        .get_json()
}

fn expires_in_or_60d(t: &Value) -> i64 {
    let s = some(t.get("expires_in")).map(|v| crate::util::to_num(Some(v))).unwrap_or(5_184_000.0);
    crate::util::now() + (s * 1000.0) as i64
}

pub fn login_profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default().to_string();
    let me = net::get(format!("{}/me", login_host()))
        .bearer(&token)
        .query(&[("fields", Some("user_id,username,name".into()))])
        .get_json()?;
    let name = [me.get("name"), me.get("username")]
        .into_iter()
        .find(|v| truthy(*v))
        .flatten()
        .map(js_string)
        .unwrap_or_default();
    Ok(vec![Connected {
        external_id: me.get("user_id").map(js_string).unwrap_or_else(|| "undefined".into()),
        name,
        handle: opt_string(me.get("username")),
        secret: json!({ "access_token": token }),
        meta: Some(Obj::new().defined("appScopedId", me.get("id")).build()),
        expires_at: Some(expires_in_or_60d(t)),
    }])
}

pub fn login_refresh(s: &Value) -> Result<(Value, Option<i64>)> {
    let r = net::get("https://graph.instagram.com/refresh_access_token")
        .query(&[
            ("grant_type", Some("ig_refresh_token".into())),
            ("access_token", opt_string(s.get("access_token"))),
        ])
        .get_json()?;
    let mut secret = s.as_object().cloned().unwrap_or_default();
    secret.insert("access_token".into(), r.get("access_token").cloned().unwrap_or(Value::Null));
    Ok((Value::Object(secret), Some(expires_in_or_60d(&r))))
}
