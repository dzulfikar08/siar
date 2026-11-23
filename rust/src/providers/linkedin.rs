//! LinkedIn personal profile and Company Page. Uploads stream from
//! disk: images/PDFs as one PUT, videos part by part from file offsets.

use serde_json::{json, Map, Value};

use super::{expires_at, opt_string, or_zero, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::config::env;
use crate::db::MediaRow;
use crate::media::{kind_of, local_file};
use crate::net::{self, Body};
use crate::util::{js_string, permanent, some, str_of, truthy, uri_component, Error, Result};

const REST: &str = "https://api.linkedin.com/rest";

fn version() -> String {
    env("SIAR_LINKEDIN_VERSION").unwrap_or_else(|| "202610".into())
}

fn h(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {token}")),
        ("LinkedIn-Version", version()),
        ("X-Restli-Protocol-Version", "2.0.0".into()),
    ]
}

/// LinkedIn "little text": these characters must be backslash-escaped or the post is cut off.
fn escape_raw(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\|{}@[]()<>#*_~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escape text for LinkedIn's "little" format; #tags become clickable hashtag templates.
/// Mirrors `/(^|\s)#([\p{L}\p{N}_]+)/gu`, skipping number-only tags (LinkedIn drops their "#").
pub fn escape_little(s: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        let at_start = i == 0 || crate::util::js_is_space(chars[i - 1].1);
        if c == '#' && at_start && pos >= last {
            let mut j = i + 1;
            while j < chars.len() && crate::util::is_tag_char(chars[j].1) {
                j += 1;
            }
            if j > i + 1 && chars[i + 1..j].iter().any(|&(_, c)| crate::util::is_letter(c)) {
                let end = if j < chars.len() { chars[j].0 } else { s.len() };
                out.push_str(&escape_raw(&s[last..pos]));
                out.push_str(&format!("{{hashtag|\\#|{}}}", escape_raw(&s[pos + 1..end])));
                last = end;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&escape_raw(&s[last..]));
    out
}

fn init(token: &str, kind: &str, owner: &str, extra: Map<String, Value>) -> Result<Value> {
    let mut req = Map::new();
    req.insert("owner".into(), owner.into());
    req.extend(extra);
    let r = net::post(format!("{REST}/{kind}?action=initializeUpload"))
        .headers(&h(token))
        .json(json!({ "initializeUploadRequest": req }))
        .get_json()?;
    Ok(r.get("value").cloned().unwrap_or_default())
}

fn upload_one(token: &str, owner: &str, m: &MediaRow) -> Result<String> {
    let file = local_file(m)?;
    let kind = kind_of(&m.mime);
    if kind == "image" || kind == "pdf" {
        let v = init(token, if kind == "image" { "images" } else { "documents" }, owner, Map::new())?;
        net::req("PUT", str_of(&v, "uploadUrl").unwrap_or_default())
            .header("authorization", format!("Bearer {token}"))
            .body(Body::File {
                path: file.path.clone(),
                offset: 0,
                len: file.len,
            })
            .timeout_secs(600)
            .send()?;
        return Ok(some(v.get("image")).or(v.get("document")).map(js_string).unwrap_or_default());
    }
    let mut extra = Map::new();
    extra.insert("fileSizeBytes".into(), file.len.into());
    extra.insert("uploadCaptions".into(), false.into());
    extra.insert("uploadThumbnail".into(), false.into());
    let v = init(token, "videos", owner, extra)?;
    let mut etags = vec![];
    for part in v["uploadInstructions"].as_array().into_iter().flatten() {
        let first = part["firstByte"].as_u64().unwrap_or(0);
        let last = part["lastByte"].as_u64().unwrap_or(0);
        let len = (last + 1).saturating_sub(first).min(file.len.saturating_sub(first));
        let res = net::req("PUT", str_of(part, "uploadUrl").unwrap_or_default())
            .body(Body::File {
                path: file.path.clone(),
                offset: first,
                len,
            })
            .timeout_secs(600)
            .send()?;
        etags.push(res.header("etag").map(Value::from).unwrap_or(Value::Null));
    }
    net::post(format!("{REST}/videos?action=finalizeUpload"))
        .headers(&h(token))
        .json(json!({ "finalizeUploadRequest": { "video": v.get("video"), "uploadToken": some(v.get("uploadToken")).cloned().unwrap_or(Value::from("")), "uploadedPartIds": etags } }))
        .send()?;
    Ok(v.get("video").map(js_string).unwrap_or_default())
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let token = ctx.token();
    let author = ctx.meta_str("urn");
    let mut kinds: Vec<&str> = ctx.media.iter().map(|m| kind_of(&m.mime)).collect();
    kinds.dedup();
    kinds.sort();
    kinds.dedup();
    if kinds.len() > 1 {
        permanent!("LinkedIn posts take images, one video, or one PDF, not a mix");
    }
    let mut ids = vec![];
    for m in &ctx.media {
        ids.push(upload_one(&token, &author, m)?);
    }
    let first = ctx.media.first();
    let alt = |m: &MediaRow| m.alt.clone().map(Value::from);
    let content: Option<Value> = match first.map(|m| kind_of(&m.mime)) {
        Some("pdf") => {
            let fname = &first.map(|m| m.filename.clone()).unwrap_or_default();
            let title = ctx.opt("document_title").cloned().unwrap_or_else(|| {
                let lower = fname.to_lowercase();
                Value::from(if lower.ends_with(".pdf") {
                    fname[..fname.len() - 4].to_string()
                } else {
                    fname.clone()
                })
            });
            Some(json!({ "media": { "id": ids[0], "title": title } }))
        }
        Some("video") => {
            let mut m = Map::new();
            m.insert("id".into(), ids[0].clone().into());
            put(&mut m, "title", ctx.opt("video_title").cloned());
            Some(json!({ "media": m }))
        }
        _ if ids.len() == 1 => {
            let mut m = Map::new();
            m.insert("id".into(), ids[0].clone().into());
            put(&mut m, "altText", first.and_then(alt));
            Some(json!({ "media": m }))
        }
        _ if ids.len() > 1 => {
            let images: Vec<Value> = ids
                .iter()
                .zip(ctx.media.iter())
                .map(|(id, m)| {
                    let mut o = Map::new();
                    o.insert("id".into(), id.clone().into());
                    put(&mut o, "altText", alt(m));
                    Value::Object(o)
                })
                .collect();
            Some(json!({ "multiImage": { "images": images } }))
        }
        _ if truthy(ctx.opt("link")) => {
            let link = ctx.opt("link").cloned().unwrap_or_default();
            Some(json!({ "article": { "source": link, "title": ctx.opt("link_title").cloned().unwrap_or(link.clone()) } }))
        }
        _ => None,
    };
    let mut body = Map::new();
    body.insert("author".into(), author.clone().into());
    body.insert("commentary".into(), escape_little(&ctx.text).into());
    body.insert("visibility".into(), ctx.opt("visibility").cloned().unwrap_or(Value::from("PUBLIC")));
    body.insert(
        "distribution".into(),
        json!({ "feedDistribution": "MAIN_FEED", "targetEntities": [], "thirdPartyDistributionChannels": [] }),
    );
    put(&mut body, "content", content);
    body.insert("lifecycleState".into(), "PUBLISHED".into());
    body.insert("isReshareDisabledByAuthor".into(), false.into());
    let res = net::post(format!("{REST}/posts")).headers(&h(&token)).json(Value::Object(body)).send()?;
    let urn = res
        .header("x-restli-id")
        .ok_or_else(|| Error::Msg("LinkedIn returned no post id (x-restli-id)".into()))?;
    if truthy(ctx.opt("first_comment")) {
        let _ = net::post(format!("{REST}/socialActions/{}/comments", uri_component(&urn)))
            .headers(&h(&token))
            .json(json!({ "actor": author, "object": urn, "message": { "text": ctx.opt("first_comment") } }))
            .send();
    }
    Ok(Published {
        url: Some(format!("https://www.linkedin.com/feed/update/{urn}")),
        id: urn,
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{REST}/posts/{}", uri_component(id)))
        .headers(&h(&ctx.token()))
        .send()?;
    Ok(())
}

pub fn social_metrics(ctx: &Ctx, urn: &str) -> Result<Value> {
    let r = net::get(format!("{REST}/socialActions/{}", uri_component(urn)))
        .headers(&h(&ctx.token()))
        .get_json()?;
    Ok(json!({
        "likes": or_zero(r.pointer("/likesSummary/totalLikes")),
        "comments": or_zero(r.pointer("/commentsSummary/aggregatedTotalComments")),
    }))
}

pub fn page_metrics(ctx: &Ctx, urn: &str) -> Result<Value> {
    let mut base = social_metrics(ctx, urn)?;
    let param = if urn.contains(":ugcPost:") { "ugcPosts[0]" } else { "shares[0]" };
    let s = net::get(format!("{REST}/organizationalEntityShareStatistics"))
        .headers(&h(&ctx.token()))
        .query(&[
            ("q", Some("organizationalEntity".into())),
            ("organizationalEntity", Some(ctx.meta_str("urn"))),
            (param, Some(urn.into())),
        ])
        .get_json()
        .ok();
    let t = s
        .as_ref()
        .and_then(|s| s.pointer("/elements/0/totalShareStatistics"))
        .cloned()
        .unwrap_or_default();
    if let Some(o) = base.as_object_mut() {
        o.insert("impressions".into(), or_zero(t.get("impressionCount")));
        o.insert("clicks".into(), or_zero(t.get("clickCount")));
        o.insert("shares".into(), or_zero(t.get("shareCount")));
        o.insert("engagement".into(), or_zero(t.get("engagement")));
    }
    Ok(base)
}

pub fn page_account_metrics(ctx: &Ctx) -> Result<Value> {
    let r = net::get(format!("{REST}/networkSizes/{}", uri_component(&ctx.meta_str("urn"))))
        .headers(&h(&ctx.token()))
        .query(&[("edgeType", Some("COMPANY_FOLLOWED_BY_MEMBER".into()))])
        .get_json()?;
    Ok(json!({ "followers": or_zero(r.get("firstDegreeSize")) }))
}

pub fn personal_spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://www.linkedin.com/oauth/v2/authorization".into(),
        token_url: "https://www.linkedin.com/oauth/v2/accessToken".into(),
        scopes: vec!["openid", "profile", "email", "w_member_social"],
        env: "LINKEDIN",
        pkce: false,
        scope_sep: " ",
        extra_auth_params: vec![],
        config_id_env: None,
        client_auth_basic: false,
        client_id_param: "client_id",
    }
}

/// Company pages use a second app with only the Community Management API.
pub fn page_spec() -> OAuthSpec {
    OAuthSpec {
        scopes: vec!["r_organization_social", "w_organization_social", "rw_organization_admin"],
        env: "LINKEDIN_PAGE",
        ..personal_spec()
    }
}

fn token_secret(t: &Value) -> Value {
    let mut s = Map::new();
    put(&mut s, "access_token", t.get("access_token").cloned());
    put(&mut s, "refresh_token", t.get("refresh_token").cloned());
    Value::Object(s)
}

pub fn profile(t: &Value) -> Result<Vec<Connected>> {
    let me = net::get("https://api.linkedin.com/v2/userinfo")
        .bearer(str_of(t, "access_token").unwrap_or_default())
        .get_json()?;
    let sub = me.get("sub").map(js_string).unwrap_or_default();
    Ok(vec![Connected {
        name: opt_string(me.get("name")).unwrap_or_default(),
        handle: None,
        secret: token_secret(t),
        expires_at: expires_at(t),
        meta: Some(json!({ "urn": format!("urn:li:person:{sub}") })),
        external_id: sub,
    }])
}

pub fn page_profile(t: &Value) -> Result<Vec<Connected>> {
    let token = str_of(t, "access_token").unwrap_or_default().to_string();
    let acls = net::get(format!("{REST}/organizationAcls"))
        .headers(&h(&token))
        .query(&[
            ("q", Some("roleAssignee".into())),
            ("role", Some("ADMINISTRATOR".into())),
            ("state", Some("APPROVED".into())),
        ])
        .get_json()?;
    let mut out = vec![];
    for el in acls["elements"].as_array().into_iter().flatten() {
        let org_urn = el.get("organization").map(js_string).unwrap_or_default();
        let id = org_urn.rsplit(':').next().unwrap_or_default().to_string();
        let org = net::get(format!("{REST}/organizations/{id}"))
            .headers(&h(&token))
            .get_json()
            .unwrap_or_default();
        out.push(Connected {
            name: opt_string(org.get("localizedName")).unwrap_or_else(|| format!("Organization {id}")),
            handle: opt_string(org.get("vanityName")),
            secret: token_secret(t),
            expires_at: expires_at(t),
            meta: Some(json!({ "urn": org_urn })),
            external_id: id,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_text_escaping() {
        assert_eq!(escape_little("Hi (team) #rust @me"), "Hi \\(team\\) {hashtag|\\#|rust} \\@me");
        assert_eq!(escape_little("C# and a#b #CMMS_2"), "C\\# and a\\#b {hashtag|\\#|CMMS\\_2}");
        assert_eq!(escape_little("Ticket #1042 #2026 #Q4"), "Ticket \\#1042 \\#2026 {hashtag|\\#|Q4}");
        assert_eq!(escape_little("#start and #ünïcode, #"), "{hashtag|\\#|start} and {hashtag|\\#|ünïcode}, \\#");
        assert_eq!(escape_little("##double"), "\\#\\#double");
    }
}
