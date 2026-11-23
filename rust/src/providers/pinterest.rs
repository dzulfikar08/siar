//! Pinterest.

use base64::Engine;
use serde_json::{json, Map, Value};

use super::{expires_at, id_of, opt_string, or_zero, poll, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::{kind_of, local_file, media_bytes};
use crate::net::{self, Part, PartData};
use crate::util::{iso, js_string, now, permanent, some, str_of, truthy, Error, Result};

const API: &str = "https://api.pinterest.com/v5";

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://www.pinterest.com/oauth/".into(),
        token_url: "https://api.pinterest.com/v5/oauth/token".into(),
        scopes: vec!["boards:read", "pins:read", "pins:write", "user_accounts:read"],
        env: "PINTEREST",
        pkce: false,
        scope_sep: ",",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: true,
        client_id_param: "client_id",
    }
}

fn boards(token: &str) -> Result<Vec<Value>> {
    let b = net::get(format!("{API}/boards"))
        .bearer(token)
        .query(&[("page_size", Some("100".into()))])
        .get_json()?;
    Ok(b["items"].as_array().cloned().unwrap_or_default())
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default();
    let me = net::get(format!("{API}/user_account")).bearer(token).get_json()?;
    let list: Vec<Value> = boards(token)?.iter().map(|b| json!({ "id": b.get("id"), "name": b.get("name") })).collect();
    let mut secret = Map::new();
    put(&mut secret, "access_token", t.get("access_token").cloned());
    put(&mut secret, "refresh_token", t.get("refresh_token").cloned());
    let name = [me.get("business_name"), me.get("username")]
        .into_iter()
        .find(|v| truthy(*v))
        .flatten()
        .map(js_string)
        .unwrap_or_default();
    Ok(vec![Connected {
        external_id: some(me.get("id")).or(me.get("username")).map(js_string).unwrap_or_else(|| "undefined".into()),
        name,
        handle: opt_string(me.get("username")),
        secret: Value::Object(secret),
        meta: Some(json!({ "boards": list })),
        expires_at: expires_at(t),
    }])
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let boards: Vec<Value> = ctx.meta.get("boards").and_then(|b| b.as_array()).cloned().unwrap_or_default();
    let board_id = match ctx.opt("board_id") {
        Some(b) => Some(b.clone()),
        None if boards.len() == 1 => boards[0].get("id").cloned(),
        None => None,
    };
    let Some(board_id) = board_id.filter(|b| truthy(Some(b))) else {
        permanent!("options.board_id is required; boards: {}", Value::Array(boards));
    };
    let media_source = if let Some(video) = ctx.media.iter().find(|m| kind_of(&m.mime) == "video") {
        let reg = net::post(format!("{API}/media"))
            .bearer(&token)
            .json(json!({ "media_type": "video" }))
            .get_json()?;
        let mut parts = vec![];
        for (k, v) in reg.get("upload_parameters").and_then(|p| p.as_object()).into_iter().flatten() {
            parts.push(Part::text(k, js_string(v)));
        }
        parts.push(Part::file("file", &video.filename, Some(&video.mime), PartData::Local(local_file(video)?)));
        net::post(reg.get("upload_url").map(js_string).unwrap_or_default())
            .multipart(parts)
            .timeout_secs(1800)
            .send()?;
        let media_id = reg.get("media_id").map(js_string).unwrap_or_default();
        poll(5000, 10 * 60_000, "Pinterest video processing", || {
            let s = net::get(format!("{API}/media/{media_id}")).bearer(&token).get_json()?;
            match str_of(&s, "status") {
                Some("failed") => Err(Error::Permanent("Pinterest video processing failed".into())),
                Some("succeeded") => Ok(Some(())),
                _ => Ok(None),
            }
        })?;
        let mut o = Map::new();
        o.insert("source_type".into(), "video_id".into());
        o.insert("media_id".into(), reg.get("media_id").cloned().unwrap_or_default());
        if truthy(ctx.opt("cover_url")) {
            o.insert("cover_image_url".into(), ctx.opt("cover_url").cloned().unwrap_or_default());
        } else {
            o.insert("cover_image_key_frame_time".into(), 0.into());
        }
        Value::Object(o)
    } else {
        let mut items = vec![];
        for m in &ctx.media {
            items.push(json!({ "content_type": m.mime, "data": base64::engine::general_purpose::STANDARD.encode(media_bytes(m)?) }));
        }
        if items.len() == 1 {
            let mut o = Map::new();
            o.insert("source_type".into(), "image_base64".into());
            if let Some(i) = items[0].as_object() {
                o.extend(i.clone());
            }
            Value::Object(o)
        } else {
            json!({ "source_type": "multiple_image_base64", "items": items })
        }
    };
    let mut body = Map::new();
    body.insert("board_id".into(), board_id);
    put(
        &mut body,
        "title",
        ctx.opt("title")
            .map(|t| Value::from(String::from_utf16_lossy(&js_string(t).encode_utf16().take(100).collect::<Vec<u16>>()))),
    );
    body.insert("description".into(), ctx.text.clone().into());
    put(&mut body, "link", ctx.opt("link").cloned());
    put(&mut body, "alt_text", ctx.media.first().and_then(|m| m.alt.clone()).map(Value::from));
    body.insert("media_source".into(), media_source);
    let pin = net::post(format!("{API}/pins")).bearer(&token).json(Value::Object(body)).get_json()?;
    let id = id_of(&pin);
    Ok(Published {
        url: Some(format!("https://www.pinterest.com/pin/{id}/")),
        id,
    })
}

pub fn info(ctx: &Ctx) -> Result<Value> {
    let list: Vec<Value> = boards(&ctx.token())?
        .iter()
        .map(|b| {
            let mut o = Map::new();
            for k in ["id", "name", "privacy"] {
                put(&mut o, k, b.get(k).cloned());
            }
            Value::Object(o)
        })
        .collect();
    Ok(json!({ "boards": list }))
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{API}/pins/{id}")).bearer(&ctx.token()).send()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let day = |d: i64| iso(now() - d * 86_400_000)[..10].to_string();
    let r = net::get(format!("{API}/pins/{id}/analytics"))
        .bearer(&ctx.token())
        .query(&[
            ("start_date", Some(day(89))),
            ("end_date", Some(day(0))),
            ("metric_types", Some("IMPRESSION,SAVE,PIN_CLICK,OUTBOUND_CLICK".into())),
        ])
        .get_json()?;
    let s = some(r.pointer("/all/lifetime_metrics"))
        .or(some(r.pointer("/all/summary_metrics")))
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "impressions": or_zero(s.get("IMPRESSION")),
        "saves": or_zero(s.get("SAVE")),
        "pin_clicks": or_zero(s.get("PIN_CLICK")),
        "outbound_clicks": or_zero(s.get("OUTBOUND_CLICK")),
    }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let me = net::get(format!("{API}/user_account")).bearer(&ctx.token()).get_json()?;
    Ok(json!({
        "followers": or_zero(me.get("follower_count")),
        "following": or_zero(me.get("following_count")),
        "pins": or_zero(me.get("pin_count")),
        "monthly_views": or_zero(me.get("monthly_views")),
    }))
}
