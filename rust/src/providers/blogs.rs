//! DEV, Hashnode, WordPress, Medium and Lemmy.

use serde_json::{json, Map, Value};

use super::{opt_string, or_zero, put, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::{assert_public_url, kind_of, local_file};
use crate::net::{self, Body, Part, PartData};
use crate::util::{bail, basic_auth, js_string, permanent, some, str_of, truthy, Result};

fn title(ctx: &Ctx) -> Result<Value> {
    match ctx.opt("title").filter(|t| truthy(Some(t))) {
        Some(t) => Ok(t.clone()),
        None => permanent!("options.title is required for articles"),
    }
}

fn cover(ctx: &Ctx, platform: &str) -> Result<Option<String>> {
    match ctx.media.iter().find(|m| kind_of(&m.mime) == "image") {
        Some(m) => {
            assert_public_url(m, platform)?;
            Ok(Some(m.url.clone()))
        }
        None => Ok(None),
    }
}

fn host(url: &str) -> String {
    crate::util::host_path(url).0.to_ascii_lowercase()
}

fn trim_slash(s: &str) -> String {
    s.strip_suffix('/').unwrap_or(s).to_string()
}

fn f(fields: &Map<String, Value>, k: &str) -> String {
    fields.get(k).map(js_string).unwrap_or_default()
}

fn first_n(v: Option<&Value>, n: usize) -> Option<Value> {
    v.and_then(|t| t.as_array()).map(|a| Value::Array(a.iter().take(n).cloned().collect()))
}

// ---------- DEV ----------

pub fn devto_verify(fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    let base = trim_slash(&Some(f(fields, "base_url")).filter(|s| !s.is_empty()).unwrap_or_else(|| "https://dev.to".into()));
    let me = net::get(format!("{base}/api/users/me")).header("api-key", f(fields, "api_key")).get_json()?;
    Ok(vec![Connected {
        external_id: me.get("id").map(js_string).unwrap_or_default(),
        name: opt_string(me.get("name")).unwrap_or_default(),
        handle: opt_string(me.get("username")),
        secret: json!({ "api_key": fields.get("api_key"), "base_url": base }),
        meta: None,
        expires_at: None,
    }])
}

pub fn devto_publish(ctx: &Ctx) -> Result<Published> {
    let mut article = Map::new();
    article.insert("title".into(), title(ctx)?);
    article.insert("body_markdown".into(), ctx.text.clone().into());
    article.insert("published".into(), (!truthy(ctx.opt("draft"))).into());
    put(&mut article, "tags", first_n(ctx.opt("tags"), 4));
    put(&mut article, "main_image", cover(ctx, "DEV")?.map(Value::from));
    put(&mut article, "canonical_url", ctx.opt("canonical_url").cloned());
    put(&mut article, "series", ctx.opt("series").cloned());
    let a = net::post(format!("{}/api/articles", str_of(&ctx.secret, "base_url").unwrap_or_default()))
        .header("api-key", str_of(&ctx.secret, "api_key").unwrap_or_default())
        .json(json!({ "article": article }))
        .get_json()?;
    Ok(Published {
        id: a.get("id").map(js_string).unwrap_or_else(|| "undefined".into()),
        url: opt_string(a.get("url")),
    })
}

pub fn devto_metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let a = net::get(format!("{}/api/articles/{id}", str_of(&ctx.secret, "base_url").unwrap_or_default()))
        .header("api-key", str_of(&ctx.secret, "api_key").unwrap_or_default())
        .get_json()?;
    Ok(
        json!({ "reactions": or_zero(a.get("public_reactions_count")), "comments": or_zero(a.get("comments_count")), "views": or_zero(a.get("page_views_count")) }),
    )
}

// ---------- Hashnode ----------

fn hn(token: &str, query: &str, variables: Value) -> Result<Value> {
    let r = net::post("https://gql.hashnode.com")
        .header("authorization", token)
        .json(json!({ "query": query, "variables": variables }))
        .get_json()?;
    if let Some(errs) = r.get("errors").and_then(|e| e.as_array()).filter(|a| !a.is_empty()) {
        let msgs: Vec<String> = errs
            .iter()
            .map(|e| e.get("message").map(js_string).unwrap_or_else(|| "undefined".into()))
            .collect();
        permanent!("Hashnode: {}", msgs.join("; "));
    }
    Ok(r.get("data").cloned().unwrap_or(Value::Null))
}

pub fn hashnode_verify(fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    let d = hn(
        &f(fields, "token"),
        "query($id: ObjectId!) { me { id username name } publication(id: $id) { id title url } }",
        json!({ "id": fields.get("publication_id") }),
    )?;
    if !truthy(d.get("publication")) {
        bail!("Publication not found");
    }
    Ok(vec![Connected {
        external_id: d.pointer("/publication/id").map(js_string).unwrap_or_default(),
        name: opt_string(d.pointer("/publication/title")).unwrap_or_default(),
        handle: opt_string(d.pointer("/me/username")),
        secret: json!({ "token": fields.get("token"), "publication_id": fields.get("publication_id") }),
        meta: Some(json!({ "url": d.pointer("/publication/url") })),
        expires_at: None,
    }])
}

fn slug(t: &str) -> String {
    let lower = t.to_lowercase();
    let mut out = String::new();
    let mut dash = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out
}

pub fn hashnode_publish(ctx: &Ctx) -> Result<Published> {
    let tags: Vec<Value> = ctx
        .opt("tags")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .map(|t| json!({ "slug": slug(&js_string(t)), "name": t }))
        .collect();
    let mut input = Map::new();
    input.insert("publicationId".into(), ctx.secret.get("publication_id").cloned().unwrap_or_default());
    input.insert("title".into(), title(ctx)?);
    put(&mut input, "subtitle", ctx.opt("subtitle").cloned());
    input.insert("contentMarkdown".into(), ctx.text.clone().into());
    input.insert("tags".into(), Value::Array(tags));
    put(&mut input, "coverImageOptions", cover(ctx, "Hashnode")?.map(|u| json!({ "coverImageURL": u })));
    put(&mut input, "originalArticleURL", ctx.opt("canonical_url").cloned());
    let d = hn(
        str_of(&ctx.secret, "token").unwrap_or_default(),
        "mutation($input: PublishPostInput!) { publishPost(input: $input) { post { id url } } }",
        json!({ "input": input }),
    )?;
    Ok(Published {
        id: d.pointer("/publishPost/post/id").map(js_string).unwrap_or_else(|| "undefined".into()),
        url: opt_string(d.pointer("/publishPost/post/url")),
    })
}

pub fn hashnode_metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let d = hn(
        str_of(&ctx.secret, "token").unwrap_or_default(),
        "query($id: ID!) { post(id: $id) { views reactionCount responseCount } }",
        json!({ "id": id }),
    )?;
    Ok(
        json!({ "views": or_zero(d.pointer("/post/views")), "reactions": or_zero(d.pointer("/post/reactionCount")), "comments": or_zero(d.pointer("/post/responseCount")) }),
    )
}

// ---------- WordPress ----------

pub fn wordpress_verify(fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    let site = trim_slash(&f(fields, "site"));
    let me = net::get(format!("{site}/wp-json/wp/v2/users/me"))
        .basic(&f(fields, "username"), &f(fields, "app_password"))
        .get_json()?;
    let h = host(&site);
    Ok(vec![Connected {
        external_id: format!("{h}:{}", me.get("id").map(js_string).unwrap_or_default()),
        name: format!("{h} ({})", me.get("name").map(js_string).unwrap_or_else(|| "undefined".into())),
        handle: opt_string(me.get("slug")),
        secret: json!({ "site": site, "username": fields.get("username"), "app_password": fields.get("app_password") }),
        meta: None,
        expires_at: None,
    }])
}

fn wp_auth(ctx: &Ctx) -> String {
    basic_auth(
        str_of(&ctx.secret, "username").unwrap_or_default(),
        str_of(&ctx.secret, "app_password").unwrap_or_default(),
    )
}

pub fn wordpress_publish(ctx: &Ctx) -> Result<Published> {
    let site = str_of(&ctx.secret, "site").unwrap_or_default().to_string();
    let mut featured: Option<Value> = None;
    if let Some(img) = ctx.media.first() {
        let file = local_file(img)?;
        let up = net::post(format!("{site}/wp-json/wp/v2/media"))
            .header("authorization", wp_auth(ctx))
            .header("content-type", img.mime.clone())
            .header("content-disposition", format!("attachment; filename=\"{}\"", img.filename))
            .body(Body::File {
                path: file.path.clone(),
                offset: 0,
                len: file.len,
            })
            .get_json()?;
        featured = up.get("id").cloned();
    }
    let mut body = Map::new();
    body.insert("title".into(), title(ctx)?);
    body.insert("content".into(), ctx.text.clone().into());
    put(&mut body, "excerpt", ctx.opt("excerpt").cloned());
    body.insert(
        "status".into(),
        ctx.opt("status")
            .cloned()
            .unwrap_or_else(|| Value::from(if truthy(ctx.opt("draft")) { "draft" } else { "publish" })),
    );
    put(&mut body, "categories", ctx.opt("categories").cloned());
    put(&mut body, "featured_media", featured);
    let p = net::post(format!("{site}/wp-json/wp/v2/posts"))
        .header("authorization", wp_auth(ctx))
        .json(Value::Object(body))
        .get_json()?;
    Ok(Published {
        id: p.get("id").map(js_string).unwrap_or_else(|| "undefined".into()),
        url: opt_string(p.get("link")),
    })
}

pub fn wordpress_remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req(
        "DELETE",
        format!("{}/wp-json/wp/v2/posts/{id}", str_of(&ctx.secret, "site").unwrap_or_default()),
    )
    .header("authorization", wp_auth(ctx))
    .get_json()?;
    Ok(())
}

// ---------- Medium ----------

pub fn medium_verify(fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    let me = net::get("https://api.medium.com/v1/me").bearer(&f(fields, "token")).get_json()?["data"].clone();
    Ok(vec![Connected {
        external_id: me.get("id").map(js_string).unwrap_or_default(),
        name: opt_string(me.get("name")).unwrap_or_default(),
        handle: opt_string(me.get("username")),
        secret: json!({ "token": fields.get("token") }),
        meta: None,
        expires_at: None,
    }])
}

pub fn medium_publish(ctx: &Ctx) -> Result<Published> {
    let path = match ctx.opt("publication_id").filter(|p| truthy(Some(p))) {
        Some(p) => format!("publications/{}", js_string(p)),
        None => format!("users/{}", ctx.external_id),
    };
    let t = title(ctx)?;
    let mut body = Map::new();
    body.insert("title".into(), t.clone());
    body.insert("contentFormat".into(), "markdown".into());
    body.insert("content".into(), format!("# {}\n\n{}", js_string(&t), ctx.text).into());
    put(&mut body, "tags", first_n(ctx.opt("tags"), 5));
    put(&mut body, "canonicalUrl", ctx.opt("canonical_url").cloned());
    body.insert("publishStatus".into(), (if truthy(ctx.opt("draft")) { "draft" } else { "public" }).into());
    let r = net::post(format!("https://api.medium.com/v1/{path}/posts"))
        .bearer(str_of(&ctx.secret, "token").unwrap_or_default())
        .json(Value::Object(body))
        .get_json()?;
    let p = r.get("data").cloned().unwrap_or_default();
    Ok(Published {
        id: p.get("id").map(js_string).unwrap_or_else(|| "undefined".into()),
        url: opt_string(p.get("url")),
    })
}

// ---------- Lemmy ----------

fn lemmy_base(s: &Value) -> String {
    let raw = s.get("instance").map(js_string).unwrap_or_else(|| "undefined".into());
    let b = trim_slash(&raw);
    if b.starts_with("http://") || b.starts_with("https://") {
        b
    } else {
        format!("https://{b}")
    }
}

fn lemmy_login(s: &Value) -> Result<String> {
    let mut body = Map::new();
    put(&mut body, "username_or_email", s.get("username").cloned());
    put(&mut body, "password", s.get("password").cloned());
    put(&mut body, "totp_2fa_token", s.get("totp").cloned());
    let r = net::post(format!("{}/api/v3/user/login", lemmy_base(s))).json(Value::Object(body)).get_json()?;
    Ok(r.get("jwt").map(js_string).unwrap_or_default())
}

pub fn lemmy_verify(fields: &Map<String, Value>) -> Result<Vec<Connected>> {
    let fv = Value::Object(fields.clone());
    let jwt = lemmy_login(&fv)?;
    let base = lemmy_base(&fv);
    let site = net::get(format!("{base}/api/v3/site")).bearer(&jwt).get_json()?;
    let p = site.pointer("/my_user/local_user_view/person").cloned().unwrap_or_default();
    let name = p.get("name").map(js_string).unwrap_or_default();
    Ok(vec![Connected {
        external_id: p.get("actor_id").map(js_string).unwrap_or_default(),
        name: some(p.get("display_name")).map(js_string).unwrap_or_else(|| name.clone()),
        handle: Some(format!("{name}@{}", host(&base))),
        secret: json!({ "instance": base, "username": fields.get("username"), "password": fields.get("password") }),
        meta: None,
        expires_at: None,
    }])
}

pub fn lemmy_publish(ctx: &Ctx) -> Result<Published> {
    let base = lemmy_base(&ctx.secret);
    let jwt = lemmy_login(&ctx.secret)?;
    if !truthy(ctx.opt("community")) {
        permanent!("options.community is required for Lemmy");
    }
    let c = net::get(format!("{base}/api/v3/community"))
        .bearer(&jwt)
        .query(&[("name", ctx.opt_str("community"))])
        .get_json()?;
    let mut url = ctx.opt("url").filter(|u| truthy(Some(u))).cloned();
    if let (None, Some(img)) = (&url, ctx.media.first()) {
        let parts = vec![Part::file("images[]", &img.filename, Some(&img.mime), PartData::Local(local_file(img)?))];
        let up = net::post(format!("{base}/pictrs/image"))
            .bearer(&jwt)
            .header("cookie", format!("jwt={jwt}"))
            .multipart(parts)
            .get_json()?;
        url = Some(Value::from(format!(
            "{base}/pictrs/image/{}",
            up.pointer("/files/0/file").map(js_string).unwrap_or_else(|| "undefined".into())
        )));
    }
    let mut body = Map::new();
    body.insert("name".into(), title(ctx)?);
    body.insert("community_id".into(), c.pointer("/community_view/community/id").cloned().unwrap_or(Value::Null));
    put(&mut body, "body", if ctx.text.is_empty() { None } else { Some(ctx.text.clone().into()) });
    put(&mut body, "url", url);
    put(&mut body, "nsfw", ctx.opt("nsfw").cloned());
    let r = net::post(format!("{base}/api/v3/post")).bearer(&jwt).json(Value::Object(body)).get_json()?;
    let id = r.pointer("/post_view/post/id").map(js_string).unwrap_or_else(|| "undefined".into());
    Ok(Published {
        url: Some(format!("{base}/post/{id}")),
        id,
    })
}

pub fn lemmy_metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let r = net::get(format!("{}/api/v3/post", lemmy_base(&ctx.secret)))
        .query(&[("id", Some(id.into()))])
        .get_json()?;
    let c = r.pointer("/post_view/counts").cloned().unwrap_or_default();
    let mut o = Map::new();
    for k in ["score", "upvotes", "downvotes", "comments"] {
        put(&mut o, k, c.get(k).cloned());
    }
    Ok(Value::Object(o))
}
