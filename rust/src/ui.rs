//! Read-only content calendar at /app: one static HTML page plus one JSON endpoint.
//! Sign-in takes SIAR_API_TOKEN once and leaves a cookie derived from it, so rotating the token
//! signs everyone out.

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::Read;
use tiny_http::{Method, Request, Response};

use crate::config::config;
use crate::server::{header, json_resp, query_param, Resp, ICON_LINKS};

const COOKIE: &str = "siar_ui";
const MAX_AGE: i64 = 60 * 60 * 24 * 30;
const CALENDAR: &str = include_str!("../web/calendar.html");
const CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; img-src * data: blob:; media-src *; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
const RANGE_ERROR: &str = "give from and to as ISO dates, at most 100 days apart";

fn session_value() -> String {
    let key = config().api_token.clone().unwrap_or_default();
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("hmac takes any key length");
    mac.update(b"siar-ui-session-v1");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn same(a: &str, b: &str) -> bool {
    !b.is_empty() && a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn signed_in(req: &Request) -> bool {
    let raw = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("cookie"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let prefix = format!("{COOKIE}=");
    let got = raw.split(';').map(str::trim).find_map(|c| c.strip_prefix(&prefix)).unwrap_or("");
    same(got, &session_value())
}

fn cookie(value: &str, max_age: i64) -> String {
    let secure = if config().public_url.starts_with("https://") { "; Secure" } else { "" };
    format!("{COOKIE}={value}; Path=/app; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
}

fn html(body: String, status: u16) -> Resp {
    Response::from_string(body)
        .with_chunked_threshold(usize::MAX)
        .with_status_code(status)
        .with_header(header("content-type", "text/html; charset=utf-8"))
        .with_header(header("content-security-policy", CSP))
        .with_header(header("referrer-policy", "no-referrer"))
        .with_header(header("cache-control", "no-store"))
}

fn redirect(cookie_value: String) -> Resp {
    Response::from_data(vec![])
        .with_status_code(303)
        .with_header(header("location", "/app"))
        .with_header(header("set-cookie", &cookie_value))
}

fn login(error: &str) -> String {
    let err = if error.is_empty() {
        String::new()
    } else {
        format!("<p class=\"err\">{error}</p>")
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>Sign in · siar</title>{ICON_LINKS}<meta name=\"color-scheme\" content=\"light dark\"><style>
body{{font:15px/1.5 system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0;background:Canvas;color:CanvasText}}
form{{display:grid;gap:.75rem;width:min(22rem,90vw)}}h1{{font-size:1.3rem;margin:0}}p{{margin:0;opacity:.7;font-size:.9rem}}
input,button{{font:inherit;padding:.6rem .75rem;border-radius:8px;border:1px solid #8886}}button{{background:#0b5cad;color:#fff;border:0;cursor:pointer}}
.err{{color:#c62828;opacity:1}}.logo{{width:3rem;height:3rem}}</style></head><body><form method=\"post\" action=\"/app/login\">
<img class=\"logo\" src=\"/icon.svg\" alt=\"\"><h1>siar calendar</h1><p>Paste this instance's <code>SIAR_API_TOKEN</code>.</p>{err}
<input type=\"password\" name=\"token\" autocomplete=\"current-password\" required autofocus aria-label=\"API token\"><button>Sign in</button></form></body></html>"
    )
}

/// Everything under /app.
pub fn handle(req: &mut Request, path: &str, query: &str, method: &Method) -> Resp {
    if path == "/app/login" && *method == Method::Post {
        let mut body = String::new();
        let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
        let token = query_param(&body, "token").unwrap_or_default();
        let want = config().api_token.clone().unwrap_or_default();
        if !same(token.trim(), &want) {
            std::thread::sleep(std::time::Duration::from_millis(800));
            return html(login("That token does not match."), 401);
        }
        return redirect(cookie(&session_value(), MAX_AGE));
    }
    if path == "/app/logout" {
        return redirect(cookie("", 0));
    }
    if !signed_in(req) {
        return if path == "/app/api/posts" {
            json_resp(&serde_json::json!({ "error": "signed out" }), 401)
        } else {
            html(login(""), 200)
        };
    }
    match path {
        "/app" | "/app/" => html(CALENDAR.to_string(), 200),
        "/app/api/posts" if *method == Method::Get => {
            let from = query_param(query, "from").and_then(|v| crate::util::parse_iso(&v));
            let to = query_param(query, "to").and_then(|v| crate::util::parse_iso(&v));
            let (Some(from), Some(to)) = (from, to) else {
                return json_resp(&serde_json::json!({ "error": RANGE_ERROR }), 400);
            };
            if to <= from || to - from > 100 * 86_400_000 {
                return json_resp(&serde_json::json!({ "error": RANGE_ERROR }), 400);
            }
            match crate::posts::calendar_data(from, to) {
                Ok(v) => json_resp(&v, 200).with_header(header("cache-control", "no-store")),
                Err(e) => json_resp(&serde_json::json!({ "error": e.to_string() }), 500),
            }
        }
        _ => Response::from_string("Not found").with_status_code(404),
    }
}
