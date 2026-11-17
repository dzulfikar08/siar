//! TikTok (src/providers/tiktok.ts). Video chunks stream from disk.

use serde_json::{json, Map, Value};

use super::{expires_at, opt_string, or_zero, poll, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::{assert_public_url, kind_of, local_file};
use crate::net::{self, Body};
use crate::util::{js_string, permanent, some, str_of, truthy, Error, Result};

const API: &str = "https://open.tiktokapis.com/v2";
const MB: u64 = 1024 * 1024;

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://www.tiktok.com/v2/auth/authorize/".into(),
        token_url: "https://open.tiktokapis.com/v2/oauth/token/".into(),
        scopes: vec![
            "user.info.basic",
            "user.info.profile",
            "user.info.stats",
            "video.publish",
            "video.upload",
            "video.list",
        ],
        env: "TIKTOK",
        pkce: false,
        scope_sep: ",",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: false,
        client_id_param: "client_key",
    }
}

fn tt(token: &str, path: &str, body: Option<Value>, query: &[(&str, Option<String>)]) -> Result<Value> {
    let mut rq = if body.is_some() {
        net::post(format!("{API}{path}"))
    } else {
        net::get(format!("{API}{path}"))
    };
    rq = rq.bearer(token).query(query);
    if let Some(b) = body {
        rq = rq.json(b);
    }
    let r = rq.get_json()?;
    if truthy(r.get("error")) && str_of(&r["error"], "code") != Some("ok") {
        permanent!(
            "TikTok: {} {}",
            r["error"].get("code").map(js_string).unwrap_or_else(|| "undefined".into()),
            r["error"].get("message").map(js_string).unwrap_or_else(|| "undefined".into())
        );
    }
    Ok(r.get("data").cloned().unwrap_or(Value::Null))
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default();
    let u = tt(token, "/user/info/", None, &[("fields", Some("open_id,display_name,username".into()))])?["user"].clone();
    let mut secret = Map::new();
    put(&mut secret, "access_token", t.get("access_token").cloned());
    put(&mut secret, "refresh_token", t.get("refresh_token").cloned());
    Ok(vec![Connected {
        external_id: u.get("open_id").map(js_string).unwrap_or_default(),
        name: opt_string(u.get("display_name")).unwrap_or_default(),
        handle: opt_string(u.get("username")),
        secret: Value::Object(secret),
        meta: None,
        expires_at: expires_at(t),
    }])
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let creator = tt(&token, "/post/publish/creator_info/query/", Some(json!({})), &[])?;
    let allowed: Vec<String> = creator
        .get("privacy_level_options")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(js_string).collect())
        .unwrap_or_default();
    let privacy = ctx.opt_str("privacy").unwrap_or_else(|| {
        if allowed.iter().any(|a| a == "PUBLIC_TO_EVERYONE") {
            "PUBLIC_TO_EVERYONE".into()
        } else {
            "SELF_ONLY".into()
        }
    });
    if !allowed.is_empty() && !allowed.contains(&privacy) {
        permanent!("TikTok privacy {privacy} not allowed for this creator; allowed: {}", allowed.join(", "));
    }
    let flag = |k: &str| truthy(ctx.opt(k));
    let mut flags = Map::new();
    flags.insert("privacy_level".into(), privacy.into());
    flags.insert("disable_comment".into(), flag("disable_comment").into());
    flags.insert("brand_content_toggle".into(), flag("brand_content").into());
    flags.insert("brand_organic_toggle".into(), false.into());
    flags.insert("is_aigc".into(), flag("is_aigc").into());
    let video = ctx.media.iter().find(|m| kind_of(&m.mime) == "video");
    let publish_id;
    if let Some(video) = video {
        if ctx.media.len() > 1 {
            permanent!("TikTok takes one video, or only images");
        }
        let file = local_file(video)?;
        let size = file.len;
        let chunk = if size <= 64 * MB { size } else { 10 * MB };
        let count = size.checked_div(chunk).unwrap_or(0);
        let mut post_info = Map::new();
        post_info.insert("title".into(), ctx.text.clone().into());
        post_info.insert("disable_duet".into(), flag("disable_duet").into());
        post_info.insert("disable_stitch".into(), flag("disable_stitch").into());
        post_info.extend(flags);
        let init = tt(
            &token,
            "/post/publish/video/init/",
            Some(json!({
                "post_info": post_info,
                "source_info": { "source": "FILE_UPLOAD", "video_size": size, "chunk_size": chunk, "total_chunk_count": count },
            })),
            &[],
        )?;
        publish_id = init.get("publish_id").map(js_string).unwrap_or_default();
        let upload_url = init.get("upload_url").map(js_string).unwrap_or_default();
        for i in 0..count {
            let start = i * chunk;
            let end = if i == count - 1 { size } else { start + chunk };
            net::req("PUT", &upload_url)
                .header("content-type", video.mime.clone())
                .header("content-range", format!("bytes {start}-{}/{size}", end - 1))
                .body(Body::File {
                    path: file.path.clone(),
                    offset: start,
                    len: end - start,
                })
                .timeout_secs(900)
                .send()?;
        }
    } else {
        for m in &ctx.media {
            assert_public_url(m, "TikTok")?;
        }
        let title: String = ctx
            .opt("title")
            .map(|t| String::from_utf16_lossy(&js_string(t).encode_utf16().take(90).collect::<Vec<u16>>()))
            .unwrap_or_default();
        let mut post_info = Map::new();
        post_info.insert("title".into(), title.into());
        post_info.insert("description".into(), ctx.text.clone().into());
        post_info.insert("auto_add_music".into(), true.into());
        post_info.extend(flags);
        let urls: Vec<String> = ctx.media.iter().map(|m| m.url.clone()).collect();
        let init = tt(
            &token,
            "/post/publish/content/init/",
            Some(json!({
                "post_info": post_info,
                "source_info": { "source": "PULL_FROM_URL", "photo_images": urls, "photo_cover_index": 0 },
                "post_mode": "DIRECT_POST",
                "media_type": "PHOTO",
            })),
            &[],
        )?;
        publish_id = init.get("publish_id").map(js_string).unwrap_or_default();
    }
    let status = poll(5000, 20 * 60_000, "TikTok processing", || {
        let s = tt(&token, "/post/publish/status/fetch/", Some(json!({ "publish_id": publish_id })), &[])?;
        match str_of(&s, "status") {
            Some("FAILED") => Err(Error::Permanent(format!(
                "TikTok publish failed: {}",
                s.get("fail_reason").map(js_string).unwrap_or_else(|| "undefined".into())
            ))),
            Some("PUBLISH_COMPLETE") => Ok(Some(s)),
            _ => Ok(None),
        }
    })?;
    let post_id = some(status.pointer("/publicaly_available_post_id/0")).map(js_string);
    let handle = ctx.handle.clone().unwrap_or_else(|| "null".into());
    let kind = if video.is_some() { "video" } else { "photo" };
    Ok(Published {
        url: post_id.as_ref().map(|p| format!("https://www.tiktok.com/@{handle}/{kind}/{p}")),
        id: post_id.unwrap_or(publish_id),
    })
}

pub fn info(ctx: &Ctx) -> Result<Value> {
    let c = tt(&ctx.token(), "/post/publish/creator_info/query/", Some(json!({})), &[])?;
    Ok(json!({ "creator_info": c, "note": "privacy_level must be one of privacy_level_options; ask the user which, never pick it yourself" }))
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let d = tt(
        &ctx.token(),
        "/video/query/?fields=id,view_count,like_count,comment_count,share_count",
        Some(json!({ "filters": { "video_ids": [id] } })),
        &[],
    )?;
    let v = d.pointer("/videos/0").cloned().unwrap_or_default();
    Ok(json!({
        "views": or_zero(v.get("view_count")),
        "likes": or_zero(v.get("like_count")),
        "comments": or_zero(v.get("comment_count")),
        "shares": or_zero(v.get("share_count")),
    }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let u = tt(
        &ctx.token(),
        "/user/info/",
        None,
        &[("fields", Some("follower_count,following_count,likes_count,video_count".into()))],
    )?["user"]
        .clone();
    let mut o = Map::new();
    for (k, src) in [
        ("followers", "follower_count"),
        ("following", "following_count"),
        ("likes", "likes_count"),
        ("videos", "video_count"),
    ] {
        put(&mut o, k, u.get(src).cloned());
    }
    Ok(Value::Object(o))
}
