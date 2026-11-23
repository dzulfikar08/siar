//! YouTube. The resumable upload streams the video from disk.

use serde_json::{json, Map, Value};

use super::{expires_at, id_of, opt_string, put, OAuthSpec, Published, RemoteComment, RemotePost};
use crate::accounts::{Connected, Ctx};
use crate::media::{get_media, local_file, media_bytes};
use crate::net::{self, Body};
use crate::util::{date_parse, jnum, js_string, permanent, some, str_of, to_num, truthy, Error, Result};

const YT: &str = "https://www.googleapis.com/youtube/v3";

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token_url: "https://oauth2.googleapis.com/token".into(),
        scopes: vec![
            "openid",
            "email",
            "https://www.googleapis.com/auth/youtube.upload",
            "https://www.googleapis.com/auth/youtube.readonly",
            "https://www.googleapis.com/auth/youtube.force-ssl",
        ],
        env: "GOOGLE",
        pkce: true,
        scope_sep: " ",
        extra_auth_params: vec![("access_type", "offline"), ("prompt", "consent"), ("include_granted_scopes", "true")],
        config_id_env: None,
        client_auth_basic: false,
        client_id_param: "client_id",
    }
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let r = net::get(format!("{YT}/channels"))
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .query(&[("part", Some("snippet".into())), ("mine", Some("true".into()))])
        .get_json()?;
    Ok(r["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let mut s = Map::new();
            put(&mut s, "access_token", t.get("access_token").cloned());
            put(&mut s, "refresh_token", t.get("refresh_token").cloned());
            Connected {
                external_id: id_of(c),
                name: opt_string(c.pointer("/snippet/title")).unwrap_or_default(),
                handle: opt_string(c.pointer("/snippet/customUrl")),
                secret: Value::Object(s),
                meta: None,
                expires_at: expires_at(t),
            }
        })
        .collect())
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    if !ctx.opt("made_for_kids").map(|v| v.is_boolean()).unwrap_or(false) {
        permanent!("options.made_for_kids (true/false) is required for YouTube");
    }
    if !truthy(ctx.opt("title")) {
        permanent!("options.title is required for YouTube");
    }
    let Some(video) = ctx.media.first() else {
        permanent!("YouTube needs a video")
    };
    let file = local_file(video)?;
    let token = ctx.token();
    // String(title).slice(0, 100) counts UTF-16 units.
    let title = String::from_utf16_lossy(&ctx.opt_str("title").unwrap_or_default().encode_utf16().take(100).collect::<Vec<u16>>());
    let mut snippet = Map::new();
    snippet.insert("title".into(), title.into());
    snippet.insert("description".into(), ctx.text.clone().into());
    put(&mut snippet, "tags", ctx.opt("tags").cloned());
    snippet.insert("categoryId".into(), ctx.opt("category_id").cloned().unwrap_or(Value::from("22")));
    let mut status = Map::new();
    status.insert("privacyStatus".into(), ctx.opt("privacy").cloned().unwrap_or(Value::from("public")));
    status.insert("selfDeclaredMadeForKids".into(), ctx.opt("made_for_kids").cloned().unwrap_or_default());
    put(&mut status, "containsSyntheticMedia", ctx.opt("contains_synthetic_media").cloned());
    let notify = ctx.opt("notify_subscribers").map(js_string).unwrap_or_else(|| "true".into());
    let init = net::post("https://www.googleapis.com/upload/youtube/v3/videos")
        .bearer(&token)
        .query(&[
            ("uploadType", Some("resumable".into())),
            ("part", Some("snippet,status".into())),
            ("notifySubscribers", Some(notify)),
        ])
        .header("x-upload-content-type", video.mime.clone())
        .header("x-upload-content-length", file.len.to_string())
        .json(json!({ "snippet": snippet, "status": status }))
        .send()?;
    let location = init
        .header("location")
        .ok_or_else(|| Error::Msg("YouTube returned no upload location".into()))?;
    let v = net::req("PUT", location)
        .header("content-type", video.mime.clone())
        .body(Body::File {
            path: file.path.clone(),
            offset: 0,
            len: file.len,
        })
        .timeout_secs(3600)
        .get_json()?;
    if v.pointer("/status/uploadStatus").and_then(|s| s.as_str()) == Some("rejected") || truthy(v.pointer("/status/rejectionReason")) {
        permanent!(
            "YouTube rejected the upload: {}",
            opt_string(v.pointer("/status/rejectionReason")).unwrap_or_else(|| "undefined".into())
        );
    }
    let vid = id_of(&v);
    if let Some(cover) = ctx.opt_str("cover_media_id").filter(|s| !s.is_empty()) {
        let thumb = get_media(&cover)?;
        // The video is live already; a thumbnail failure must not fail the post.
        let r = media_bytes(&thumb).and_then(|bytes| {
            net::post("https://www.googleapis.com/upload/youtube/v3/thumbnails/set")
                .bearer(&token)
                .query(&[("videoId", Some(vid.clone())), ("uploadType", Some("media".into()))])
                .header("content-type", thumb.mime.clone())
                .body(Body::Bytes(bytes))
                .send()
        });
        if let Err(e) = r {
            eprintln!("[youtube] thumbnail for {vid} failed: {e}");
        }
    }
    Ok(Published {
        url: Some(format!("https://www.youtube.com/watch?v={vid}")),
        id: vid,
    })
}

fn n(v: Option<&Value>) -> Value {
    jnum(to_num(some(v)))
}

pub fn posts(ctx: &Ctx, since: i64, limit: usize) -> Result<Vec<RemotePost>> {
    let token = ctx.token();
    let ch = net::get(format!("{YT}/channels"))
        .bearer(&token)
        .query(&[("part", Some("contentDetails".into())), ("id", Some(ctx.external_id.clone()))])
        .get_json()?;
    let Some(uploads) = opt_string(ch.pointer("/items/0/contentDetails/relatedPlaylists/uploads")).filter(|s| !s.is_empty()) else {
        return Ok(vec![]);
    };
    let mut ids: Vec<String> = vec![];
    let mut page_token: Option<String> = None;
    'outer: loop {
        let r = net::get(format!("{YT}/playlistItems"))
            .bearer(&token)
            .query(&[
                ("part", Some("contentDetails".into())),
                ("playlistId", Some(uploads.clone())),
                ("maxResults", Some("50".into())),
                ("pageToken", page_token.clone()),
            ])
            .get_json()?;
        for it in r["items"].as_array().into_iter().flatten() {
            if date_parse(it.pointer("/contentDetails/videoPublishedAt")) < since || ids.len() >= limit {
                break 'outer;
            }
            ids.push(opt_string(it.pointer("/contentDetails/videoId")).unwrap_or_default());
        }
        page_token = opt_string(r.get("nextPageToken")).filter(|s| !s.is_empty());
        if page_token.is_none() {
            break;
        }
    }
    let mut out = vec![];
    for chunk in ids.chunks(50) {
        let r = net::get(format!("{YT}/videos"))
            .bearer(&token)
            .query(&[("part", Some("snippet,statistics,contentDetails".into())), ("id", Some(chunk.join(",")))])
            .get_json()?;
        for v in r["items"].as_array().into_iter().flatten() {
            let s = v.get("statistics").cloned().unwrap_or_default();
            let id = id_of(v);
            out.push(RemotePost {
                url: Some(format!("https://www.youtube.com/watch?v={id}")),
                id,
                text: opt_string(v.pointer("/snippet/title")),
                kind: Some("video".into()),
                username: None,
                created_at: date_parse(v.pointer("/snippet/publishedAt")),
                metrics: Some(json!({ "views": n(s.get("viewCount")), "likes": n(s.get("likeCount")), "comments": n(s.get("commentCount")) })),
            });
        }
    }
    Ok(out)
}

pub fn comments(ctx: &Ctx, video_id: &str) -> Result<Vec<RemoteComment>> {
    let r = net::get(format!("{YT}/commentThreads"))
        .bearer(&ctx.token())
        .query(&[
            ("part", Some("snippet,replies".into())),
            ("videoId", Some(video_id.into())),
            ("maxResults", Some("100".into())),
            ("order", Some("time".into())),
            ("textFormat", Some("plainText".into())),
        ])
        .get_json()?;
    let map = |c: &Value, parent: Option<String>| {
        let author = opt_string(c.pointer("/snippet/authorChannelId/value"));
        RemoteComment {
            id: id_of(c),
            post_id: video_id.to_string(),
            parent_id: parent,
            text: opt_string(c.pointer("/snippet/textDisplay"))
                .or_else(|| opt_string(c.pointer("/snippet/textOriginal")))
                .unwrap_or_default(),
            mine: author.as_deref() == Some(ctx.external_id.as_str()),
            author_id: author,
            username: opt_string(c.pointer("/snippet/authorDisplayName")),
            created_at: date_parse(c.pointer("/snippet/publishedAt")),
            hidden: false,
        }
    };
    let mut out = vec![];
    for t in r["items"].as_array().into_iter().flatten() {
        out.push(map(t.pointer("/snippet/topLevelComment").unwrap_or(&Value::Null), None));
        for rep in t.pointer("/replies/comments").and_then(|c| c.as_array()).into_iter().flatten() {
            out.push(map(rep, Some(id_of(t))));
        }
    }
    Ok(out)
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{YT}/videos"))
        .bearer(&ctx.token())
        .query(&[("id", Some(id.into()))])
        .send()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let r = net::get(format!("{YT}/videos"))
        .bearer(&ctx.token())
        .query(&[("part", Some("statistics".into())), ("id", Some(id.into()))])
        .get_json()?;
    let s = r.pointer("/items/0/statistics").cloned().unwrap_or_default();
    Ok(json!({ "views": n(s.get("viewCount")), "likes": n(s.get("likeCount")), "comments": n(s.get("commentCount")), "favorites": n(s.get("favoriteCount")) }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{YT}/channels"))
        .bearer(&ctx.token())
        .query(&[("part", Some("statistics".into())), ("id", Some(ctx.external_id.clone()))])
        .get_json()?;
    let s = r.pointer("/items/0/statistics").cloned().unwrap_or_default();
    Ok(json!({ "subscribers": n(s.get("subscriberCount")), "views": n(s.get("viewCount")), "videos": n(s.get("videoCount")) }))
}

pub fn comment_reply(ctx: &Ctx, comment_id: &str, text: &str) -> Result<Value> {
    net::post(format!("{YT}/comments"))
        .bearer(&ctx.token())
        .query(&[("part", Some("snippet".into()))])
        .json(json!({ "snippet": { "parentId": comment_id, "textOriginal": text } }))
        .get_json()
}

pub fn comment_hide(ctx: &Ctx, comment_id: &str, hidden: bool) -> Result<()> {
    net::post(format!("{YT}/comments/setModerationStatus"))
        .bearer(&ctx.token())
        .query(&[
            ("id", Some(comment_id.into())),
            ("moderationStatus", Some(if hidden { "heldForReview" } else { "published" }.into())),
        ])
        .send()?;
    Ok(())
}

pub fn comment_remove(ctx: &Ctx, comment_id: &str) -> Result<()> {
    net::req("DELETE", format!("{YT}/comments"))
        .bearer(&ctx.token())
        .query(&[("id", Some(comment_id.into()))])
        .send()?;
    Ok(())
}
