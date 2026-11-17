//! Telegram, Discord and Slack (src/providers/chat.ts).

use serde_json::{json, Map, Value};

use super::{opt_string, put, Published};
use crate::accounts::{Connected, Ctx};
use crate::db::MediaRow;
use crate::media::{kind_of, local_file};
use crate::net::{self, Body, Part, PartData};
use crate::util::{js_string, permanent, some, str_of, truthy, Result};

// ---------- Telegram ----------

fn tg(token: &str, method: &str) -> String {
    format!("https://api.telegram.org/bot{token}/{method}")
}

enum TgBody {
    Json(Value),
    Form(Vec<Part>),
}

fn tg_call(token: &str, method: &str, body: TgBody) -> Result<Value> {
    let rq = net::post(tg(token, method)).timeout_secs(600);
    let r = match body {
        TgBody::Json(v) => rq.json(v),
        TgBody::Form(p) => rq.multipart(p),
    }
    .get_json()?;
    if !truthy(r.get("ok")) {
        permanent!(
            "Telegram {method}: {}",
            r.get("description").map(js_string).unwrap_or_else(|| "undefined".into())
        );
    }
    Ok(r.get("result").cloned().unwrap_or(Value::Null))
}

fn tg_type(m: &MediaRow) -> &'static str {
    match kind_of(&m.mime) {
        "image" => "photo",
        "video" => "video",
        _ => "document",
    }
}

fn tg_url(chat: &Value, message_id: &str) -> Option<String> {
    if truthy(chat.get("username")) {
        return Some(format!("https://t.me/{}/{message_id}", js_string(&chat["username"])));
    }
    let id = chat.get("id").map(js_string).unwrap_or_default();
    id.strip_prefix("-100").map(|rest| format!("https://t.me/c/{rest}/{message_id}"))
}

pub fn telegram_verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let token = f.get("bot_token").map(js_string).unwrap_or_default();
    let chat = tg_call(&token, "getChat", TgBody::Json(json!({ "chat_id": f.get("chat_id") })))?;
    let id = chat.get("id").map(js_string).unwrap_or_default();
    Ok(vec![Connected {
        name: some(chat.get("title"))
            .or(some(chat.get("username")))
            .map(js_string)
            .unwrap_or_else(|| id.clone()),
        handle: opt_string(chat.get("username")),
        secret: json!({ "bot_token": f.get("bot_token"), "chat_id": chat.get("id") }),
        external_id: id,
        meta: None,
        expires_at: None,
    }])
}

pub fn telegram_publish(ctx: &Ctx) -> Result<Published> {
    let token = str_of(&ctx.secret, "bot_token").unwrap_or_default().to_string();
    let chat_id = ctx.secret.get("chat_id").cloned().unwrap_or(Value::Null);
    let mut common = Map::new();
    common.insert("chat_id".into(), chat_id.clone());
    put(&mut common, "parse_mode", ctx.opt("parse_mode").cloned());
    put(&mut common, "disable_notification", ctx.opt("silent").cloned());
    let fits = ctx.text.encode_utf16().count() <= 1024;
    let mut first: Option<Value> = None;
    if ctx.media.len() == 1 {
        let m = &ctx.media[0];
        let mut parts: Vec<Part> = common.iter().map(|(k, v)| Part::text(k, js_string(v))).collect();
        if fits && !ctx.text.is_empty() {
            parts.push(Part::text("caption", ctx.text.clone()));
        }
        let t = tg_type(m);
        parts.push(Part::file(t, &m.filename, Some(&m.mime), PartData::Local(local_file(m)?)));
        let method = format!("send{}{}", t[..1].to_uppercase(), &t[1..]);
        first = Some(tg_call(&token, &method, TgBody::Form(parts))?);
    } else if ctx.media.len() > 1 {
        let mut parts = vec![Part::text("chat_id", js_string(&chat_id))];
        let mut group = vec![];
        for (i, m) in ctx.media.iter().enumerate() {
            parts.push(Part::file(&format!("f{i}"), &m.filename, Some(&m.mime), PartData::Local(local_file(m)?)));
            let mut g = Map::new();
            g.insert("type".into(), tg_type(m).into());
            g.insert("media".into(), format!("attach://f{i}").into());
            if i == 0 && fits && !ctx.text.is_empty() {
                g.insert("caption".into(), ctx.text.clone().into());
                put(&mut g, "parse_mode", ctx.opt("parse_mode").cloned());
            }
            group.push(Value::Object(g));
        }
        parts.push(Part::text("media", Value::Array(group).to_string()));
        first = tg_call(&token, "sendMediaGroup", TgBody::Form(parts))?.get(0).cloned();
    }
    if ctx.media.is_empty() || (!fits && !ctx.text.is_empty()) {
        let mut body = common.clone();
        body.insert("text".into(), ctx.text.clone().into());
        let msg = tg_call(&token, "sendMessage", TgBody::Json(Value::Object(body)))?;
        if first.is_none() {
            first = Some(msg);
        }
    }
    let first = first.unwrap_or_default();
    let mid = first.get("message_id").map(js_string).unwrap_or_else(|| "undefined".into());
    Ok(Published {
        url: tg_url(first.get("chat").unwrap_or(&Value::Null), &mid),
        id: mid,
    })
}

pub fn telegram_remove(ctx: &Ctx, id: &str) -> Result<()> {
    let n: f64 = id.trim().parse().unwrap_or(f64::NAN);
    tg_call(
        str_of(&ctx.secret, "bot_token").unwrap_or_default(),
        "deleteMessage",
        TgBody::Json(json!({ "chat_id": ctx.secret.get("chat_id"), "message_id": crate::util::jnum(n) })),
    )?;
    Ok(())
}

pub fn telegram_account_metrics(ctx: &Ctx) -> Result<Value> {
    let n = tg_call(
        str_of(&ctx.secret, "bot_token").unwrap_or_default(),
        "getChatMemberCount",
        TgBody::Json(json!({ "chat_id": ctx.secret.get("chat_id") })),
    )?;
    Ok(json!({ "members": n }))
}

// ---------- Discord (channel webhook) ----------

pub fn discord_verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let url = f.get("webhook_url").map(js_string).unwrap_or_default();
    let w = net::get(&url).get_json()?;
    Ok(vec![Connected {
        external_id: w.get("id").map(js_string).unwrap_or_default(),
        name: opt_string(w.get("name")).unwrap_or_default(),
        handle: Some(format!("#{}", w.get("channel_id").map(js_string).unwrap_or_else(|| "undefined".into()))),
        secret: json!({ "webhook_url": f.get("webhook_url") }),
        meta: Some(Obj::pairs(&[("guildId", w.get("guild_id")), ("channelId", w.get("channel_id"))])),
        expires_at: None,
    }])
}

struct Obj;
impl Obj {
    fn pairs(p: &[(&str, Option<&Value>)]) -> Value {
        let mut m = Map::new();
        for (k, v) in p {
            put(&mut m, k, v.cloned());
        }
        Value::Object(m)
    }
}

pub fn discord_publish(ctx: &Ctx) -> Result<Published> {
    let attachments: Vec<Value> = ctx
        .media
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut o = Map::new();
            o.insert("id".into(), i.into());
            o.insert("filename".into(), m.filename.clone().into());
            put(&mut o, "description", m.alt.clone().map(Value::from));
            Value::Object(o)
        })
        .collect();
    let mut payload = Map::new();
    payload.insert("content".into(), ctx.text.clone().into());
    put(&mut payload, "username", ctx.opt("username").cloned());
    put(&mut payload, "thread_name", ctx.opt("thread_name").cloned());
    payload.insert("allowed_mentions".into(), json!({ "parse": [] }));
    payload.insert("attachments".into(), Value::Array(attachments));
    let mut parts = vec![Part::text("payload_json", Value::Object(payload).to_string())];
    for (i, m) in ctx.media.iter().enumerate() {
        parts.push(Part::file(&format!("files[{i}]"), &m.filename, Some(&m.mime), PartData::Local(local_file(m)?)));
    }
    let url = str_of(&ctx.secret, "webhook_url").unwrap_or_default();
    let msg = net::post(format!("{url}?wait=true")).multipart(parts).timeout_secs(600).get_json()?;
    let id = msg.get("id").map(js_string).unwrap_or_else(|| "undefined".into());
    let guild = ctx.meta.get("guildId").filter(|g| truthy(Some(g)));
    let channel = some(msg.get("channel_id"))
        .or(ctx.meta.get("channelId"))
        .map(js_string)
        .unwrap_or_else(|| "undefined".into());
    Ok(Published {
        url: guild.map(|g| format!("https://discord.com/channels/{}/{channel}/{id}", js_string(g))),
        id,
    })
}

pub fn discord_remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{}/messages/{id}", str_of(&ctx.secret, "webhook_url").unwrap_or_default())).send()?;
    Ok(())
}

// ---------- Slack (bot token) ----------

fn slack(token: &str, method: &str, body: Value, form: bool) -> Result<Value> {
    let rq = net::post(format!("https://slack.com/api/{method}")).bearer(token);
    let r = if form {
        let pairs: Vec<(String, Option<String>)> = body
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), some(Some(v)).map(js_string)))
            .collect();
        let refs: Vec<(&str, Option<String>)> = pairs.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        rq.form(&refs)
    } else {
        rq.json(body)
    }
    .get_json()?;
    if !truthy(r.get("ok")) {
        permanent!("Slack {method}: {}", r.get("error").map(js_string).unwrap_or_else(|| "undefined".into()));
    }
    Ok(r)
}

pub fn slack_verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let token = f.get("bot_token").map(js_string).unwrap_or_default();
    let auth = slack(&token, "auth.test", json!({}), false)?;
    let ch = slack(&token, "conversations.info", json!({ "channel": f.get("channel") }), true)?;
    let channel = f.get("channel").map(js_string).unwrap_or_default();
    let cname = ch.pointer("/channel/name").map(js_string).unwrap_or_else(|| "undefined".into());
    Ok(vec![Connected {
        external_id: format!("{}:{channel}", auth.get("team_id").map(js_string).unwrap_or_else(|| "undefined".into())),
        name: format!("{} #{cname}", auth.get("team").map(js_string).unwrap_or_else(|| "undefined".into())),
        handle: Some(format!("#{cname}")),
        secret: json!({ "bot_token": f.get("bot_token"), "channel": f.get("channel") }),
        meta: None,
        expires_at: None,
    }])
}

pub fn slack_publish(ctx: &Ctx) -> Result<Published> {
    let t = str_of(&ctx.secret, "bot_token").unwrap_or_default().to_string();
    let channel = ctx.secret.get("channel").cloned().unwrap_or(Value::Null);
    if ctx.media.is_empty() {
        let r = slack(
            &t,
            "chat.postMessage",
            json!({ "channel": channel, "text": ctx.text, "unfurl_links": true }),
            false,
        )?;
        let link = slack(&t, "chat.getPermalink", json!({ "channel": channel, "message_ts": r.get("ts") }), true)?;
        return Ok(Published {
            id: r.get("ts").map(js_string).unwrap_or_default(),
            url: opt_string(link.get("permalink")),
        });
    }
    let mut files = vec![];
    for m in &ctx.media {
        let f = local_file(m)?;
        let mut req = Map::new();
        req.insert("filename".into(), m.filename.clone().into());
        req.insert("length".into(), f.len.into());
        put(&mut req, "alt_text", m.alt.clone().map(Value::from));
        let up = slack(&t, "files.getUploadURLExternal", Value::Object(req), true)?;
        net::post(up.get("upload_url").map(js_string).unwrap_or_default())
            .body(Body::File {
                path: f.path.clone(),
                offset: 0,
                len: f.len,
            })
            .timeout_secs(600)
            .send()?;
        files.push(json!({ "id": up.get("file_id"), "title": m.filename }));
    }
    let done = slack(
        &t,
        "files.completeUploadExternal",
        json!({ "files": files, "channel_id": channel, "initial_comment": ctx.text }),
        false,
    )?;
    Ok(Published {
        id: done.pointer("/files/0/id").map(js_string).unwrap_or_default(),
        url: opt_string(done.pointer("/files/0/permalink")),
    })
}
