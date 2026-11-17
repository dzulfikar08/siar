//! Bluesky (src/providers/bluesky.ts).

use serde_json::{json, Map, Value};

use super::{or_zero, put, Published};
use crate::accounts::{Connected, Ctx};
use crate::db::MediaRow;
use crate::media::{kind_of, local_file};
use crate::net::{self, Body};
use crate::util::{hashtags, iso, js_is_space, js_string, now, permanent, str_of, Result};

fn svc(s: &Value) -> String {
    let v = str_of(s, "service").filter(|v| !v.is_empty()).unwrap_or("https://bsky.social");
    v.strip_suffix('/').unwrap_or(v).to_string()
}

struct Session {
    jwt: String,
    did: String,
    handle: String,
}

fn session(s: &Value) -> Result<Session> {
    let r = net::post(format!("{}/xrpc/com.atproto.server.createSession", svc(s)))
        .json(json!({ "identifier": s.get("identifier"), "password": s.get("password") }))
        .get_json()?;
    Ok(Session {
        jwt: r.get("accessJwt").map(js_string).unwrap_or_default(),
        did: r.get("did").map(js_string).unwrap_or_default(),
        handle: r.get("handle").map(js_string).unwrap_or_default(),
    })
}

/// Link spans like `/https?:\/\/[^\s<>"')\]]+[^\s<>"')\].,;:!?]/g`, as byte ranges.
fn links(text: &str) -> Vec<(usize, usize)> {
    let stop = |c: char| js_is_space(c) || "<>\"')]".contains(c);
    let punct = |c: char| ".,;:!?".contains(c);
    let mut out = vec![];
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            i += rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            continue;
        };
        let body: Vec<(usize, char)> = rest[scheme..].char_indices().take_while(|(_, c)| !stop(*c)).collect();
        // Greedy run, then backtrack until the last character is not punctuation; at least 2 chars.
        let mut end = body.len();
        while end >= 2 && punct(body[end - 1].1) {
            end -= 1;
        }
        if end >= 2 {
            let (off, c) = body[end - 1];
            let stop_at = i + scheme + off + c.len_utf8();
            out.push((i, stop_at));
            i = stop_at;
        } else {
            i += rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }
    out
}

/// `(?<=^|\s)@([a-z0-9.-]+\.[a-z]{2,})` (case-insensitive): (byte start of '@', byte end, handle).
fn mentions(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = vec![];
    let mut prev: Option<char> = None;
    let mut skip_to = 0;
    for (i, c) in text.char_indices() {
        let at_start = prev.map(js_is_space).unwrap_or(true);
        prev = Some(c);
        if i < skip_to || c != '@' || !at_start {
            continue;
        }
        let run: Vec<char> = text[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        // Rightmost dot with a non-empty part before and 2+ letters after.
        let mut found = None;
        for d in (1..run.len()).rev() {
            if run[d] != '.' {
                continue;
            }
            let letters = run[d + 1..].iter().take_while(|c| c.is_ascii_alphabetic()).count();
            if letters >= 2 {
                found = Some(d + 1 + letters);
                break;
            }
        }
        if let Some(len) = found {
            let handle: String = run[..len].iter().collect();
            let end = i + 1 + handle.len();
            out.push((i, end, handle));
            skip_to = end;
        }
    }
    out
}

fn facets(text: &str, service: &str) -> Vec<Value> {
    let mut out = vec![];
    let add = |out: &mut Vec<Value>, start: usize, end: usize, feature: Value| {
        out.push(json!({ "index": { "byteStart": start, "byteEnd": end }, "features": [feature] }));
    };
    for (s, e) in links(text) {
        add(&mut out, s, e, json!({ "$type": "app.bsky.richtext.facet#link", "uri": &text[s..e] }));
    }
    for (s, tag) in hashtags(text) {
        let e = s + 1 + tag.len();
        add(&mut out, s, e, json!({ "$type": "app.bsky.richtext.facet#tag", "tag": tag }));
    }
    for (s, e, handle) in mentions(text) {
        let r = net::get(format!("{service}/xrpc/com.atproto.identity.resolveHandle"))
            .query(&[("handle", Some(handle))])
            .get_json();
        if let Ok(r) = r {
            add(&mut out, s, e, json!({ "$type": "app.bsky.richtext.facet#mention", "did": r.get("did") }));
        }
    }
    out
}

fn upload(service: &str, jwt: &str, m: &MediaRow) -> Result<Value> {
    let f = local_file(m)?;
    let r = net::post(format!("{service}/xrpc/com.atproto.repo.uploadBlob"))
        .bearer(jwt)
        .header("content-type", m.mime.clone())
        .body(Body::File {
            path: f.path.clone(),
            offset: 0,
            len: f.len,
        })
        .timeout_secs(600)
        .get_json()?;
    Ok(r.get("blob").cloned().unwrap_or(Value::Null))
}

fn embed(service: &str, jwt: &str, media: &[MediaRow]) -> Result<Option<Value>> {
    if media.is_empty() {
        return Ok(None);
    }
    if let Some(v) = media.iter().find(|m| kind_of(&m.mime) == "video") {
        let mut o = Map::new();
        o.insert("$type".into(), "app.bsky.embed.video".into());
        o.insert("video".into(), upload(service, jwt, v)?);
        put(&mut o, "alt", v.alt.clone().map(Value::from));
        return Ok(Some(Value::Object(o)));
    }
    let mut images = vec![];
    for m in media {
        images.push(json!({ "alt": m.alt.clone().unwrap_or_default(), "image": upload(service, jwt, m)? }));
    }
    Ok(Some(json!({ "$type": "app.bsky.embed.images", "images": images })))
}

fn rkey(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

pub fn verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let fv = Value::Object(f.clone());
    let s = session(&fv)?;
    Ok(vec![Connected {
        external_id: s.did,
        name: s.handle.clone(),
        handle: Some(s.handle),
        secret: json!({ "identifier": f.get("identifier"), "password": f.get("password"), "service": svc(&fv) }),
        meta: None,
        expires_at: None,
    }])
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let service = svc(&ctx.secret);
    let s = session(&ctx.secret)?;
    let mut parts: Vec<Value> = vec![Value::from(ctx.text.clone())];
    parts.extend(ctx.opt("thread").and_then(|t| t.as_array()).cloned().unwrap_or_default());
    let mut root: Option<Value> = None;
    let mut parent: Option<Value> = None;
    for (i, text) in parts.iter().enumerate() {
        let t = js_string(text);
        let mut record = Map::new();
        record.insert("$type".into(), "app.bsky.feed.post".into());
        record.insert("text".into(), text.clone());
        record.insert("createdAt".into(), iso(now()).into());
        record.insert("facets".into(), Value::Array(facets(&t, &service)));
        put(&mut record, "langs", ctx.opt("langs").cloned());
        if i == 0 {
            put(&mut record, "embed", embed(&service, &s.jwt, &ctx.media)?);
        }
        if let (Some(r), Some(p)) = (&root, &parent) {
            record.insert("reply".into(), json!({ "root": r, "parent": p }));
        }
        let r = net::post(format!("{service}/xrpc/com.atproto.repo.createRecord"))
            .bearer(&s.jwt)
            .json(json!({ "repo": s.did, "collection": "app.bsky.feed.post", "record": record }))
            .get_json()?;
        if root.is_none() {
            root = Some(r.clone());
        }
        parent = Some(r);
    }
    let Some(root) = root else { permanent!("Nothing to post") };
    let uri = root.get("uri").map(js_string).unwrap_or_default();
    Ok(Published {
        url: Some(format!("https://bsky.app/profile/{}/post/{}", s.handle, rkey(&uri))),
        id: uri,
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    let s = session(&ctx.secret)?;
    net::post(format!("{}/xrpc/com.atproto.repo.deleteRecord", svc(&ctx.secret)))
        .bearer(&s.jwt)
        .json(json!({ "repo": s.did, "collection": "app.bsky.feed.post", "rkey": rkey(id) }))
        .get_json()?;
    Ok(())
}

pub fn metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let s = session(&ctx.secret)?;
    let r = net::get(format!("{}/xrpc/app.bsky.feed.getPosts", svc(&ctx.secret)))
        .bearer(&s.jwt)
        .query(&[("uris", Some(id.into()))])
        .get_json()?;
    let p = r.pointer("/posts/0").cloned().unwrap_or_default();
    Ok(json!({
        "likes": or_zero(p.get("likeCount")),
        "reposts": or_zero(p.get("repostCount")),
        "replies": or_zero(p.get("replyCount")),
        "quotes": or_zero(p.get("quoteCount")),
    }))
}

pub fn account_metrics(ctx: &Ctx) -> Result<Value> {
    let s = session(&ctx.secret)?;
    let p = net::get(format!("{}/xrpc/app.bsky.actor.getProfile", svc(&ctx.secret)))
        .bearer(&s.jwt)
        .query(&[("actor", Some(s.did.clone()))])
        .get_json()?;
    Ok(json!({ "followers": or_zero(p.get("followersCount")), "following": or_zero(p.get("followsCount")), "posts": or_zero(p.get("postsCount")) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_spans() {
        let t = "See https://example.com/a?b=1. and @alice.bsky.social! #rust ok";
        let l = links(t);
        assert_eq!(&t[l[0].0..l[0].1], "https://example.com/a?b=1");
        let m = mentions(t);
        assert_eq!(m[0].2, "alice.bsky.social");
        assert_eq!(&t[m[0].0..m[0].1], "@alice.bsky.social");
    }
}
