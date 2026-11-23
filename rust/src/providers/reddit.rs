//! Reddit.

use serde_json::{json, Map, Value};

use super::{expires_at, opt_string, or_zero, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::local_file;
use crate::net::{self, Part, PartData};
use crate::util::{js_string, now, permanent, some, str_of, truthy, Result};

const OAUTH: &str = "https://oauth.reddit.com";

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://www.reddit.com/api/v1/authorize".into(),
        token_url: "https://www.reddit.com/api/v1/access_token".into(),
        scopes: vec!["identity", "submit", "read"],
        env: "REDDIT",
        pkce: false,
        scope_sep: " ",
        extra_auth_params: vec![("duration", "permanent")],
        config_id_env: None,
        client_auth_basic: true,
        client_id_param: "client_id",
    }
}

fn ua(user: Option<&str>) -> String {
    format!("server:siar:0.1 (by /u/{})", user.unwrap_or("siar"))
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let me = net::get(format!("{OAUTH}/api/v1/me"))
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .header("user-agent", ua(None))
        .get_json()?;
    let mut secret = Map::new();
    put(&mut secret, "access_token", t.get("access_token").cloned());
    put(&mut secret, "refresh_token", t.get("refresh_token").cloned());
    Ok(vec![Connected {
        external_id: me.get("id").map(js_string).unwrap_or_default(),
        name: opt_string(me.get("name")).unwrap_or_default(),
        handle: opt_string(me.get("name")),
        secret: Value::Object(secret),
        meta: None,
        expires_at: expires_at(t),
    }])
}

/// A form value as URLSearchParams writes it (undefined is skipped).
fn fv(v: Option<&Value>) -> Option<String> {
    some(v).map(js_string)
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let agent = ua(ctx.handle.as_deref());
    let sr_raw = ctx.opt("subreddit").map(js_string).unwrap_or_default();
    let sr = sr_raw.strip_prefix("/r/").or_else(|| sr_raw.strip_prefix("r/")).unwrap_or(&sr_raw).to_string();
    if sr.is_empty() || !truthy(ctx.opt("title")) {
        permanent!("options.subreddit and options.title are required for Reddit");
    }
    let title = String::from_utf16_lossy(&ctx.opt_str("title").unwrap_or_default().encode_utf16().take(300).collect::<Vec<u16>>());
    let mut form: Vec<(&str, Option<String>)> = vec![
        ("sr", Some(sr)),
        ("title", Some(title)),
        ("api_type", Some("json".into())),
        ("flair_id", fv(ctx.opt("flair_id"))),
        ("nsfw", fv(ctx.opt("nsfw"))),
        ("spoiler", fv(ctx.opt("spoiler"))),
        ("resubmit", Some("true".into())),
    ];
    if let Some(img) = ctx.media.first() {
        let lease = net::post(format!("{OAUTH}/api/media/asset.json"))
            .bearer(&token)
            .header("user-agent", agent.clone())
            .form(&[("filepath", Some(img.filename.clone())), ("mimetype", Some(img.mime.clone()))])
            .get_json()?;
        let action = lease.pointer("/args/action").map(js_string).unwrap_or_default();
        let action = if action.starts_with("//") { format!("https:{action}") } else { action };
        let fields = lease.pointer("/args/fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        let mut parts: Vec<Part> = fields
            .iter()
            .map(|f| {
                Part::text(
                    &f.get("name").map(js_string).unwrap_or_default(),
                    f.get("value").map(js_string).unwrap_or_default(),
                )
            })
            .collect();
        parts.push(Part::file("file", &img.filename, Some(&img.mime), PartData::Local(local_file(img)?)));
        net::post(&action).multipart(parts).send()?;
        let key = fields
            .iter()
            .find(|f| str_of(f, "name") == Some("key"))
            .and_then(|f| f.get("value"))
            .map(js_string)
            .unwrap_or_default();
        form.push(("kind", Some("image".into())));
        form.push(("url", Some(format!("{action}/{key}"))));
        form.push(("text", if ctx.text.is_empty() { None } else { Some(ctx.text.clone()) }));
    } else if truthy(ctx.opt("url")) {
        form.push(("kind", Some("link".into())));
        form.push(("url", ctx.opt_str("url")));
    } else {
        form.push(("kind", Some("self".into())));
        form.push(("text", Some(ctx.text.clone())));
    }
    let r = net::post(format!("{OAUTH}/api/submit"))
        .bearer(&token)
        .header("user-agent", agent)
        .form(&form)
        .get_json()?;
    let errs = r.pointer("/json/errors").and_then(|e| e.as_array()).cloned().unwrap_or_default();
    if !errs.is_empty() {
        let msg: Vec<String> = errs
            .iter()
            .map(|e| {
                e.as_array()
                    .map(|a| a.iter().map(js_string).collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|| js_string(e))
            })
            .collect();
        permanent!("Reddit: {}", msg.join("; "));
    }
    let d = r.pointer("/json/data").cloned().unwrap_or_default();
    if truthy(d.get("name")) {
        return Ok(Published {
            id: js_string(&d["name"]),
            url: opt_string(d.get("url")),
        });
    }
    // Image posts are processed asynchronously and don't return the post id.
    Ok(Published {
        id: format!("pending:{}", now()),
        url: Some(format!(
            "https://www.reddit.com/user/{}/submitted/",
            ctx.handle.clone().unwrap_or_else(|| "null".into())
        )),
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::post(format!("{OAUTH}/api/del"))
        .bearer(&ctx.token())
        .header("user-agent", ua(ctx.handle.as_deref()))
        .form(&[("id", Some(id.into()))])
        .send()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let r = net::get(format!("{OAUTH}/api/info"))
        .bearer(&ctx.token())
        .header("user-agent", ua(ctx.handle.as_deref()))
        .query(&[("id", Some(id.into()))])
        .get_json()?;
    let p = r.pointer("/data/children/0/data").cloned().unwrap_or_default();
    Ok(json!({ "score": or_zero(p.get("score")), "upvote_ratio": or_zero(p.get("upvote_ratio")), "comments": or_zero(p.get("num_comments")) }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let me = net::get(format!("{OAUTH}/api/v1/me"))
        .bearer(&ctx.token())
        .header("user-agent", ua(ctx.handle.as_deref()))
        .get_json()?;
    Ok(json!({ "link_karma": or_zero(me.get("link_karma")), "comment_karma": or_zero(me.get("comment_karma")) }))
}
