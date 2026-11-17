//! HTTP server (src/http.ts): health, legal pages, /mcp with a bearer token, OAuth callbacks
//! and local media. One thread per request, like Bun's async handlers never blocking each other.

use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::config::config;
use crate::mcp::handle_batch;
use crate::media::{local_media_path, mime_for_ext};
use crate::oauth::finish_oauth;
use crate::util::{bail, Result};

fn page_html(title: &str, body: &str) -> String {
    let c = config();
    let (op, contact) = (&c.operator, &c.contact);
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>{title} · siar</title><style>body{{font:16px/1.6 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#1a1a1a}}h1{{font-size:1.6rem}}h2{{font-size:1.15rem;margin-top:1.8rem}}a{{color:#0b5cad}}footer{{margin-top:3rem;font-size:.9rem;color:#555}}</style></head>
<body>{body}<footer>siar is operated by {op}. Contact: <a href=\"mailto:{contact}\">{contact}</a> · <a href=\"/privacy\">Privacy policy</a> · <a href=\"/terms\">Terms of use</a></footer></body></html>"
    )
}

const HOME: &str = "<h1>siar</h1>
<p>siar is a self-hosted social media scheduler. Its owner and their AI agents use it to schedule and publish posts, read comments and messages, and see post statistics for the owner's own accounts on Instagram, Threads, YouTube, LinkedIn and other platforms.</p>
<p>It has no public sign-up: only accounts connected by the owner are used. It is controlled through the Model Context Protocol (MCP) by the owner's authorized clients.</p>";

fn privacy() -> String {
    let c = config();
    let (op, contact) = (&c.operator, &c.contact);
    let loc = if c.operator_location.is_empty() {
        String::new()
    } else {
        format!(", {}", c.operator_location)
    };
    format!(
        "<h1>Privacy policy</h1>
<p>Last updated: 16 November 2026.</p>
<p>siar (\"the app\") is a social media scheduler operated by {op} for its own and its owner's accounts. This policy explains what the app does with data from the platforms it connects to.</p>

<h2>YouTube API Services</h2>
<p>siar uses YouTube API Services to upload videos, set their titles, descriptions, thumbnails and privacy settings, read the connected channel's videos, comments and statistics, and reply to comments. By connecting a YouTube channel to siar you agree to the <a href=\"https://www.youtube.com/t/terms\">YouTube Terms of Service</a>. Google's handling of your data is described in the <a href=\"https://policies.google.com/privacy\">Google Privacy Policy</a>.</p>

<h2>What data siar accesses and stores</h2>
<ul>
<li>OAuth access and refresh tokens for each connected account, stored encrypted (AES-256-GCM) on the operator's server.</li>
<li>Basic account details: account or channel id, name, handle and profile picture URL.</li>
<li>The posts and videos created through siar, their media files, scheduled times, and the ids and links the platform returns after publishing.</li>
<li>Statistics for the account's own posts and videos (for example views, likes, comments and followers), stored as snapshots so trends can be compared.</li>
<li>Comments on the account's own posts and direct messages sent to the account, so the owner can read and reply to them.</li>
</ul>

<h2>How the data is used</h2>
<p>Only to publish and schedule the owner's content, reply to comments and messages on the owner's behalf, and show the owner how their posts perform. siar does not use the data for advertising, does not build profiles of other users, and does not sell data.</p>

<h2>Sharing</h2>
<p>Data is not shared with third parties. Content is sent only to the platform it is published on (for example YouTube for videos uploaded to a YouTube channel). Media files are staged in the operator's own Cloudflare R2 storage so platforms can fetch them.</p>

<h2>Cookies</h2>
<p>siar has no web interface for end users and stores no cookies on your device.</p>

<h2>Retention and deletion</h2>
<p>Tokens are kept while an account is connected and deleted when it is disconnected. Statistics refreshed from YouTube are updated or deleted within 30 days. To have any stored data deleted, email <a href=\"mailto:{contact}\">{contact}</a>; requests are handled within 7 days.</p>

<h2>Revoking access</h2>
<p>You can revoke siar's access to your Google account at any time on the <a href=\"https://security.google.com/settings/security/permissions\">Google security settings page</a>. Access for other platforms can be removed in their settings (for example Meta's Business Integrations or LinkedIn's Permitted Services), or by asking the operator to disconnect the account.</p>

<h2>Contact</h2>
<p>{op}{loc}. Email: <a href=\"mailto:{contact}\">{contact}</a>.</p>"
    )
}

fn terms() -> String {
    let c = config();
    let (op, contact) = (&c.operator, &c.contact);
    format!(
        "<h1>Terms of use</h1>
<p>Last updated: 16 November 2026.</p>
<p>siar is a private tool operated by {op} for publishing to accounts it controls. There is no public sign-up.</p>
<p>By connecting an account you confirm you are authorized to manage it, and you agree to the terms of each platform you connect, including the <a href=\"https://www.youtube.com/t/terms\">YouTube Terms of Service</a> for YouTube channels.</p>
<p>The data siar handles is described in the <a href=\"/privacy\">privacy policy</a>. Access can be revoked at any time, for Google accounts on the <a href=\"https://security.google.com/settings/security/permissions\">Google security settings page</a>.</p>
<p>siar is provided as is. Questions: <a href=\"mailto:{contact}\">{contact}</a>.</p>"
    )
}

/// A response header; bytes a header cannot carry (non-ASCII, control characters) are percent-encoded.
fn header(k: &str, v: &str) -> Header {
    let safe: String = v
        .bytes()
        .map(|b| {
            if (0x20..0x7f).contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Header::from_bytes(k.as_bytes(), safe.as_bytes()).unwrap_or_else(|_| Header::from_bytes(&b"x-invalid"[..], &b"1"[..]).expect("static header"))
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn text(body: &str, status: u16) -> Resp {
    Response::from_string(format!("{body}\n"))
        .with_chunked_threshold(usize::MAX)
        .with_status_code(status)
        .with_header(header("content-type", "text/plain; charset=utf-8"))
}

fn html(title: &str, body: &str) -> Resp {
    Response::from_string(page_html(title, body))
        .with_chunked_threshold(usize::MAX)
        .with_header(header("content-type", "text/html; charset=utf-8"))
}

fn json_resp(v: &Value, status: u16) -> Resp {
    // One write with a Content-Length instead of 8 KB chunks.
    Response::from_data(serde_json::to_vec(v).unwrap_or_default())
        .with_chunked_threshold(usize::MAX)
        .with_status_code(status)
        .with_header(header("content-type", "application/json;charset=utf-8"))
}

fn empty(status: u16) -> Resp {
    Response::from_data(vec![]).with_status_code(status)
}

/// Constant-time comparison of the bearer token.
fn authorized(req: &Request) -> bool {
    let got = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("authorization"))
        .map(|h| {
            let v = h.value.as_str();
            // replace(/^Bearer\s+/i, "")
            if v.len() >= 6 && v[..6].eq_ignore_ascii_case("bearer") && v[6..].starts_with(|c: char| c.is_whitespace()) {
                v[6..].trim_start().to_string()
            } else {
                v.to_string()
            }
        })
        .unwrap_or_default();
    let want = config().api_token.clone().unwrap_or_default();
    if got.len() != want.len() || want.is_empty() {
        return false;
    }
    got.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if decode(k) == key {
            return Some(decode(v));
        }
    }
    None
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `[\w-]{6,16}`
fn is_link_code(s: &str) -> bool {
    (6..=16).contains(&s.len()) && s.chars().all(|c| is_word(c) || c == '-')
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn route(mut req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let query = query.to_string();
    let method = req.method().clone();

    let resp: Resp = match path.as_str() {
        "/health" => text("ok", 200),
        "/" => html("siar", HOME),
        "/privacy" => html("Privacy policy", &privacy()),
        "/terms" => html("Terms of use", &terms()),
        "/mcp" => {
            if !authorized(&req) {
                Response::from_string("Unauthorized")
                    .with_status_code(401)
                    .with_header(header("content-type", "text/plain;charset=utf-8"))
                    .with_header(header("www-authenticate", "Bearer"))
            } else if method == Method::Get {
                Response::from_string("SSE stream not offered; POST JSON-RPC here")
                    .with_status_code(405)
                    .with_header(header("content-type", "text/plain;charset=utf-8"))
                    .with_header(header("allow", "POST"))
            } else if method == Method::Delete {
                empty(204)
            } else if method != Method::Post {
                empty(405)
            } else {
                let t0 = std::time::Instant::now();
                let mut body = String::new();
                let parsed = req
                    .as_reader()
                    .read_to_string(&mut body)
                    .ok()
                    .and_then(|_| serde_json::from_str::<Value>(&body).ok());
                match parsed {
                    None => json_resp(
                        &serde_json::json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } }),
                        400,
                    ),
                    Some(v) => {
                        let t1 = t0.elapsed();
                        let out = handle_batch(&v);
                        let t2 = t0.elapsed();
                        let r = match out {
                            Some(out) => json_resp(&out, 200),
                            None => empty(202),
                        };
                        if crate::config::env("SIAR_TIMING").is_some() {
                            let t3 = t0.elapsed();
                            let _ = req.respond(r);
                            eprintln!(
                                "[timing] read+parse {:?} handle {:?} serialize {:?} respond {:?}",
                                t1,
                                t2 - t1,
                                t3 - t2,
                                t0.elapsed() - t3
                            );
                            return;
                        }
                        r
                    }
                }
            }
        }
        p if p.starts_with("/oauth/") && p.ends_with("/callback") && method == Method::Get => {
            let platform = &p["/oauth/".len()..p.len() - "/callback".len()];
            if platform.is_empty() || !platform.chars().all(|c| is_word(c) || c == '-') {
                text("Not found", 404)
            } else if let Some(err) = query_param(&query, "error_description").or_else(|| query_param(&query, "error")) {
                text(&format!("Connection cancelled: {err}"), 400)
            } else {
                let code = query_param(&query, "code").unwrap_or_default();
                let state = query_param(&query, "state").unwrap_or_default();
                match finish_oauth(platform, &code, &state) {
                    Ok(accounts) => {
                        let names: Vec<String> = accounts
                            .iter()
                            .map(|a| {
                                let h = a
                                    .get("handle")
                                    .filter(|h| !h.is_null())
                                    .or(a.get("name"))
                                    .map(crate::util::js_string)
                                    .unwrap_or_default();
                                format!("{}:{h}", crate::util::js_string(&a["platform"]))
                            })
                            .collect();
                        text(&format!("Connected {}. You can close this tab.", names.join(", ")), 200)
                    }
                    Err(e) => text(&format!("Connection failed: {e}"), 400),
                }
            }
        }
        // Meta webhooks (Instagram comments, DMs, postbacks). Optional: polling works without them.
        "/webhooks/meta" if method == Method::Get => {
            let want = crate::config::env("SIAR_META_VERIFY_TOKEN").filter(|w| !w.is_empty());
            let ok = want.is_some() && query_param(&query, "hub.mode").as_deref() == Some("subscribe") && query_param(&query, "hub.verify_token") == want;
            if ok {
                text(&query_param(&query, "hub.challenge").unwrap_or_default(), 200)
            } else {
                text("Forbidden", 403)
            }
        }
        "/webhooks/meta" if method == Method::Post => {
            let mut raw = vec![];
            let _ = req.as_reader().read_to_end(&mut raw);
            let sig = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("x-hub-signature-256"))
                .map(|h| h.value.as_str().to_string());
            if !crate::automations::verify_meta_signature(&raw, sig.as_deref()) {
                text("Bad signature", 401)
            } else {
                // Answer Meta at once; process in the background.
                std::thread::spawn(move || {
                    let r = serde_json::from_slice::<Value>(&raw)
                        .map_err(crate::util::Error::from)
                        .and_then(|b| crate::automations::handle_meta_webhook(&b));
                    if let Err(e) = r {
                        eprintln!("[webhook] {e}");
                    }
                });
                text("ok", 200)
            }
        }
        // Tracked links from automation DMs.
        p if p.starts_with("/l/") && method == Method::Get && is_link_code(&p[3..]) => match crate::automations::click_link(&p[3..]) {
            Ok(Some(to)) => {
                let r = empty(302).with_header(header("location", &to));
                let _ = req.respond(r);
                return;
            }
            _ => text("Not found", 404),
        },
        p if p.starts_with("/media/") && (method == Method::Get || method == Method::Head) => match serve_media(&p["/media/".len()..]) {
            Ok(Some((file, ct))) => {
                let r = Response::from_file(file)
                    .with_chunked_threshold(usize::MAX)
                    .with_header(header("cache-control", "public, max-age=86400"))
                    .with_header(header("content-type", ct));
                let _ = req.respond(r);
                return;
            }
            _ => text("Not found", 404),
        },
        _ => text("Not found", 404),
    };
    let _ = req.respond(resp);
}

/// `/media/(med_[0-9a-f]{16})/([\w.\-]+)`
fn serve_media(rest: &str) -> Result<Option<(std::fs::File, &'static str)>> {
    let Some((id, name)) = rest.split_once('/') else { return Ok(None) };
    let id_ok = id.len() == 20 && id.starts_with("med_") && id[4..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    let name_ok = !name.is_empty() && name.chars().all(|c| is_word(c) || c == '.' || c == '-');
    if !id_ok || !name_ok || name == "." || name == ".." {
        return Ok(None);
    }
    let path = local_media_path(&format!("{id}/{name}"));
    let ext = name.rfind('.').map(|i| name[i..].to_lowercase()).unwrap_or_default();
    match std::fs::File::open(path) {
        Ok(f) if f.metadata().map(|m| m.is_file()).unwrap_or(false) => Ok(Some((f, mime_for_ext(&ext).unwrap_or("application/octet-stream")))),
        _ => Ok(None),
    }
}

/// Requests go to a pool of reused worker threads (warm stacks and allocator caches). When every
/// worker is busy (a long publish_now, say) another one is started, so nothing waits behind it.
fn dispatch(server: Arc<Server>) {
    let (tx, rx) = mpsc::channel::<Request>();
    let rx = Arc::new(Mutex::new(rx));
    // Workers waiting for a request. The dispatcher claims one per request or starts a new worker,
    // so a request never queues behind a busy one.
    let idle = Arc::new(AtomicUsize::new(0));
    let spawn_worker = |rx: Arc<Mutex<mpsc::Receiver<Request>>>, idle: Arc<AtomicUsize>| {
        std::thread::Builder::new().name("worker".into()).stack_size(1024 * 1024).spawn(move || loop {
            let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
            match next {
                Ok(req) => {
                    route(req);
                    idle.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => return,
            }
        })
    };
    for _ in 0..2 {
        if spawn_worker(rx.clone(), idle.clone()).is_ok() {
            idle.fetch_add(1, Ordering::SeqCst);
        }
    }
    for req in server.incoming_requests() {
        #[allow(deprecated)]
        let claimed = idle.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        if !claimed {
            if let Err(e) = spawn_worker(rx.clone(), idle.clone()) {
                eprintln!("[http] cannot start a worker: {e}");
            }
        }
        if tx.send(req).is_err() {
            return;
        }
    }
}

pub fn start() -> Result<()> {
    let c = config();
    if c.api_token.as_deref().map(|t| t.len() < 24).unwrap_or(true) {
        bail!("Set SIAR_API_TOKEN (24+ characters) before `serve`; it protects the /mcp endpoint");
    }
    if c.contact.is_empty() {
        eprintln!("SIAR_CONTACT is not set: the privacy and terms pages will have no contact email");
    }
    let addr = format!("{}:{}", c.host, c.port);
    let server = Arc::new(Server::http(&addr).map_err(|e| crate::util::Error::Msg(format!("listen {addr}: {e}")))?);
    eprintln!("siar listening on http://{addr} (public: {})", c.public_url);
    std::thread::Builder::new()
        .name("http".into())
        .spawn(move || dispatch(server))
        .map_err(|e| crate::util::Error::Msg(e.to_string()))?;
    Ok(())
}
