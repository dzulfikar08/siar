//! Google Business Profile.

use serde_json::{json, Map, Value};

use super::{expires_at, opt_string, put, OAuthSpec, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::assert_public_url;
use crate::net;
use crate::util::{js_string, str_of, truthy, Result};

pub fn spec() -> OAuthSpec {
    OAuthSpec {
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token_url: "https://oauth2.googleapis.com/token".into(),
        scopes: vec!["openid", "email", "https://www.googleapis.com/auth/business.manage"],
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
    let token = str_of(t, "access_token").unwrap_or_default();
    let accounts = net::get("https://mybusinessaccountmanagement.googleapis.com/v1/accounts")
        .bearer(token)
        .get_json()?;
    let mut out = vec![];
    for acc in accounts["accounts"].as_array().into_iter().flatten() {
        let name = acc.get("name").map(js_string).unwrap_or_default();
        let locs = net::get(format!("https://mybusinessbusinessinformation.googleapis.com/v1/{name}/locations"))
            .bearer(token)
            .query(&[("readMask", Some("name,title".into())), ("pageSize", Some("100".into()))])
            .get_json()?;
        for l in locs["locations"].as_array().into_iter().flatten() {
            let mut secret = Map::new();
            put(&mut secret, "access_token", t.get("access_token").cloned());
            put(&mut secret, "refresh_token", t.get("refresh_token").cloned());
            out.push(Connected {
                external_id: l.get("name").map(js_string).unwrap_or_default(),
                name: opt_string(l.get("title")).unwrap_or_default(),
                handle: None,
                secret: Value::Object(secret),
                meta: Some(json!({ "account": acc.get("name"), "location": l.get("name") })),
                expires_at: expires_at(t),
            });
        }
    }
    Ok(out)
}

pub fn publish(ctx: &Ctx) -> Result<Published> {
    let m = ctx.media.first();
    if let Some(m) = m {
        assert_public_url(m, "Google Business Profile")?;
    }
    let mut body = Map::new();
    body.insert("languageCode".into(), ctx.opt("language").cloned().unwrap_or(Value::from("en")));
    body.insert("summary".into(), ctx.text.clone().into());
    body.insert("topicType".into(), "STANDARD".into());
    if let Some(m) = m {
        body.insert("media".into(), json!([{ "mediaFormat": "PHOTO", "sourceUrl": m.url }]));
    }
    if truthy(ctx.opt("cta")) {
        let mut cta = Map::new();
        cta.insert("actionType".into(), ctx.opt("cta").cloned().unwrap_or_default());
        put(&mut cta, "url", ctx.opt("cta_url").cloned());
        body.insert("callToAction".into(), Value::Object(cta));
    }
    let r = net::post(format!(
        "https://mybusiness.googleapis.com/v4/{}/{}/localPosts",
        ctx.meta_str("account"),
        ctx.meta_str("location")
    ))
    .bearer(&ctx.token())
    .json(Value::Object(body))
    .get_json()?;
    Ok(Published {
        id: r.get("name").map(js_string).unwrap_or_else(|| "undefined".into()),
        url: opt_string(r.get("searchUrl")),
    })
}

pub fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("https://mybusiness.googleapis.com/v4/{id}"))
        .bearer(&ctx.token())
        .send()?;
    Ok(())
}
