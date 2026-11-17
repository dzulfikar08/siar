//! Hosted mode HTTP: public pages, sign-in (GitHub, Google), the dashboard, MCP by API key,
//! per-tenant OAuth callbacks, media and tracked links, Meta webhooks and payment webhooks.
//! Server-rendered HTML, no JavaScript framework.

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};
use std::io::Read;
use tiny_http::{Method, Request, Response};

use crate::billing;
use crate::config::{config, env};
use crate::control;
use crate::limits;
use crate::platforms;
use crate::server::{decode, empty, header, json_resp, query_param, serve_media_in, text, Resp};
use crate::tenant;
use crate::util::{iso, js_string, now, random_bytes, Error, Result};

// ---------- small helpers ----------

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn operator() -> String {
    env("SIAR_OPERATOR").unwrap_or_else(|| "PT Joyo Digitama Solusi".into())
}

fn location() -> String {
    env("SIAR_OPERATOR_LOCATION").filter(|s| !s.is_empty()).unwrap_or_else(|| "Indonesia".into())
}

fn contact() -> String {
    config().contact.clone()
}

fn public() -> String {
    config().public_url.clone()
}

// ---------- templates (rust/web/, previewable as static files) ----------

const LAYOUT: &str = include_str!("../web/layout.html");
const CSS: &str = include_str!("../web/base.css");
const JS: &str = include_str!("../web/app.js");
const LANDING: &str = include_str!("../web/landing.html");
const DOCS: &str = include_str!("../web/docs.html");
const SETUP: &str = include_str!("../web/setup.html");
const PROMPT: &str = include_str!("../web/prompt.txt");
const GITHUB: &str = "https://github.com/dzulfikar08/siar";

/// Fill `{{name}}` slots in one pass. Values go in as given (escape them first) and are not
/// scanned again; unknown slots stay as they are.
pub fn tpl(t: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(t.len() + vars.iter().map(|(_, v)| v.len()).sum::<usize>());
    let mut rest = t;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            out.push_str(&rest[i..]);
            return out;
        };
        let name = &after[..j];
        match vars.iter().find(|(k, _)| *k == name) {
            Some((_, v)) => out.push_str(v),
            None => out.push_str(&rest[i..i + j + 4]),
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

fn page(title: &str, body: &str, signed_in: bool) -> Resp {
    let nav = if signed_in {
        "<a href=\"/docs\">Docs</a><a href=\"/pricing\">Pricing</a><a class=\"gh\" href=\"https://github.com/dzulfikar08/siar\">GitHub</a><a class=\"btn small\" href=\"/dashboard\">Dashboard</a>"
    } else {
        "<a href=\"/docs\">Docs</a><a href=\"/pricing\">Pricing</a><a class=\"gh\" href=\"https://github.com/dzulfikar08/siar\">GitHub</a><a class=\"btn small\" href=\"/login\">Sign in</a>"
    };
    let full_title = if title.starts_with("siar") {
        title.to_string()
    } else {
        format!("{title} | siar")
    };
    let html = tpl(
        LAYOUT,
        &[
            ("title", esc(&full_title).as_str()),
            ("css", CSS),
            ("nav", nav),
            ("operator", esc(&operator()).as_str()),
            ("location", esc(&location()).as_str()),
            ("contact", esc(&contact()).as_str()),
            ("js", JS),
            ("body", body),
        ],
    );
    Response::from_string(html)
        .with_chunked_threshold(usize::MAX)
        .with_header(header("content-type", "text/html; charset=utf-8"))
}

fn redirect(to: &str, cookie: Option<String>) -> Resp {
    let mut r = empty(303).with_header(header("location", to));
    if let Some(c) = cookie {
        r = r.with_header(header("set-cookie", &c));
    }
    r
}

fn hdr(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// Client IP; behind Cloudflare/a proxy with SIAR_TRUST_PROXY=1 the forwarded address.
fn client_ip(req: &Request) -> String {
    if env("SIAR_TRUST_PROXY").as_deref() == Some("1") {
        if let Some(ip) = hdr(req, "cf-connecting-ip").or_else(|| hdr(req, "x-forwarded-for").and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))) {
            return ip;
        }
    }
    req.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default()
}

fn read_body(req: &mut Request) -> Result<Vec<u8>> {
    let max = env("SIAR_MAX_BODY_MB").and_then(|v| v.parse::<u64>().ok()).unwrap_or(64) * 1_048_576;
    let mut buf = vec![];
    req.as_reader().take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(Error::Msg("Request body too large".into()));
    }
    Ok(buf)
}

fn form(body: &[u8]) -> Map<String, Value> {
    let mut m = Map::new();
    for pair in String::from_utf8_lossy(body).split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(decode(k), Value::from(decode(v)));
    }
    m
}

fn fstr(f: &Map<String, Value>, k: &str) -> String {
    f.get(k).map(js_string).unwrap_or_default()
}

fn cookie(req: &Request, name: &str) -> Option<String> {
    hdr(req, "cookie")?.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn secure() -> &'static str {
    if public().starts_with("https://") {
        "; Secure"
    } else {
        ""
    }
}

// ---------- sessions ----------

struct Session {
    user_id: String,
    csrf: String,
    email: Option<String>,
    tenant: String,
}

const SESSION_DAYS: i64 = 30;

fn session(req: &Request) -> Result<Option<Session>> {
    let Some(raw) = cookie(req, "siar_session").filter(|v| v.len() == 64) else {
        return Ok(None);
    };
    let h = control::sha256_hex(&raw);
    let row: Option<(String, String, String, Option<String>)> = control::with(|c| {
        Ok(c.query_row(
            "SELECT s.hash, s.user_id, s.csrf, u.email FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.hash = ? AND s.expires_at > ?",
            params![h, now()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
    })?;
    let Some((stored, user_id, csrf, email)) = row else { return Ok(None) };
    if !control::ct_eq(&stored, &h) {
        return Ok(None);
    }
    let tenant = match control::tenant_of_user(&user_id)? {
        Some(t) => t,
        None => control::create_tenant(email.as_deref().unwrap_or("My workspace"), Some(&user_id))?,
    };
    Ok(Some(Session { user_id, csrf, email, tenant }))
}

fn new_session(user_id: &str) -> Result<String> {
    let raw = crate::util::hex(&random_bytes(32));
    let csrf = crate::util::hex(&random_bytes(16));
    control::with(|c| {
        c.execute("DELETE FROM sessions WHERE expires_at < ?", [now()])?;
        c.execute(
            "INSERT INTO sessions (hash, user_id, csrf, created_at, expires_at) VALUES (?, ?, ?, ?, ?)",
            params![control::sha256_hex(&raw), user_id, csrf, now(), now() + SESSION_DAYS * 86_400_000],
        )?;
        Ok(())
    })?;
    Ok(format!(
        "siar_session={raw}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_DAYS * 86_400,
        secure()
    ))
}

/// The CSRF token must match, and a browser-sent Origin must be ours.
fn csrf_ok(req: &Request, s: &Session, f: &Map<String, Value>) -> bool {
    if let Some(o) = hdr(req, "origin") {
        if o != "null" && !public().starts_with(&o) {
            return false;
        }
    }
    control::ct_eq(&fstr(f, "csrf"), &s.csrf)
}

fn csrf_field(s: &Session) -> String {
    format!("<input type=\"hidden\" name=\"csrf\" value=\"{}\">", esc(&s.csrf))
}

// ---------- sign-in (GitHub, Google) ----------

struct LoginProvider {
    id: &'static str,
    name: &'static str,
    env: &'static str,
}

const LOGIN: [LoginProvider; 2] = [
    LoginProvider {
        id: "github",
        name: "GitHub",
        env: "GITHUB_LOGIN",
    },
    LoginProvider {
        id: "google",
        name: "Google",
        env: "GOOGLE_LOGIN",
    },
];

fn login_creds(p: &LoginProvider) -> Option<(String, String)> {
    let id = env(&format!("{}_CLIENT_ID", p.env)).filter(|v| !v.is_empty())?;
    Some((id, env(&format!("{}_CLIENT_SECRET", p.env)).unwrap_or_default()))
}

fn login_redirect(p: &LoginProvider) -> String {
    format!("{}/auth/{}/callback", public(), p.id)
}

fn start_login(p: &LoginProvider) -> Result<String> {
    let Some((client_id, _)) = login_creds(p) else {
        return Err(Error::Msg(format!("Sign-in with {} is not configured", p.name)));
    };
    let state = crate::util::hex(&random_bytes(16));
    let verifier = crate::util::hex(&random_bytes(32));
    control::with(|c| {
        c.execute("DELETE FROM login_states WHERE created_at < ?", [now() - 15 * 60_000])?;
        c.execute(
            "INSERT INTO login_states (state, provider, verifier, created_at) VALUES (?, ?, ?, ?)",
            params![state, p.id, verifier, now()],
        )?;
        Ok(())
    })?;
    let redirect = login_redirect(p);
    Ok(match p.id {
        "github" => format!(
            "https://github.com/login/oauth/authorize?{}",
            crate::util::query_string(&[
                ("client_id", client_id),
                ("redirect_uri", redirect),
                ("scope", "read:user user:email".into()),
                ("state", state),
                ("allow_signup", "true".into())
            ])
        ),
        _ => {
            use base64::Engine;
            use sha2::Digest;
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
            format!(
                "https://accounts.google.com/o/oauth2/v2/auth?{}",
                crate::util::query_string(&[
                    ("client_id", client_id),
                    ("redirect_uri", redirect),
                    ("response_type", "code".into()),
                    ("scope", "openid email profile".into()),
                    ("state", state),
                    ("code_challenge", challenge),
                    ("code_challenge_method", "S256".into()),
                    ("prompt", "select_account".into()),
                ])
            )
        }
    })
}

/// (subject, email (verified only), name)
fn finish_login(p: &LoginProvider, code: &str, state: &str) -> Result<(String, Option<String>, String)> {
    let verifier: Option<String> = control::with(|c| {
        let v = c
            .query_row(
                "SELECT verifier FROM login_states WHERE state = ? AND provider = ? AND created_at > ?",
                params![state, p.id, now() - 15 * 60_000],
                |r| r.get(0),
            )
            .optional()?;
        c.execute("DELETE FROM login_states WHERE state = ?", [state])?;
        Ok(v)
    })?;
    let Some(verifier) = verifier else {
        return Err(Error::Msg("This sign-in link expired; try again".into()));
    };
    let Some((id, secret)) = login_creds(p) else {
        return Err(Error::Msg("Sign-in is not configured".into()));
    };
    if p.id == "github" {
        let t = crate::net::post("https://github.com/login/oauth/access_token")
            .header("accept", "application/json")
            .form(&[
                ("client_id", Some(id)),
                ("client_secret", Some(secret)),
                ("code", Some(code.into())),
                ("redirect_uri", Some(login_redirect(p))),
            ])
            .get_json()?;
        let token = t
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Msg("GitHub sign-in failed".into()))?
            .to_string();
        let user = crate::net::get("https://api.github.com/user")
            .bearer(&token)
            .header("accept", "application/vnd.github+json")
            .get_json()?;
        let emails = crate::net::get("https://api.github.com/user/emails")
            .bearer(&token)
            .header("accept", "application/vnd.github+json")
            .get_json()
            .unwrap_or(Value::Null);
        let email = github_verified_email(&emails);
        let name = user
            .get("name")
            .and_then(|v| v.as_str())
            .or(user.get("login").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        Ok((user.get("id").map(js_string).unwrap_or_default(), email, name))
    } else {
        let t = crate::net::post("https://oauth2.googleapis.com/token")
            .form(&[
                ("code", Some(code.into())),
                ("client_id", Some(id)),
                ("client_secret", Some(secret)),
                ("redirect_uri", Some(login_redirect(p))),
                ("grant_type", Some("authorization_code".into())),
                ("code_verifier", Some(verifier)),
            ])
            .get_json()?;
        let token = t
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Msg("Google sign-in failed".into()))?
            .to_string();
        let u = crate::net::get("https://openidconnect.googleapis.com/v1/userinfo").bearer(&token).get_json()?;
        let email = google_verified_email(&u);
        Ok((
            u.get("sub").map(js_string).unwrap_or_default(),
            email,
            u.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ))
    }
}

/// GitHub: only the primary address, and only if GitHub marks it verified (/user/emails).
pub fn github_verified_email(emails: &Value) -> Option<String> {
    emails
        .as_array()?
        .iter()
        .find(|e| e["primary"] == true && e["verified"] == true)
        .and_then(|e| e.get("email").and_then(|v| v.as_str()))
        .filter(|e| e.contains('@'))
        .map(|e| e.to_string())
}

/// Google: the userinfo email, only with email_verified = true.
pub fn google_verified_email(userinfo: &Value) -> Option<String> {
    if userinfo["email_verified"] != true {
        return None;
    }
    userinfo
        .get("email")
        .and_then(|v| v.as_str())
        .filter(|e| e.contains('@'))
        .map(|e| e.to_string())
}

/// The user for an identity: existing, linked to another sign-in by a verified email, or new
/// (with its workspace). `email` must be None unless the provider verified it.
pub fn user_for(provider: &str, subject: &str, email: Option<&str>, name: &str) -> Result<String> {
    if subject.is_empty() {
        return Err(Error::Msg("Sign-in returned no account id".into()));
    }
    let existing: Option<String> = control::with(|c| {
        Ok(c.query_row(
            "SELECT user_id FROM identities WHERE provider = ? AND subject = ?",
            params![provider, subject],
            |r| r.get(0),
        )
        .optional()?)
    })?;
    if let Some(u) = existing {
        return Ok(u);
    }
    let by_email: Option<String> = match email {
        Some(e) => control::with(|c| {
            Ok(c.query_row("SELECT id FROM users WHERE email = ? COLLATE NOCASE", [e], |r| r.get(0))
                .optional()?)
        })?,
        None => None,
    };
    let user = match by_email {
        Some(u) => u,
        None => {
            let u = crate::util::new_id("user");
            control::with(|c| {
                Ok(c.execute(
                    "INSERT INTO users (id, email, name, created_at) VALUES (?, ?, ?, ?)",
                    params![u, email, name, now()],
                )
                .map(|_| ())?)
            })?;
            control::create_tenant(email.unwrap_or(if name.is_empty() { "My workspace" } else { name }), Some(&u))?;
            u
        }
    };
    control::with(|c| {
        Ok(c.execute(
            "INSERT OR IGNORE INTO identities (provider, subject, user_id, email) VALUES (?, ?, ?, ?)",
            params![provider, subject, user, email],
        )
        .map(|_| ())?)
    })?;
    Ok(user)
}

// ---------- router ----------

pub fn route(mut req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let method = req.method().clone();
    let get = method == Method::Get || method == Method::Head;
    let post = method == Method::Post;
    let parts: Vec<&str> = path.split('/').skip(1).collect();

    // Routes that stream files or need the request after responding.
    if get && parts.len() >= 4 && parts[0] == "media" && parts[1] == "t" && valid_tid(parts[2]) {
        let rest = parts[3..].join("/");
        if let Ok(Some((file, ct))) = serve_media_in(&tenant::media_dir_of(parts[2]), &rest) {
            let r = Response::from_file(file)
                .with_chunked_threshold(usize::MAX)
                .with_header(header("cache-control", "public, max-age=86400"))
                .with_header(header("content-type", ct));
            let _ = req.respond(r);
            return;
        }
        let _ = req.respond(text("Not found", 404));
        return;
    }

    let resp = match (parts.as_slice(), get, post) {
        (["health"], true, _) => text("ok", 200),
        ([""], true, _) => landing(&req),
        (["docs"], true, _) => docs(&req),
        (["pricing"], true, _) => pricing(&req),
        (["privacy"], true, _) => privacy(&req),
        (["terms"], true, _) => terms(&req),
        (["login"], true, _) => login_page(&req, query_param(&query, "error")),
        (["auth", p], true, _) => match LOGIN.iter().find(|l| l.id == *p) {
            Some(_) if !limits::allow(&format!("login:{}", client_ip(&req)), 20, 600_000) => {
                login_page(&req, Some("Too many sign-in attempts; wait a few minutes".into()))
            }
            Some(l) => match start_login(l) {
                Ok(u) => redirect(&u, None),
                Err(e) => login_page(&req, Some(e.to_string())),
            },
            None => not_found(&req),
        },
        (["auth", p, "callback"], true, _) => match LOGIN.iter().find(|l| l.id == *p) {
            Some(l) => login_callback(&req, l, &query),
            None => not_found(&req),
        },
        (["logout"], _, true) => logout(&mut req),
        (["dashboard"], true, _) => with_session(&req, |s| dashboard(&req, s, &query, None)),
        (["dashboard", ..], _, true) => dashboard_post(&mut req, &parts[1..]),
        (["dashboard", "connect", platform], true, _) => with_session(&req, |s| credential_form(&req, s, platform)),
        (["dashboard", "billing", "polar", plan], true, _) => with_session(&req, |s| match control::plan(plan).ok().flatten() {
            Some(p) => polar_confirm(s, &p),
            None => not_found(&req),
        }),
        (["dashboard", "billing", "doku", plan, months], true, _) => {
            with_session(&req, |s| match (control::plan(plan).ok().flatten(), months.parse::<i64>()) {
                (Some(p), Ok(m)) => doku_choose(s, &p, m, None),
                _ => not_found(&req),
            })
        }
        (["mcp"], _, _) => mcp(&mut req, &method),
        (["oauth", "t", tid, platform, "callback"], true, _) => oauth_callback(&req, Some(tid), platform, &query),
        (["oauth", platform, "callback"], true, _) => oauth_callback(&req, None, platform, &query),
        (["billing", "polar", "webhook"], _, true) => polar_webhook(&mut req),
        (["billing", "doku", "notify"], _, true) => doku_notify(&mut req, &path),
        (["webhooks", "meta"], true, _) => meta_verify(&query, None),
        (["webhooks", "meta"], _, true) => meta_webhook(&mut req, None),
        (["webhooks", "t", tid, "meta"], true, _) => meta_verify(&query, Some(tid)),
        (["webhooks", "t", tid, "meta"], _, true) => meta_webhook(&mut req, Some(tid)),
        (["l", "t", tid, code], true, _) if crate::server::is_link_code(code) => match in_tenant(tid, || crate::automations::click_link(code)) {
            Ok(Some(to)) => empty(302).with_header(header("location", &to)),
            _ => text("Not found", 404),
        },
        _ => not_found(&req),
    };
    let _ = req.respond(resp);
}

fn valid_tid(t: &str) -> bool {
    !t.is_empty() && t.len() <= 40 && t.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Run `f` inside a tenant, then refresh its scheduler index.
fn in_tenant<T>(tid: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let t = tenant::get(tid)?;
    let _g = tenant::enter(t);
    let r = f();
    let _ = tenant::refresh_due();
    r
}

fn not_found(req: &Request) -> Resp {
    let signed = session(req).ok().flatten().is_some();
    page(
        "Not found",
        "<div class=\"wrap notfound\"><h1>Not found</h1><p class=\"lead\">That page does not exist.</p><div class=\"actions\"><a class=\"btn\" href=\"/\">Go to the home page</a><a class=\"btn ghost\" href=\"/docs\">Read the docs</a></div></div>",
        signed,
    )
    .with_status_code(404)
}

fn with_session(req: &Request, f: impl FnOnce(&Session) -> Resp) -> Resp {
    match session(req) {
        Ok(Some(s)) => f(&s),
        _ => redirect("/login", None),
    }
}

// ---------- MCP by API key ----------

fn mcp(req: &mut Request, method: &Method) -> Resp {
    let token = hdr(req, "authorization")
        .map(|v| {
            if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
                v[7..].trim().to_string()
            } else {
                v
            }
        })
        .unwrap_or_default();
    let hit = control::find_api_key(&token).ok().flatten();
    let Some(hit) = hit else {
        return Response::from_string("Unauthorized")
            .with_status_code(401)
            .with_header(header("www-authenticate", "Bearer"))
            .with_header(header("content-type", "text/plain;charset=utf-8"));
    };
    if *method == Method::Get {
        return Response::from_string("SSE stream not offered; POST JSON-RPC here")
            .with_status_code(405)
            .with_header(header("allow", "POST"))
            .with_header(header("content-type", "text/plain;charset=utf-8"));
    }
    if *method == Method::Delete {
        return empty(204);
    }
    if *method != Method::Post {
        return empty(405);
    }
    let per_min = control::effective_plan(&hit.tenant_id).map(|p| p.api_per_minute).unwrap_or(60);
    if !limits::allow(&format!("key:{}", hit.key_id), per_min, 60_000) {
        return json_resp(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": format!("Rate limit: at most {per_min} requests per minute for this API key on your plan; retry shortly") } }), 429)
            .with_header(header("retry-after", "60"));
    }
    let body = match read_body(req) {
        Ok(b) => b,
        Err(e) => {
            return json_resp(
                &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": e.to_string() } }),
                413,
            )
        }
    };
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return json_resp(
            &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } }),
            400,
        );
    };
    let out = in_tenant(&hit.tenant_id, || Ok(crate::mcp::handle_batch(&v)));
    match out {
        Ok(Some(o)) => json_resp(&o, 200),
        Ok(None) => empty(202),
        Err(e) => json_resp(
            &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32603, "message": e.to_string() } }),
            500,
        ),
    }
}

/// Call an MCP tool inside the current tenant (the dashboard uses the same code paths as agents).
fn tool(name: &str, args: Value) -> std::result::Result<Value, String> {
    let r =
        crate::mcp::handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })).unwrap_or(Value::Null);
    let text = r.pointer("/result/content/0/text").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if r.pointer("/result/isError") == Some(&Value::Bool(true)) || r.get("error").is_some() {
        return Err(if text.is_empty() {
            r.pointer("/error/message").map(js_string).unwrap_or_default()
        } else {
            text
        });
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

// ---------- OAuth callbacks for connecting platform accounts ----------

fn oauth_callback(req: &Request, tid: Option<&str>, platform: &str, query: &str) -> Resp {
    let signed = session(req).ok().flatten().is_some();
    let back = |msg: &str, ok: bool| {
        let body = format!(
            "<h1>{}</h1><p class=\"note {}\">{}</p><p><a class=\"button\" href=\"/dashboard#accounts\">Back to the dashboard</a></p>",
            if ok { "Connected" } else { "Not connected" },
            if ok { "ok" } else { "err" },
            esc(msg)
        );
        page("Connect", &body, signed).with_status_code(if ok { 200 } else { 400 })
    };
    if let Some(err) = query_param(query, "error_description").or_else(|| query_param(query, "error")) {
        return back(&format!("Connection cancelled: {err}"), false);
    }
    let state = query_param(query, "state").unwrap_or_default();
    let tenant_id = match tid {
        Some(t) => Some(t.to_string()),
        None => crate::oauth::tenant_of_state(&state).ok().flatten(),
    };
    let Some(tenant_id) = tenant_id else {
        return back("Unknown or expired OAuth state. Start again from the dashboard or connect_account.", false);
    };
    let code = query_param(query, "code").unwrap_or_default();
    match in_tenant(&tenant_id, || crate::oauth::finish_oauth(platform, &code, &state)) {
        Ok(accounts) => {
            let names: Vec<String> = accounts
                .iter()
                .map(|a| {
                    format!(
                        "{}:{}",
                        js_string(&a["platform"]),
                        a.get("handle").filter(|h| !h.is_null()).or(a.get("name")).map(js_string).unwrap_or_default()
                    )
                })
                .collect();
            back(&format!("Connected {}.", names.join(", ")), true)
        }
        Err(e) => back(&format!("Connection failed: {e}"), false),
    }
}

// ---------- payment webhooks ----------

fn polar_webhook(req: &mut Request) -> Resp {
    let body = match read_body(req) {
        Ok(b) => b,
        Err(_) => return text("Too large", 413),
    };
    let r = billing::handle_polar(
        &hdr(req, "webhook-id").unwrap_or_default(),
        &hdr(req, "webhook-timestamp").unwrap_or_default(),
        &hdr(req, "webhook-signature").unwrap_or_default(),
        &body,
    );
    match r {
        Ok(h) => text(&format!("{h:?}").to_lowercase(), 200),
        Err(e) => {
            eprintln!("{} [billing] polar webhook: {e}", iso(now()));
            text(&e.to_string(), billing::error_status(&e))
        }
    }
}

fn doku_notify(req: &mut Request, path: &str) -> Resp {
    let body = match read_body(req) {
        Ok(b) => b,
        Err(_) => return text("Too large", 413),
    };
    let r = billing::handle_doku(
        &hdr(req, "client-id").unwrap_or_default(),
        &hdr(req, "request-id").unwrap_or_default(),
        &hdr(req, "request-timestamp").unwrap_or_default(),
        &hdr(req, "signature").unwrap_or_default(),
        path,
        &body,
    );
    match r {
        Ok(_) => text("OK", 200),
        Err(e) => {
            eprintln!("{} [billing] doku notification: {e}", iso(now()));
            text(&e.to_string(), billing::error_status(&e))
        }
    }
}

// ---------- Meta webhooks ----------

fn meta_verify(query: &str, tid: Option<&str>) -> Resp {
    let want = match tid {
        Some(t) => in_tenant(t, || {
            Ok(tenant::byo_creds("INSTAGRAM")?.and_then(|c| c.get("webhook_verify_token").and_then(|v| v.as_str()).map(|s| s.to_string())))
        })
        .ok()
        .flatten(),
        None => env("SIAR_META_VERIFY_TOKEN"),
    }
    .filter(|w| !w.is_empty());
    let ok = want.is_some() && query_param(query, "hub.mode").as_deref() == Some("subscribe") && query_param(query, "hub.verify_token") == want;
    if ok {
        text(&query_param(query, "hub.challenge").unwrap_or_default(), 200)
    } else {
        text("Forbidden", 403)
    }
}

fn meta_webhook(req: &mut Request, tid: Option<&str>) -> Resp {
    let raw = match read_body(req) {
        Ok(b) => b,
        Err(_) => return text("Too large", 413),
    };
    let sig = hdr(req, "x-hub-signature-256");
    let ok = match tid {
        // A tenant's own Meta app: its app secrets.
        Some(t) => in_tenant(t, || {
            let mut secrets = vec![];
            for p in ["INSTAGRAM", "FACEBOOK", "INSTAGRAM_LOGIN"] {
                if let Some(c) = tenant::byo_creds(p)? {
                    secrets.push(c.get("client_secret").and_then(|v| v.as_str()).unwrap_or("").to_string());
                }
            }
            Ok(crate::automations::verify_meta_signature_with(&raw, sig.as_deref(), &secrets))
        })
        .unwrap_or(false),
        None => crate::automations::verify_meta_signature(&raw, sig.as_deref()),
    };
    if !ok {
        return text("Bad signature", 401);
    }
    let tid = tid.map(|t| t.to_string());
    std::thread::spawn(move || {
        let Ok(body) = serde_json::from_slice::<Value>(&raw) else { return };
        // Route each entry to the tenant(s) that connected that Instagram account.
        for entry in body.get("entry").and_then(|e| e.as_array()).into_iter().flatten() {
            let ext = entry.get("id").map(js_string).unwrap_or_default();
            let tenants = match &tid {
                Some(t) => vec![t.clone()],
                None => control::tenants_for_account(&["instagram", "instagram-login", "instagram-facebook"], &ext).unwrap_or_default(),
            };
            let one = json!({ "object": body.get("object"), "entry": [entry] });
            for t in tenants {
                if let Err(e) = in_tenant(&t, || crate::automations::handle_meta_webhook(&one)) {
                    eprintln!("[webhook] {t}: {e}");
                }
            }
        }
    });
    text("ok", 200)
}

// ---------- sign-in pages ----------

fn login_page(req: &Request, error: Option<String>) -> Resp {
    if session(req).ok().flatten().is_some() && error.is_none() {
        return redirect("/dashboard", None);
    }
    let mut buttons = String::new();
    for p in LOGIN.iter() {
        if login_creds(p).is_some() {
            buttons.push_str(&format!(
                "<a class=\"btn{}\" href=\"/auth/{}\">Sign in with {}</a>",
                if buttons.is_empty() { "" } else { " ghost" },
                p.id,
                p.name
            ));
        }
    }
    if buttons.is_empty() {
        buttons = "<p class=\"note warn\">Sign-in is not configured on this server yet.</p>".into();
    }
    let err = error
        .map(|e| format!("<p class=\"note err\" role=\"alert\">{}</p>", esc(&e)))
        .unwrap_or_default();
    let free = control::plans()
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.public && p.price_usd_cents == 0)
        .map(|p| {
            format!(
                "<p class=\"muted small\">The free plan covers {} accounts, {} posts a month and {} MB of media. No card needed.</p>",
                p.max_accounts, p.max_posts_month, p.max_storage_mb
            )
        })
        .unwrap_or_default();
    page(
        "Sign in",
        &format!(
            "<div class=\"wrap signin\"><section class=\"signin-box\" aria-labelledby=\"signin-title\"><h1 id=\"signin-title\">Sign in to siar</h1>{err}\
<p class=\"muted\">One account per person. Signing in for the first time creates your workspace.</p><div class=\"providers\">{buttons}</div>\
<p class=\"muted small\">By signing in you accept the <a href=\"/terms\">terms</a> and the <a href=\"/privacy\">privacy policy</a>.</p></section>\
<section class=\"next\" aria-labelledby=\"next-title\"><h2 id=\"next-title\">What happens next</h2><ol class=\"steps\">\
<li><strong>Create an API key</strong><span>One click in the dashboard. It is shown once.</span></li>\
<li><strong>Paste one snippet into your agent</strong><span>Claude Code, Claude Desktop, Cursor, Codex, Windsurf or any MCP client.</span></li>\
<li><strong>Connect your accounts</strong><span>In the dashboard, or ask your agent to do it.</span></li></ol>{free}</section></div>"
        ),
        false,
    )
}

fn login_callback(req: &Request, p: &LoginProvider, query: &str) -> Resp {
    if let Some(e) = query_param(query, "error") {
        return login_page(req, Some(format!("Sign-in cancelled: {e}")));
    }
    let r = finish_login(
        p,
        &query_param(query, "code").unwrap_or_default(),
        &query_param(query, "state").unwrap_or_default(),
    )
    .and_then(|(sub, email, name)| user_for(p.id, &sub, email.as_deref(), &name))
    .and_then(|u| new_session(&u));
    match r {
        Ok(cookie) => redirect("/dashboard", Some(cookie)),
        Err(e) => login_page(req, Some(e.to_string())),
    }
}

fn logout(req: &mut Request) -> Resp {
    let body = read_body(req).unwrap_or_default();
    let f = form(&body);
    if let Ok(Some(s)) = session(req) {
        if csrf_ok(req, &s, &f) {
            if let Some(raw) = cookie(req, "siar_session") {
                let _ = control::with(|c| Ok(c.execute("DELETE FROM sessions WHERE hash = ?", [control::sha256_hex(&raw)]).map(|_| ())?));
            }
        }
    }
    redirect("/", Some(format!("siar_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", secure())))
}

// ---------- dashboard ----------

/// OAuth app prefixes a tenant can bring: (env prefix, label, platform ids using it).
fn app_prefixes() -> Vec<(String, Vec<&'static platforms::Platform>)> {
    let mut out: Vec<(String, Vec<&'static platforms::Platform>)> = vec![];
    for p in platforms::all() {
        if let Some(env) = p.oauth_env() {
            match out.iter_mut().find(|(e, _)| e == env) {
                Some((_, v)) => v.push(p),
                None => out.push((env.to_string(), vec![p])),
            }
        }
    }
    out
}

fn fmt_ts(ms: Option<i64>) -> String {
    ms.filter(|m| *m > 0)
        .map(|m| iso(m)[..16].replace('T', " ") + " UTC")
        .unwrap_or_else(|| "–".into())
}

fn dashboard(_req: &Request, s: &Session, query: &str, shown_key: Option<(String, String)>) -> Resp {
    let r = in_tenant(&s.tenant, || dashboard_body(s, query, shown_key));
    match r {
        Ok(body) => page("Dashboard", &body, true),
        Err(e) => page(
            "Dashboard",
            &format!(
                "<div class=\"wrap\"><div class=\"dash-head\"><h1>Dashboard</h1></div><p class=\"note err\">{}</p></div>",
                esc(&e.to_string())
            ),
            true,
        ),
    }
    .with_header(header("cache-control", "no-store"))
    .with_header(header("referrer-policy", "same-origin"))
    .with_header(header("x-frame-options", "DENY"))
}

/// The setup guide in docs/setup/ for an app prefix.
fn setup_guide(prefix: &str) -> (&'static str, &'static str) {
    match prefix {
        "INSTAGRAM" | "INSTAGRAM_LOGIN" | "FACEBOOK" | "THREADS" => ("meta.md", "Meta setup guide"),
        "GOOGLE" => ("google.md", "Google setup guide"),
        "LINKEDIN" | "LINKEDIN_PAGE" => ("linkedin.md", "LinkedIn setup guide"),
        _ => ("other.md", "setup guide"),
    }
}

/// One usage meter: label, used, limit, unit suffix.
fn meter(label: &str, used: f64, limit: i64, shown: &str, unit: &str) -> String {
    let pct = if limit > 0 { (used / limit as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    format!(
        "<div class=\"meter\"><div class=\"label\">{label}</div><div class=\"val\">{shown} <small>of {limit}{unit}</small></div><div class=\"bar{}\" aria-hidden=\"true\"><span style=\"width:{pct:.0}%\"></span></div></div>",
        if pct >= 100.0 { " full" } else { "" }
    )
}

fn dashboard_body(s: &Session, query: &str, shown_key: Option<(String, String)>) -> Result<String> {
    let csrf = csrf_field(s);
    let plan = control::effective_plan(&s.tenant)?;
    let keys = control::list_api_keys(&s.tenant)?;
    let live_keys: Vec<&Value> = keys.iter().filter(|k| k["revoked"] != true).collect();
    let accounts = crate::accounts::list_accounts(None)?;
    let mut b = format!(
        "<div class=\"wrap\"><div class=\"dash-head\"><div><h1>Dashboard</h1><p>Signed in as {}. Workspace <code>{}</code>. Plan <span class=\"plan-tag\">{}</span></p></div>\
<form class=\"inline\" method=\"post\" action=\"/logout\">{csrf}<button class=\"ghost small\">Sign out</button></form></div>",
        esc(s.email.as_deref().unwrap_or("(no email)")),
        esc(&s.tenant),
        esc(&plan.name)
    );
    if let Some(m) = query_param(query, "msg") {
        b.push_str(&format!("<p class=\"note ok\" role=\"status\">{}</p>", esc(&m)));
    }
    if let Some(m) = query_param(query, "err") {
        b.push_str(&format!("<p class=\"note err\" role=\"alert\">{}</p>", esc(&m)));
    }
    if query_param(query, "checkout").is_some() {
        b.push_str("<p class=\"note ok\" role=\"status\">Thanks. Your plan changes as soon as the payment provider confirms the payment (usually within a minute); refresh this page.</p>");
    }

    // A key that was just created: shown once, with the snippets already filled in.
    let mcp_url = format!("{}/mcp", public());
    if let Some((name, key)) = &shown_key {
        b.push_str(&format!(
            "<section class=\"newkey\" role=\"status\" aria-labelledby=\"newkey-title\"><h3 id=\"newkey-title\">New key “{}”: copy it now</h3><p class=\"muted small\">It is shown only once. We keep a hash, so it cannot be shown again.</p>\
<div class=\"code\"><pre class=\"wrap-lines\" data-what=\"API key\"><code>{}</code></pre></div>\
<p>Your key is already filled in below. Paste the snippet for your agent:</p>{}</section>",
            esc(name),
            esc(key),
            setup_tabs(&mcp_url, key)
        ));
    }

    // First-run checklist, until all three are done.
    let has_key = !live_keys.is_empty();
    let used = live_keys.iter().any(|k| k["last_used_at"].as_i64().unwrap_or(0) > 0);
    let has_acct = !accounts.is_empty();
    if !(has_key && used && has_acct) {
        let steps = [
            (has_key, "Create an API key", "Your agent signs in to siar with it.", "#keys", "Create a key"),
            (
                used,
                "Add siar to your agent",
                "Paste one snippet into Claude Code, Cursor, Codex or another MCP client. This turns green after your agent's first call.",
                "/docs#agents",
                "Get the snippet",
            ),
            (
                has_acct,
                "Connect an account",
                "Below, or ask your agent: \"connect my Bluesky account\".",
                "#accounts",
                "Connect an account",
            ),
        ];
        let first_open = steps.iter().position(|st| !st.0);
        b.push_str("<section class=\"checklist\" aria-labelledby=\"start-title\"><h2 id=\"start-title\">Get your agent posting</h2><ol>");
        for (i, (done, title, text, href, label)) in steps.iter().enumerate() {
            let (cls, state) = if *done {
                ("done", "Done")
            } else if Some(i) == first_open {
                ("now", "Next")
            } else {
                ("", "To do")
            };
            b.push_str(&format!(
                "<li class=\"{cls}\"><span class=\"state\">Step {} of 3: {state}</span><strong>{title}</strong><p>{}</p>{}</li>",
                i + 1,
                esc(text),
                if *done {
                    String::new()
                } else {
                    format!(
                        "<a class=\"btn small{}\" href=\"{href}\">{label}</a>",
                        if Some(i) == first_open { "" } else { " ghost" }
                    )
                }
            ));
        }
        b.push_str("</ol></section>");
    }

    // Usage
    let storage: i64 = crate::db::with(|c| Ok(c.query_row("SELECT COALESCE(SUM(size), 0) FROM media", [], |r| r.get(0))?))?;
    let posts = control::posts_this_month(&s.tenant)?;
    let storage_mb = storage as f64 / 1_048_576.0;
    b.push_str(&format!(
        "<section id=\"usage\" aria-labelledby=\"usage-title\"><h2 id=\"usage-title\" class=\"sr-only\">Usage</h2><div class=\"meters\">{}{}{}</div>\
<p class=\"hint\">On the {} plan: uploads up to {} MB each, {} API requests per minute per key. <a href=\"/pricing\">Compare plans</a></p></section>",
        meter("Connected accounts", accounts.len() as f64, plan.max_accounts, &accounts.len().to_string(), ""),
        meter("Posts this month", posts as f64, plan.max_posts_month, &posts.to_string(), ""),
        meter("Media storage", storage_mb, plan.max_storage_mb, &format!("{storage_mb:.1}"), " MB"),
        esc(&plan.name),
        plan.max_upload_mb,
        plan.api_per_minute
    ));
    b.push_str("<ul class=\"jump\" aria-label=\"Sections\"><li><a href=\"#keys\">API keys</a></li><li><a href=\"#accounts\">Accounts</a></li><li><a href=\"#apps\">Your platform apps</a></li><li><a href=\"#billing\">Billing</a></li><li><a href=\"#delete\">Delete account</a></li></ul>");

    // API keys
    b.push_str(&format!(
        "<section class=\"dsec\" id=\"keys\" aria-labelledby=\"keys-title\"><h2 id=\"keys-title\">API keys</h2><p class=\"lede\">Agents send a key as <code>Authorization: Bearer …</code> to <code>{}</code>. One key per agent or machine makes it easy to revoke one. Setup per agent is in the <a href=\"/docs#agents\">docs</a>.</p>",
        esc(&mcp_url)
    ));
    if live_keys.is_empty() {
        b.push_str("<div class=\"empty\"><strong>No keys yet.</strong> Create one below; it is shown once, right after you create it.</div>");
    } else {
        b.push_str("<div class=\"table-scroll\"><table class=\"data stack\"><thead><tr><th>Name</th><th>Key</th><th>Created</th><th>Last used</th><th><span class=\"sr-only\">Actions</span></th></tr></thead><tbody>");
        for k in &live_keys {
            let name = js_string(&k["name"]);
            b.push_str(&format!(
                "<tr><td data-l=\"Name\">{}</td><td data-l=\"Key\"><code>{}…</code></td><td data-l=\"Created\">{}</td><td data-l=\"Last used\">{}</td><td><form class=\"inline\" method=\"post\" action=\"/dashboard/keys/{}/revoke\">{csrf}<button class=\"ghost small\" aria-label=\"Revoke key {}\">Revoke</button></form></td></tr>",
                esc(&name),
                esc(&js_string(&k["prefix"])),
                fmt_ts(k["created_at"].as_i64()),
                if k["last_used_at"].as_i64().unwrap_or(0) > 0 { fmt_ts(k["last_used_at"].as_i64()) } else { "Never".into() },
                esc(&js_string(&k["id"])),
                esc(&name)
            ));
        }
        b.push_str("</tbody></table></div>");
    }
    b.push_str(&format!(
        "<form class=\"keyform\" method=\"post\" action=\"/dashboard/keys\">{csrf}<div><label for=\"keyname\">New key name</label><input id=\"keyname\" name=\"name\" placeholder=\"Claude Code on my laptop\" maxlength=\"60\"></div><button>Create key</button></form></section>"
    ));

    // Accounts
    b.push_str("<section class=\"dsec\" id=\"accounts\" aria-labelledby=\"acc-title\"><h2 id=\"acc-title\">Connected accounts</h2>");
    b.push_str(&format!(
        "<p class=\"lede\">{} of {} accounts on the {} plan. Your agent posts to these with <code>schedule_post</code> and <code>publish_now</code>.</p>",
        accounts.len(),
        plan.max_accounts,
        esc(&plan.name)
    ));
    if accounts.is_empty() {
        b.push_str(
            "<div class=\"empty\"><strong>No accounts connected yet.</strong> Pick a network below, or ask your agent: \"connect my Bluesky account\".</div>",
        );
    } else {
        b.push_str("<div class=\"table-scroll\"><table class=\"data stack\"><thead><tr><th>Platform</th><th>Account</th><th>Status</th><th>Token expires</th><th><span class=\"sr-only\">Actions</span></th></tr></thead><tbody>");
        for a in &accounts {
            let status = js_string(&a["status"]);
            b.push_str(&format!(
                "<tr><td data-l=\"Platform\">{}</td><td data-l=\"Account\">{}</td><td data-l=\"Status\" class=\"{}\">{}</td><td data-l=\"Token expires\">{}</td><td><form class=\"inline\" method=\"post\" action=\"/dashboard/accounts/{}/disconnect\">{csrf}<button class=\"ghost small\" aria-label=\"Disconnect {}\">Disconnect</button></form></td></tr>",
                esc(&js_string(&a["platform"])),
                esc(&a.get("handle").filter(|h| !h.is_null()).map(js_string).unwrap_or_else(|| js_string(&a["name"]))),
                if status == "active" || status == "ok" { "status-ok" } else { "status-bad" },
                esc(&status),
                esc(&a.get("expires_at").filter(|h| !h.is_null()).map(js_string).unwrap_or_else(|| "–".into())),
                esc(&js_string(&a["id"])),
                esc(&js_string(&a["name"]))
            ));
        }
        b.push_str("</tbody></table></div>");
    }
    let prefixes = app_prefixes();
    let mut app_state: Vec<(String, bool, bool)> = vec![];
    for (prefix, _) in &prefixes {
        app_state.push((prefix.clone(), tenant::byo_creds(prefix)?.is_some(), tenant::shared_app_enabled(prefix)));
    }
    let (mut oauth, mut creds) = (String::new(), String::new());
    for p in platforms::all() {
        if let Some(env) = p.oauth_env() {
            let (saved, shared) = app_state.iter().find(|a| a.0 == env).map(|a| (a.1, a.2)).unwrap_or((false, false));
            let need = if saved {
                "Uses your saved app".to_string()
            } else if shared {
                "Uses siar's app".to_string()
            } else {
                format!("Needs your own app: <a href=\"#app-{}\">set it up</a>", esc(env))
            };
            oauth.push_str(&format!(
                "<div><form method=\"post\" action=\"/dashboard/connect/{}\">{csrf}<button class=\"ghost small\">Connect {}</button></form><span class=\"need\">{need}</span></div>",
                esc(&p.id),
                esc(&p.name)
            ));
        } else {
            creds.push_str(&format!(
                "<div><a class=\"btn ghost small\" href=\"/dashboard/connect/{}\">Connect {}</a></div>",
                esc(&p.id),
                esc(&p.name)
            ));
        }
    }
    b.push_str(&format!(
        "<div class=\"connect-group\"><h3>Sign in with the platform</h3><div class=\"connect\">{oauth}</div></div><div class=\"connect-group\"><h3>Token or password</h3><div class=\"connect\">{creds}</div></div></section>"
    ));

    // Bring your own app
    b.push_str("<section class=\"dsec\" id=\"apps\" aria-labelledby=\"apps-title\"><h2 id=\"apps-title\">Your own platform apps</h2><p class=\"lede\">OAuth platforms need a developer app. Create one on the platform, register the redirect URI shown here, and save its client id and secret (stored encrypted with your workspace key). When siar's shared app for a platform is approved, you can connect without your own.</p>");
    for (prefix, ps) in &prefixes {
        let (saved, shared) = app_state.iter().find(|a| &a.0 == prefix).map(|a| (a.1, a.2)).unwrap_or((false, false));
        let (status, cls) = if saved {
            ("your app is saved", "status-ok")
        } else if shared {
            ("siar's shared app is available", "status-ok")
        } else {
            ("needs your own app", "muted")
        };
        let uris: Vec<String> = ps
            .iter()
            .map(|p| {
                format!(
                    "<code class=\"uri\">{}/oauth/t/{}/{}/callback</code>",
                    esc(&public()),
                    esc(&s.tenant),
                    esc(&p.id)
                )
            })
            .collect();
        let names: Vec<&str> = ps.iter().map(|p| p.name.as_str()).collect();
        let (guide, guide_name) = setup_guide(prefix);
        let extra = if prefix == "INSTAGRAM" {
            format!(
                "<label for=\"cfg-{prefix}\">Login configuration id (Facebook Login for Business)</label><input id=\"cfg-{prefix}\" name=\"config_id\"><label for=\"wh-{prefix}\">Webhook verify token (optional)</label><input id=\"wh-{prefix}\" name=\"webhook_verify_token\"><p class=\"hint\">Meta webhooks URL for this app: <code>{}/webhooks/t/{}/meta</code></p>",
                esc(&public()),
                esc(&s.tenant)
            )
        } else {
            String::new()
        };
        b.push_str(&format!(
            "<details class=\"app\" id=\"app-{prefix}\"><summary><strong>{}</strong><span class=\"for\">{}</span><span class=\"st {cls}\">{status}</span></summary><div class=\"inner\">\
<p>Redirect URI(s) to register: {}</p><p class=\"small muted\">Step by step: the <a href=\"{GITHUB}/blob/main/docs/setup/{guide}\">{guide_name}</a> (written for self-hosting: use the redirect URI above).</p>\
<form method=\"post\" action=\"/dashboard/apps/{prefix}\">{csrf}<label for=\"id-{prefix}\">Client id</label><input id=\"id-{prefix}\" name=\"client_id\" required autocomplete=\"off\">\
<label for=\"sec-{prefix}\">Client secret</label><input id=\"sec-{prefix}\" name=\"client_secret\" type=\"password\" autocomplete=\"off\">{extra}<div class=\"actions\"><button>Save</button></div></form>{}</div></details>",
            esc(prefix),
            esc(&names.join(", ")),
            uris.join(" "),
            if saved {
                format!("<form class=\"actions\" method=\"post\" action=\"/dashboard/apps/{prefix}/delete\">{csrf}<button class=\"ghost small\">Remove saved app</button></form>")
            } else {
                String::new()
            }
        ));
    }
    b.push_str("</section>");

    // Billing
    let sub = control::subscription(&s.tenant)?;
    b.push_str(&format!(
        "<section class=\"dsec\" id=\"billing\" aria-labelledby=\"billing-title\"><h2 id=\"billing-title\">Billing</h2><p class=\"billing-now\">Current plan: <strong>{}</strong>",
        esc(&plan.name)
    ));
    if let Some(sub) = &sub {
        b.push_str(&format!(
            " ({} via {}, {} {})",
            esc(&sub.status),
            esc(&sub.provider),
            if sub.cancel_at_period_end { "ends" } else { "renews" },
            fmt_ts(sub.current_period_end)
        ));
        if sub.provider == "doku" && sub.status == "active" && sub.current_period_end.map(|e| e - now() < 7 * 86_400_000).unwrap_or(false) {
            b.push_str("</p><p class=\"note warn\" role=\"status\">Your prepaid period ends within 7 days. Pay for another period below to keep your plan.");
        }
    }
    b.push_str("</p><p class=\"muted small\">Card prices include Polar's processing fee; rupiah payments show DOKU's admin fee as its own line before you pay. <a href=\"/pricing\">Pricing and fees</a></p><div class=\"plans\">");
    for p in control::plans()?.into_iter().filter(|p| p.public && p.price_usd_cents > 0) {
        let mut buttons = String::new();
        if billing::polar_enabled() && p.polar_product_id.is_some() {
            let price = billing::polar_price(&p);
            buttons.push_str(&format!(
                "<a class=\"btn\" href=\"/dashboard/billing/polar/{}\">Pay by card: {}/{} (incl. {} fee)</a>",
                esc(&p.id),
                usd(price.total),
                esc(&p.interval),
                usd(price.fee)
            ));
        }
        if billing::doku_enabled() {
            for (months, amount, currency) in control::plan_prices(&p.id, "doku")? {
                buttons.push_str(&format!(
                    "<a class=\"btn ghost\" href=\"/dashboard/billing/doku/{}/{months}\">Pay {} {} + admin fee for {months} month{} (QRIS, VA, e-wallet)</a>",
                    esc(&p.id),
                    esc(&currency),
                    fmt_amount(amount),
                    if months == 1 { "" } else { "s" }
                ));
            }
        }
        b.push_str(&format!(
            "<div class=\"plan{}\"><h3>{}</h3><p class=\"desc\">{}</p><p class=\"small\">{} accounts, {} posts a month, {} media storage</p><div class=\"actions pay\">{}</div></div>",
            if p.id == plan.id { " paid" } else { "" },
            esc(&p.name),
            esc(&p.description),
            p.max_accounts,
            p.max_posts_month,
            mb(p.max_storage_mb),
            if buttons.is_empty() {
                "<p class=\"muted\">Payments are not enabled yet.</p>".into()
            } else {
                buttons
            }
        ));
    }
    b.push_str("</div></section>");

    // Delete
    b.push_str(&format!(
        "<section class=\"dsec\" id=\"delete\" aria-labelledby=\"delete-title\"><div class=\"danger-zone\"><h2 id=\"delete-title\">Delete account</h2><p>Deletes your workspace: connected accounts and their tokens, posts, media, API keys and sign-in. This cannot be undone. <strong>A card subscription (Polar) is cancelled immediately, with no refund for the rest of the period.</strong> A prepaid rupiah period ends with the account and is not refunded.</p>\
<form method=\"post\" action=\"/dashboard/delete\">{csrf}<label for=\"confirm\">Type DELETE to confirm</label><input id=\"confirm\" name=\"confirm\" autocomplete=\"off\"><div class=\"actions\"><button class=\"danger\">Delete my account</button></div></form></div></section></div>"
    ));
    Ok(b)
}

fn fmt_amount(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

fn usd(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

fn idr(amount: i64) -> String {
    format!("Rp{}", fmt_amount(amount))
}

/// Before Polar: the plan's net price, the card processing fee the customer pays, the total.
fn polar_confirm(s: &Session, p: &control::Plan) -> Resp {
    let price = billing::polar_price(p);
    let f = billing::polar_fees();
    page(
        "Confirm payment",
        &format!(
            "<div class=\"wrap narrow\"><h1>Subscribe to {}</h1><p class=\"muted\">Check the amounts, then pay on Polar's checkout page.</p><div class=\"receipt\"><table><tbody><tr><th>{} plan, per {}</th><td>{}</td></tr><tr><th>Card processing fee (Polar: {}% + {}{})</th><td>{}</td></tr><tr><th>Total per {}</th><td><strong>{}</strong></td></tr></tbody></table></div>\
<p class=\"muted small\">The fee is what Polar, our merchant of record, charges for the payment; we pass it on at cost. Polar may add sales tax or VAT for your country at checkout. Renews every {} until you cancel.</p>\
<form method=\"post\" action=\"/dashboard/billing/polar/{}\">{}<div class=\"actions\"><button>Continue to Polar</button><a href=\"/dashboard#billing\">Cancel</a></div></form></div>",
            esc(&p.name),
            esc(&p.name),
            esc(&p.interval),
            usd(price.net),
            f.percent,
            usd(f.fixed_cents),
            if f.extra_percent > 0.0 { format!(", +{}% for cards outside the US", f.extra_percent) } else { String::new() },
            usd(price.fee),
            esc(&p.interval),
            usd(price.total),
            esc(&p.interval),
            esc(&p.id),
            csrf_field(s)
        ),
        true,
    )
}

/// The customer picks the DOKU payment method here, seeing each method's admin fee.
fn doku_choose(s: &Session, p: &control::Plan, months: i64, error: Option<&str>) -> Resp {
    let mut rows = String::new();
    for (i, m) in billing::doku_methods().unwrap_or_default().iter().enumerate() {
        if let Ok(q) = billing::doku_quote(p, months, &m.method) {
            rows.push_str(&format!(
                "<tr><td><label for=\"m{i}\"><input type=\"radio\" name=\"method\" id=\"m{i}\" value=\"{}\"{}>{}</label></td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                esc(&m.method),
                if i == 0 { " required" } else { "" },
                esc(&m.label),
                idr(q.fee),
                idr(q.total)
            ));
        }
    }
    let net = control::plan_prices(&p.id, "doku")
        .unwrap_or_default()
        .into_iter()
        .find(|x| x.0 == months)
        .map(|x| x.1)
        .unwrap_or(0);
    page(
        "Choose a payment method",
        &format!(
            "<div class=\"wrap narrow\"><h1>{} for {} month{}</h1>{}<p>Price {}. Each payment method has its own admin fee (Biaya admin), charged by DOKU and passed on at cost:</p>\
<form method=\"post\" action=\"/dashboard/billing/doku/{}/{months}\">{}<fieldset class=\"bare\"><legend class=\"sr-only\">Payment method</legend><div class=\"receipt\"><table class=\"methods\"><thead><tr><th>Method</th><th class=\"num\">Biaya admin</th><th class=\"num\">Total</th></tr></thead><tbody>{rows}</tbody></table></div></fieldset>\
<div class=\"actions\"><button>Continue</button><a href=\"/dashboard#billing\">Cancel</a></div></form><p class=\"muted small\">Prepaid: no automatic renewal. We e-mail you 7 days and 1 day before the period ends.</p></div>",
            esc(&p.name),
            months,
            if months == 1 { "" } else { "s" },
            error.map(|e| format!("<p class=\"note err\" role=\"alert\">{}</p>", esc(e))).unwrap_or_default(),
            idr(net),
            esc(&p.id),
            csrf_field(s)
        ),
        true,
    )
}

/// Last step before DOKU: the breakdown with the admin fee as its own line.
fn doku_confirm(s: &Session, p: &control::Plan, months: i64, method: &str) -> Resp {
    match billing::doku_quote(p, months, method) {
        Ok(q) => page(
            "Confirm payment",
            &format!(
                "<div class=\"wrap narrow\"><h1>Confirm payment</h1><p class=\"muted\">Check the amounts, then pay on DOKU's page.</p><div class=\"receipt\"><table><tbody><tr><th>siar {} ({} month{})</th><td>{}</td></tr><tr><th>Biaya admin {}</th><td>{}</td></tr><tr><th>Total</th><td><strong>{}</strong></td></tr></tbody></table></div>\
<form method=\"post\" action=\"/dashboard/billing/doku/{}/{months}\">{}<input type=\"hidden\" name=\"method\" value=\"{}\"><input type=\"hidden\" name=\"confirm\" value=\"1\"><div class=\"actions\"><button>Pay with {}</button><a href=\"/dashboard/billing/doku/{}/{months}\">Choose another method</a></div></form><p class=\"muted small\">Prepaid: no automatic renewal.</p></div>",
                esc(&p.name),
                months,
                if months == 1 { "" } else { "s" },
                idr(q.net),
                esc(&q.label),
                idr(q.fee),
                idr(q.total),
                esc(&p.id),
                csrf_field(s),
                esc(method),
                esc(&q.label),
                esc(&p.id)
            ),
            true,
        ),
        Err(e) => doku_choose(s, p, months, Some(&e.to_string())),
    }
}

fn credential_form(req: &Request, s: &Session, platform: &str) -> Resp {
    let Ok(p) = platforms::get(platform) else { return not_found(req) };
    if p.oauth_env().is_some() {
        return redirect("/dashboard#accounts", None);
    }
    let mut fields = String::new();
    for f in p.meta["auth"]["fields"].as_array().into_iter().flatten() {
        let key = js_string(&f["key"]);
        let secret = f.get("secret") == Some(&Value::Bool(true));
        let optional = f.get("optional") == Some(&Value::Bool(true));
        fields.push_str(&format!(
            "<label for=\"f-{k}\">{}{}</label><input id=\"f-{k}\" name=\"cred_{k}\" type=\"{}\" autocomplete=\"off\" value=\"{}\"{}>",
            esc(&js_string(&f["label"])),
            if optional { " (optional)" } else { "" },
            if secret { "password" } else { "text" },
            esc(&f.get("default").map(js_string).unwrap_or_default()),
            if optional { "" } else { " required" },
            k = esc(&key)
        ));
    }
    let help = p.meta["auth"]
        .get("help")
        .map(|h| format!("<p class=\"muted\">{}</p>", esc(&js_string(h))))
        .unwrap_or_default();
    page(
        &format!("Connect {}", p.name),
        &format!(
            "<div class=\"wrap narrow\"><h1>Connect {}</h1>{help}<form method=\"post\" action=\"/dashboard/connect/{}\">{}{fields}<div class=\"actions\"><button>Connect</button><a href=\"/dashboard#accounts\">Cancel</a></div></form><p class=\"muted small\">siar checks these with {} and stores them encrypted with your workspace key.</p></div>",
            esc(&p.name),
            esc(&p.id),
            csrf_field(s),
            esc(&p.name)
        ),
        true,
    )
}

fn back(msg: Result<String>) -> Resp {
    match msg {
        Ok(m) => redirect(&format!("/dashboard?msg={}", crate::util::form_encode(&m)), None),
        Err(e) => redirect(&format!("/dashboard?err={}", crate::util::form_encode(&e.to_string())), None),
    }
}

fn dashboard_post(req: &mut Request, parts: &[&str]) -> Resp {
    let Ok(Some(s)) = session(req) else { return redirect("/login", None) };
    let body = match read_body(req) {
        Ok(b) => b,
        Err(e) => return back(Err(e)),
    };
    let f = form(&body);
    if !csrf_ok(req, &s, &f) {
        return text("Invalid form token; reload the page and try again", 403);
    }
    if !limits::allow(&format!("dash:{}", s.user_id), 120, 60_000) {
        return back(Err(Error::Msg("Too many requests; wait a minute".into())));
    }
    match parts {
        ["keys"] => match control::create_api_key(&s.tenant, &fstr(&f, "name")) {
            Ok((_, key)) => dashboard(req, &s, "", Some((fstr(&f, "name"), key))),
            Err(e) => back(Err(e)),
        },
        ["keys", id, "revoke"] => back(control::revoke_api_key(&s.tenant, id).map(|_| "Key revoked".to_string())),
        ["accounts", id, "disconnect"] => back(
            in_tenant(&s.tenant, || tool("disconnect_account", json!({ "account_id": id })).map_err(Error::Msg))
                .map(|v| format!("Disconnected {}", js_string(&v["disconnected"]))),
        ),
        ["connect", platform] => {
            let Ok(p) = platforms::get(platform) else {
                return back(Err(Error::Msg("Unknown platform".into())));
            };
            if p.oauth_env().is_some() {
                match in_tenant(&s.tenant, || crate::oauth::start_oauth(platform)) {
                    Ok(url) => redirect(&url, None),
                    Err(e) => back(Err(e)),
                }
            } else {
                let mut creds = Map::new();
                for (k, v) in &f {
                    if let Some(name) = k.strip_prefix("cred_") {
                        creds.insert(name.to_string(), v.clone());
                    }
                }
                back(
                    in_tenant(&s.tenant, || {
                        tool("connect_account", json!({ "platform": platform, "credentials": creds })).map_err(Error::Msg)
                    })
                    .map(|v| {
                        let n = v["connected"].as_array().map(|a| a.len()).unwrap_or(0);
                        format!("Connected {n} {} account(s)", p.name)
                    }),
                )
            }
        }
        ["apps", prefix] if app_prefixes().iter().any(|(p, _)| p == prefix) => {
            let mut v = Map::new();
            for k in ["client_id", "client_secret", "config_id", "webhook_verify_token"] {
                let val = fstr(&f, k).trim().to_string();
                if !val.is_empty() {
                    v.insert(k.into(), Value::from(val));
                }
            }
            if !v.contains_key("client_id") {
                return back(Err(Error::Msg("Client id is required".into())));
            }
            back(in_tenant(&s.tenant, || tenant::save_byo_creds(prefix, &Value::Object(v))).map(|_| format!("Saved your {prefix} app")))
        }
        ["apps", prefix, "delete"] => back(in_tenant(&s.tenant, || tenant::delete_byo_creds(prefix)).map(|_| format!("Removed your {prefix} app"))),
        ["billing", "polar", plan] => match control::plan(plan).ok().flatten() {
            Some(p) => match billing::polar_checkout(&s.tenant, &p, s.email.as_deref()) {
                Ok(url) => redirect(&url, None),
                Err(e) => back(Err(e)),
            },
            None => back(Err(Error::Msg("Unknown plan".into()))),
        },
        ["billing", "doku", plan, months] => match (control::plan(plan).ok().flatten(), months.parse::<i64>()) {
            (Some(p), Ok(m)) => {
                let method = fstr(&f, "method");
                if method.is_empty() {
                    return doku_choose(&s, &p, m, Some("Choose a payment method"));
                }
                if fstr(&f, "confirm") != "1" {
                    return doku_confirm(&s, &p, m, &method);
                }
                match billing::doku_checkout(&s.tenant, &p, m, &method, s.email.as_deref()) {
                    Ok(url) => redirect(&url, None),
                    Err(e) => back(Err(e)),
                }
            }
            _ => back(Err(Error::Msg("Unknown plan".into()))),
        },
        ["delete"] => {
            if fstr(&f, "confirm") != "DELETE" {
                return back(Err(Error::Msg("Type DELETE to confirm".into())));
            }
            match delete_account(&s.user_id, &s.tenant) {
                Ok(()) => redirect(
                    "/?deleted=1",
                    Some(format!("siar_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", secure())),
                ),
                Err(e) => back(Err(e)),
            }
        }
        _ => text("Not found", 404),
    }
}

/// Wipe a workspace: R2 objects of its media, its database and media directory, and its control
/// rows (keys, subscription, usage, app credentials), then the user and their sessions.
pub fn delete_account(user_id: &str, tenant_id: &str) -> Result<()> {
    // A card subscription is revoked at once (no refund); on failure the deletion goes on and the
    // revocation is retried by the scheduler.
    if let Some(sub) = control::subscription(tenant_id)? {
        if sub.provider == "polar" && sub.status != "revoked" {
            let ext: Option<String> = control::with(|c| {
                Ok(
                    c.query_row("SELECT external_id FROM subscriptions WHERE tenant_id = ?", [tenant_id], |r| r.get(0))
                        .optional()?
                        .flatten(),
                )
            })?;
            if let Some(id) = ext.filter(|i| !i.is_empty()) {
                billing::polar_revoke_or_queue(&id, tenant_id);
            }
        }
    }
    let _ = in_tenant(tenant_id, || {
        let ids: Vec<String> = crate::db::with(|c| crate::db::rows(c, "SELECT id FROM media", [], |r| r.get(0)))?;
        for id in ids {
            let _ = crate::media::delete_media(&id);
        }
        Ok(())
    });
    tenant::forget(tenant_id);
    let db = tenant::db_path(tenant_id);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let _ = std::fs::remove_dir_all(config().data_dir.join("tenants").join(tenant_id));
    control::with(|c| {
        c.execute("DELETE FROM tenants WHERE id = ?", [tenant_id])?;
        c.execute("DELETE FROM users WHERE id = ?", [user_id])?;
        Ok(())
    })
}

// ---------- public pages ----------

fn signed_in(req: &Request) -> bool {
    session(req).ok().flatten().is_some()
}

fn landing(req: &Request) -> Resp {
    let signed = signed_in(req);
    let mcp = format!("{}/mcp", public());
    let (start_href, start_label) = if signed {
        ("/dashboard", "Open your dashboard")
    } else {
        ("/login", "Create a free account")
    };
    let plans = control::plans().unwrap_or_default();
    let body = tpl(
        LANDING,
        &[
            ("start_href", start_href),
            ("start_label", start_label),
            ("setup", setup_tabs(&mcp, "YOUR_SIAR_KEY").as_str()),
            ("prompt", esc(PROMPT).as_str()),
            ("platforms", platform_list().as_str()),
            ("price_line", price_line(&plans).as_str()),
            ("hosted_price", hosted_price(&plans).as_str()),
        ],
    );
    page("siar: the social media scheduler your AI agent runs", &body, signed)
}

/// The per-agent setup tabs (rust/web/setup.html) for an endpoint and a key or placeholder.
fn setup_tabs(mcp_url: &str, key: &str) -> String {
    tpl(SETUP, &[("mcp_url", esc(mcp_url).as_str()), ("key", esc(key).as_str())])
}

/// Platform names from platforms.json, grouped by how they connect; variants share one name.
fn platform_list() -> String {
    let (mut oauth, mut creds): (Vec<&str>, Vec<&str>) = (vec![], vec![]);
    for p in platforms::all() {
        let name = p.name.split(" (").next().unwrap_or(&p.name);
        let v = if p.oauth_env().is_some() { &mut oauth } else { &mut creds };
        if !v.contains(&name) {
            v.push(name);
        }
    }
    let li = |v: &[&str]| v.iter().map(|n| format!("<li>{}</li>", esc(n))).collect::<String>();
    format!(
        "<h3 class=\"plist-h\">Sign in with the platform</h3><ul class=\"plist\">{}</ul><h3 class=\"plist-h\">Connect with a token or password</h3><ul class=\"plist\">{}</ul>",
        li(oauth.as_slice()),
        li(creds.as_slice())
    )
}

fn plan_limits(p: &control::Plan) -> String {
    format!(
        "<ul class=\"limits\"><li><b>{}</b> connected accounts</li><li><b>{}</b> scheduled or published posts per month</li><li><b>{}</b> media storage, files up to {}</li><li><b>{}</b> API requests per minute per key</li></ul>",
        p.max_accounts,
        p.max_posts_month,
        mb(p.max_storage_mb),
        mb(p.max_upload_mb),
        p.api_per_minute
    )
}

/// Megabytes as "200 MB" or, for whole gigabytes, "2 GB".
fn mb(n: i64) -> String {
    if n >= 1024 && n % 1024 == 0 {
        format!("{} GB", n / 1024)
    } else {
        format!("{n} MB")
    }
}

fn same_limits(a: &control::Plan, b: &control::Plan) -> bool {
    (a.max_accounts, a.max_posts_month, a.max_storage_mb, a.max_upload_mb, a.api_per_minute)
        == (b.max_accounts, b.max_posts_month, b.max_storage_mb, b.max_upload_mb, b.api_per_minute)
}

fn per(interval: &str) -> String {
    match interval {
        "month" => "a month".into(),
        "year" => "a year".into(),
        i => format!("per {}", esc(i)),
    }
}

/// "Free for 2 accounts and 30 posts a month. 10 accounts and 300 posts a month for $3.75 a month or $13.37 a year."
fn price_line(plans: &[control::Plan]) -> String {
    let mut out = String::new();
    if let Some(f) = plans.iter().find(|p| p.public && p.price_usd_cents == 0) {
        out.push_str(&format!(
            "<strong>Free</strong> for {} accounts and {} posts a month.",
            f.max_accounts, f.max_posts_month
        ));
    }
    let paid: Vec<&control::Plan> = plans.iter().filter(|p| p.public && p.price_usd_cents > 0).collect();
    if let Some(first) = paid.first() {
        let prices: Vec<String> = paid
            .iter()
            .filter(|p| p.max_accounts == first.max_accounts && p.max_posts_month == first.max_posts_month)
            .map(|p| format!("<strong>{}</strong> {}", usd(billing::polar_price(p).total), per(&p.interval)))
            .collect();
        out.push_str(&format!(
            " {} accounts and {} posts a month for {}, card fee included.",
            first.max_accounts,
            first.max_posts_month,
            prices.join(" or ")
        ));
    }
    out
}

fn hosted_price(plans: &[control::Plan]) -> String {
    let free = plans.iter().any(|p| p.public && p.price_usd_cents == 0);
    let cheapest = plans
        .iter()
        .filter(|p| p.public && p.price_usd_cents > 0 && p.interval == "month")
        .map(|p| billing::polar_price(p).total)
        .min();
    match (free, cheapest) {
        (true, Some(c)) => format!("Free plan, then {} a month", usd(c)),
        (false, Some(c)) => format!("From {} a month", usd(c)),
        (true, None) => "Free plan".into(),
        (false, None) => "See pricing".into(),
    }
}

/// Every MCP tool with the first sentence of its description.
fn tool_table() -> String {
    let mut rows = String::new();
    for t in crate::mcp::tools().as_array().into_iter().flatten() {
        let d = js_string(&t["description"]);
        let first = match d.find(". ") {
            Some(i) => &d[..=i],
            None => d.as_str(),
        };
        rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td></tr>",
            esc(&js_string(&t["name"])),
            esc(first)
        ));
    }
    format!(
        "<div class=\"table-scroll\"><table class=\"data tools\"><thead><tr><th>Tool</th><th>What it does</th></tr></thead><tbody>{rows}</tbody></table></div>"
    )
}

fn docs(req: &Request) -> Resp {
    let url = format!("{}/mcp", public());
    let body = tpl(
        DOCS,
        &[
            ("mcp_url", esc(&url).as_str()),
            ("setup", setup_tabs(&url, "YOUR_SIAR_KEY").as_str()),
            ("prompt", esc(PROMPT).as_str()),
            ("tools", tool_table().as_str()),
        ],
    );
    page("Docs", &body, signed_in(req))
}

fn pricing(req: &Request) -> Resp {
    let signed = signed_in(req);
    let fees = billing::polar_fees();
    let plans: Vec<control::Plan> = control::plans().unwrap_or_default().into_iter().filter(|p| p.public).collect();
    let mut cards = String::new();
    let mut fee_tables = String::new();

    // Free plans: one card each.
    for p in plans.iter().filter(|p| p.price_usd_cents == 0) {
        let (href, label) = if signed {
            ("/dashboard", "Open your dashboard")
        } else {
            ("/login", "Create a free account")
        };
        cards.push_str(&format!(
            "<div class=\"plan\"><h2>{}</h2><p class=\"desc\">{}</p><p class=\"amount\">$0</p><p class=\"fee\">No card needed. Sign in with GitHub or Google.</p>{}<div class=\"actions\"><a class=\"btn\" href=\"{href}\">{label}</a></div></div>",
            esc(&p.name),
            esc(&p.description),
            plan_limits(p)
        ));
    }

    // Paid plans with the same limits share a card, one option per billing period.
    let paid: Vec<&control::Plan> = plans.iter().filter(|p| p.price_usd_cents > 0).collect();
    let mut done: Vec<String> = vec![];
    for p in &paid {
        if done.contains(&p.id) {
            continue;
        }
        let group: Vec<&control::Plan> = paid.iter().copied().filter(|q| same_limits(p, q)).collect();
        done.extend(group.iter().map(|q| q.id.clone()));
        let monthly = group.iter().find(|q| q.interval == "month").map(|q| billing::polar_price(q).total);
        let mut options = String::new();
        for q in &group {
            let price = billing::polar_price(q);
            let saving = match (q.interval.as_str(), monthly) {
                ("year", Some(m)) if m * 12 > price.total => format!(
                    "<span class=\"saving\">{} less than 12 monthly payments</span><p class=\"fee\">Works out to {} a month.</p>",
                    usd(m * 12 - price.total),
                    usd(price.total / 12)
                ),
                _ => String::new(),
            };
            let periods = control::plan_prices(&q.id, "doku").unwrap_or_default();
            let idr_line: Vec<String> = periods
                .iter()
                .map(|(m, a, _)| format!("{} for {m} month{}", idr(*a), if *m == 1 { "" } else { "s" }))
                .collect();
            options.push_str(&format!(
                "<div class=\"option\">{}<p class=\"amount\">{} <small>{}</small></p><p class=\"fee\">{} + {} card processing fee</p>{saving}{}</div>",
                if group.len() > 1 {
                    format!("<h3>{}</h3>", esc(&q.name))
                } else {
                    String::new()
                },
                usd(price.total),
                per(&q.interval),
                usd(price.net),
                usd(price.fee),
                if idr_line.is_empty() {
                    String::new()
                } else {
                    format!("<p class=\"fee\">Or prepaid in rupiah: {} + admin fee.</p>", esc(&idr_line.join(", ")))
                }
            ));
            if let Some((m, a, _)) = periods.first() {
                let mut rows = String::new();
                for method in billing::doku_methods().unwrap_or_default() {
                    if let Ok(qt) = billing::doku_quote(q, *m, &method.method) {
                        rows.push_str(&format!(
                            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
                            esc(&method.label),
                            idr(qt.fee),
                            idr(qt.total)
                        ));
                    }
                }
                fee_tables.push_str(&format!(
                    "<h3>{}, {} month{} ({})</h3><div class=\"table-scroll\"><table class=\"data\"><thead><tr><th>Payment method</th><th class=\"num\">Biaya admin</th><th class=\"num\">Total</th></tr></thead><tbody>{rows}</tbody></table></div>",
                    esc(&q.name),
                    m,
                    if *m == 1 { "" } else { "s" },
                    idr(*a)
                ));
            }
        }
        let title: Vec<&str> = group.iter().map(|q| q.name.as_str()).collect();
        let (href, label) = if signed {
            ("/dashboard#billing", "Upgrade in your dashboard")
        } else {
            ("/login", "Start free, upgrade any time")
        };
        cards.push_str(&format!(
            "<div class=\"plan paid\"><h2>{}</h2><p class=\"desc\">{}</p>{options}{}<div class=\"actions\"><a class=\"btn\" href=\"{href}\">{label}</a></div></div>",
            esc(&title.join(" or ")),
            if group.len() > 1 { "The same plan, paid each month or once a year.".to_string() } else { esc(&p.description) },
            plan_limits(p)
        ));
    }

    cards.push_str(&format!(
        "<div class=\"plan selfhost\"><h2>Self-hosted</h2><p class=\"desc\">Run the same binary on your own server.</p><p class=\"amount\">$0</p><p class=\"fee\">AGPL-3.0. You run the server and register your own platform apps.</p>\
<ul class=\"limits\"><li>No siar plan limits; each platform's own API limits still apply</li><li>Same MCP tools as the hosted service</li><li>Your data stays on your disk</li></ul><div class=\"actions\"><a class=\"btn ghost\" href=\"{GITHUB}\">Read the source on GitHub</a></div></div>"
    ));

    let doku = if fee_tables.is_empty() {
        String::new()
    } else {
        format!("<div><h2>Paying in rupiah</h2><p class=\"muted\">You choose the payment method before paying; its admin fee (Biaya admin, set by DOKU, VAT included) is added as a separate line:</p>{fee_tables}</div>")
    };
    page(
        "Pricing",
        &format!(
            "<div class=\"wrap\"><div class=\"page-head\"><h1>Pricing</h1><p>Start free. Pay in USD by card through Polar, or in rupiah with QRIS, virtual accounts or e-wallets through DOKU. You pay the payment provider's fee, shown as its own line; the plan price is what we receive.</p></div>\
<div class=\"plans\">{cards}</div>\
<section class=\"band fees-band\" aria-labelledby=\"fees-title\"><div class=\"fees-wrap\"><div><h2 id=\"fees-title\">Fees, line by line</h2>\
<p class=\"muted\">Card prices include Polar's fee ({}% + {}{}), passed on at cost. Polar may add sales tax or VAT for your country at checkout. Cancel any time; a plan stays active until the end of the period you paid for.</p>\
<dl class=\"qa\"><dt>Do I need a card for the free plan?</dt><dd>No. Sign in with GitHub or Google and the free plan starts right away.</dd>\
<dt>What happens when I reach a limit?</dt><dd>The tool your agent called returns an error that names the limit and how to raise it. Nothing else changes.</dd>\
<dt>Why do some networks need my own developer app?</dt><dd>Instagram, Threads, Facebook, LinkedIn, YouTube and the other sign-in networks only let approved apps post. Until siar's own apps are approved, you register an app and paste its id and secret; the dashboard gives you the redirect URI. This works on every plan.</dd>\
<dt>Do rupiah payments renew?</dt><dd>No. They buy a prepaid period, and we e-mail you 7 days and 1 day before it ends.</dd></dl></div>{doku}</div></section></div>",
            fees.percent,
            usd(fees.fixed_cents),
            if fees.extra_percent > 0.0 { format!(", +{}% for cards outside the US", fees.extra_percent) } else { String::new() }
        ),
        signed,
    )
}

fn privacy(req: &Request) -> Resp {
    let op = esc(&operator());
    let loc = esc(&location());
    let c = esc(&contact());
    let body = format!(
        "<div class=\"wrap prose\"><h1>Privacy policy</h1><p>Last updated: 17 November 2026.</p>\
<p>siar (\"the service\") is a hosted social media scheduler operated by {op}, {loc} (\"we\"). Customers sign in, connect their own social media accounts, and use AI agents or the dashboard to schedule and publish posts, read comments and messages, and see statistics. This policy explains what we do with the data.</p>\
<h2>Data we process</h2><ul>\
<li><strong>Your siar account:</strong> name and email from GitHub or Google sign-in, session records, API keys (stored only as hashes), plan, payment status and usage counts.</li>\
<li><strong>Connected accounts:</strong> OAuth access and refresh tokens or credentials, encrypted (AES-256-GCM) with a key that belongs to your workspace; account or channel id, name, handle.</li>\
<li><strong>Content:</strong> the posts, videos and media you create through siar, scheduled times, and the ids and links platforms return.</li>\
<li><strong>Platform data:</strong> statistics for your own posts and accounts, comments on your posts, and direct messages sent to your accounts, so you can read and answer them.</li>\
<li><strong>Payments:</strong> handled by Polar (Polar Software Inc., merchant of record) or DOKU (PT Nusa Satu Inti Artha). We receive the payment status and subscription details, not your card or bank details.</li></ul>\
<h2>How we use it</h2><p>Only to provide the service to you: publish and schedule your content, show your comments, messages and statistics, enforce your plan, and keep the service secure. We do not sell data, use it for advertising, or build profiles of people who interact with your accounts. We do not use platform data to train AI models.</p>\
<h2>YouTube API Services</h2><p>siar uses YouTube API Services to upload videos, set their details, read your channel's videos, comments and statistics, and reply to comments. By connecting a YouTube channel you agree to the <a href=\"https://www.youtube.com/t/terms\">YouTube Terms of Service</a>; Google's handling of data is described in the <a href=\"https://policies.google.com/privacy\">Google Privacy Policy</a>. You can revoke access on the <a href=\"https://security.google.com/settings/security/permissions\">Google security settings page</a>. Data obtained from YouTube is refreshed or deleted within 30 days.</p>\
<h2>Google user data</h2><p>siar's use and transfer of information received from Google APIs adheres to the <a href=\"https://developers.google.com/terms/api-services-user-data-policy\">Google API Services User Data Policy</a>, including the Limited Use requirements. Google user data is used only to provide the features you use, is not transferred to others except as needed to provide those features, for security, or to comply with law, and is not read by humans unless you ask us to for support, for security, or as required by law.</p>\
<h2>Meta (Instagram, Facebook, Threads)</h2><p>Data from Meta platforms is used only to publish for your accounts, show your insights, comments and messages, and run the automations you set up. You can remove siar's access in Meta's Business Integrations settings, and ask for deletion of the data as described below; Meta data deletion requests are completed within 30 days.</p>\
<h2>LinkedIn</h2><p>LinkedIn data (your profile or Company Page id and name, post statistics) is used only to publish for you and show your results, in line with the LinkedIn API Terms. Remove access under LinkedIn's Permitted Services.</p>\
<h2>Sharing and processors</h2><p>Content goes only to the platforms you publish to. Processors: our server host, Cloudflare (network and media storage), Polar and DOKU (payments), GitHub and Google (sign-in). Each processes data only to provide its part of the service.</p>\
<h2>Retention and deletion</h2><p>Data is kept while your account exists. Disconnecting an account deletes its tokens at once. Deleting your siar account in the dashboard deletes your workspace (connected accounts, tokens, posts, media, API keys) immediately; remaining copies in backups and logs are deleted within 30 days. Payment records are kept as long as tax law requires. To ask for deletion or a copy of your data, email <a href=\"mailto:{c}\">{c}</a>.</p>\
<h2>Security</h2><p>Encryption in transit (HTTPS) and at rest for tokens, per-workspace encryption keys, hashed API keys and session tokens, and access limited to the operator.</p>\
<h2>Your rights</h2><p>You may access, correct, export or delete your personal data, and object to processing, under Indonesia's Personal Data Protection Law (UU PDP) and other laws that apply to you.</p>\
<h2>Cookies</h2><p>One necessary cookie keeps you signed in. No tracking or advertising cookies.</p>\
<h2>Contact</h2><p>{op}, {loc}. Email: <a href=\"mailto:{c}\">{c}</a>.</p></div>"
    );
    page("Privacy policy", &body, signed_in(req))
}

fn terms(req: &Request) -> Resp {
    let op = esc(&operator());
    let loc = esc(&location());
    let c = esc(&contact());
    let body = format!(
        "<div class=\"wrap prose\"><h1>Terms of service</h1><p>Last updated: 17 November 2026.</p>\
<p>These terms are an agreement between you and {op}, {loc}, the operator of siar.</p>\
<h2>The service</h2><p>siar lets you schedule and publish content to social media accounts you connect, and read comments, messages and statistics for them, through AI agents (MCP) or the dashboard.</p>\
<h2>Your accounts and content</h2><p>You may only connect accounts you are authorized to manage. You are responsible for the content you publish and for following each platform's terms, including the <a href=\"https://www.youtube.com/t/terms\">YouTube Terms of Service</a>, Meta's terms and community standards, LinkedIn's User Agreement and the rules of other platforms. You keep all rights to your content; you give us permission to process it only to provide the service.</p>\
<h2>Acceptable use</h2><p>No spam, deceptive or illegal content, harassment, attempts to break or overload the service, or use of automations that violate platform rules. We may suspend accounts that do.</p>\
<h2>Plans and payment</h2><p>Plans have limits shown on the <a href=\"/pricing\">pricing page</a>. USD subscriptions are sold by Polar as merchant of record and renew until cancelled; rupiah payments through DOKU buy a prepaid period and do not renew automatically. Prices shown include the payment provider's fee, which you pay: Polar's card processing fee, or DOKU's admin fee (Biaya admin) for the method you choose. A plan starts when the payment provider confirms payment. Refunds follow the provider's policy and applicable law.</p>\
<h2>Availability</h2><p>We aim for a reliable service but platforms change their APIs and may reject or delay posts; the service is provided as is, without guarantees that every post is published at the exact time.</p>\
<h2>Liability</h2><p>To the extent the law allows, our liability is limited to the amount you paid in the 3 months before the claim.</p>\
<h2>Ending</h2><p>You can delete your account at any time in the dashboard. Deleting it cancels a card subscription (Polar) immediately, without a refund for the rest of the period; a prepaid rupiah period ends with the account without a refund. Data deletion is described in the <a href=\"/privacy\">privacy policy</a>.</p>\
<h2>Law</h2><p>These terms are governed by the laws of the Republic of Indonesia.</p>\
<h2>Contact</h2><p><a href=\"mailto:{c}\">{c}</a></p></div>"
    );
    page("Terms of service", &body, signed_in(req))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_emails_link() {
        let gh = json!([
            { "email": "old@example.com", "primary": false, "verified": true },
            { "email": "me@example.com", "primary": true, "verified": true },
        ]);
        assert_eq!(github_verified_email(&gh).as_deref(), Some("me@example.com"));
        let gh_unverified =
            json!([{ "email": "me@example.com", "primary": true, "verified": false }, { "email": "x@example.com", "primary": false, "verified": true }]);
        assert_eq!(github_verified_email(&gh_unverified), None, "a verified non-primary address is not used");
        assert_eq!(github_verified_email(&Value::Null), None);
        assert_eq!(
            google_verified_email(&json!({ "sub": "1", "email": "me@example.com", "email_verified": true })).as_deref(),
            Some("me@example.com")
        );
        assert_eq!(
            google_verified_email(&json!({ "sub": "1", "email": "me@example.com", "email_verified": false })),
            None
        );
        assert_eq!(google_verified_email(&json!({ "sub": "1", "email": "me@example.com" })), None);
        assert_eq!(
            google_verified_email(&json!({ "sub": "1", "email": "me@example.com", "email_verified": "true" })),
            None,
            "only a real boolean counts"
        );
    }
}
