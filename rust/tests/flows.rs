//! The MCP tools end to end against fake
//! platforms (a Discord webhook, and the Instagram and Threads Graph APIs). One test function,
//! because the config, database and environment are process-wide.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use siar::util::{iso, now};

#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    follows: AtomicBool,
    comment_ts: AtomicI64,
    fail_next: AtomicI64,
    discord: Mutex<Vec<(Value, usize)>>,
}

fn respond(req: tiny_http::Request, status: u16, body: Value) {
    let r = tiny_http::Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(tiny_http::Header::from_bytes(&b"content-type"[..], &b"application/json"[..]).unwrap());
    let _ = req.respond(r);
}

fn parse_query(q: &str) -> Value {
    let mut m = serde_json::Map::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(decode(k), Value::from(decode(v)));
    }
    Value::Object(m)
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let mut out = vec![];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
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

/// Count multipart parts named files[...] and return payload_json.
fn parse_discord(body: &[u8], content_type: &str) -> (Value, usize) {
    let boundary = content_type.split("boundary=").nth(1).unwrap_or("").to_string();
    let text = String::from_utf8_lossy(body);
    let mut payload = Value::Null;
    let mut files = 0;
    for part in text.split(&format!("--{boundary}")) {
        if part.contains("name=\"payload_json\"") {
            let json = part.split("\r\n\r\n").nth(1).unwrap_or("").trim_end_matches("\r\n");
            payload = serde_json::from_str(json).unwrap_or(Value::Null);
        }
        if part.contains("name=\"files[") {
            files += 1;
        }
    }
    (payload, files)
}

fn serve(fake: Arc<Fake>) -> u16 {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let url = req.url().to_string();
            let (path, query) = url.split_once('?').unwrap_or((&url, ""));
            let path = path.to_string();
            let method = req.method().to_string();
            let ct = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("content-type"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut raw = vec![];
            let _ = req.as_reader().read_to_end(&mut raw);

            // Discord webhook
            if path.starts_with("/hook") {
                if method == "GET" {
                    respond(req, 200, json!({ "id": "w1", "name": "Test hook", "channel_id": "c1", "guild_id": "g1" }));
                    continue;
                }
                if fake.fail_next.load(Ordering::SeqCst) > 0 {
                    fake.fail_next.fetch_sub(1, Ordering::SeqCst);
                    let _ = req.respond(tiny_http::Response::from_string("busy").with_status_code(503));
                    continue;
                }
                let parsed = parse_discord(&raw, &ct);
                let n = {
                    let mut d = fake.discord.lock().unwrap();
                    d.push(parsed);
                    d.len()
                };
                respond(req, 200, json!({ "id": format!("m{n}"), "channel_id": "c1" }));
                continue;
            }

            let body = if method == "GET" {
                parse_query(query)
            } else if ct.contains("json") {
                serde_json::from_slice(&raw).unwrap_or(Value::Null)
            } else {
                parse_query(&String::from_utf8_lossy(&raw))
            };
            let n_calls = {
                let mut c = fake.calls.lock().unwrap();
                c.push(Call {
                    method: method.clone(),
                    path: path.clone(),
                    body: body.clone(),
                });
                c.len()
            };
            let p = path.strip_prefix("/ig").or_else(|| path.strip_prefix("/th")).unwrap_or(&path).to_string();
            if path.starts_with("/th") {
                let r = match (method.as_str(), p.as_str()) {
                    ("POST", "/me/threads") => json!({ "id": "tc1" }),
                    (_, "/tc1") => json!({ "status": "FINISHED" }),
                    (_, "/me/threads_publish") => json!({ "id": "tp1" }),
                    (_, "/tp1") => json!({ "permalink": "https://threads.net/@demo/post/tp1" }),
                    _ => {
                        respond(req, 404, json!("nf"));
                        continue;
                    }
                };
                respond(req, 200, r);
                continue;
            }
            let ts = fake.comment_ts.load(Ordering::SeqCst);
            let r = match (method.as_str(), p.as_str()) {
                ("POST", "/ig1/media") => json!({ "id": "c1" }),
                (_, "/c1") => json!({ "status_code": "FINISHED" }),
                (_, "/ig1/media_publish") => json!({ "id": "m100" }),
                ("GET", "/m100") => json!({ "permalink": "https://instagram.com/p/m100" }),
                ("POST", "/m100/comments") => json!({ "id": "first" }),
                ("GET", "/ig1/media") => json!({ "data": [{
                    "id": "m100", "caption": "hi", "media_type": "VIDEO", "media_product_type": "REELS",
                    "permalink": "https://instagram.com/p/m100", "timestamp": iso(now()), "like_count": 3,
                    "comments_count": if ts != 0 { 2 } else { 1 }
                }] }),
                (_, "/m100/comments") => {
                    let mut data = vec![
                        json!({ "id": "cm1", "text": "Info dong kak", "timestamp": iso(ts), "username": "fan", "from": { "id": "u1", "username": "fan" } }),
                    ];
                    if ts > 1 {
                        data.push(
                            json!({ "id": "cm2", "text": "info lagi", "timestamp": iso(ts + 1), "username": "fan", "from": { "id": "u1", "username": "fan" } }),
                        );
                    }
                    json!({ "data": data })
                }
                (_, "/cm1/replies") => json!({ "id": "r1" }),
                (_, "/ig1/messages") => json!({ "message_id": format!("mid{n_calls}"), "recipient_id": "u1" }),
                (_, "/u1") => json!({ "is_user_follow_business": fake.follows.load(Ordering::SeqCst), "username": "fan" }),
                _ => {
                    respond(req, 404, json!(format!("nf {p}")));
                    continue;
                }
            };
            respond(req, 200, r);
        }
    });
    port
}

fn call(name: &str, args: Value) -> Result<Value, String> {
    let r = siar::mcp::handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })).unwrap();
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    if r["result"]["isError"] == true {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap())
}

fn sql(q: &str) {
    siar::db::with(|c| {
        c.execute_batch(q)?;
        Ok(())
    })
    .unwrap();
}

fn count(q: &str) -> i64 {
    siar::db::with(|c| Ok(c.query_row(q, [], |r| r.get(0))?)).unwrap()
}

fn run_due() {
    sql("UPDATE jobs SET run_at = 0 WHERE status = 'pending'");
    siar::automations::run_jobs(20).unwrap();
}

fn sent(fake: &Fake, suffix: &str) -> Vec<Call> {
    fake.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.method == "POST" && c.path.ends_with(suffix))
        .cloned()
        .collect()
}

fn fake_media(id: &str, mime: &str, url: &str) {
    let name = url.rsplit('/').next().unwrap();
    siar::db::with(|c| {
        c.execute(
            "INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES (?, ?, ?, 1000, 'r2', ?, ?, NULL, ?)",
            rusqlite::params![id, name, mime, format!("{id}/x"), url, now()],
        )?;
        Ok(())
    })
    .unwrap();
}

fn connected(platform: &str, ext: &str) -> String {
    let accs = siar::accounts::save_accounts(
        platform,
        &[siar::accounts::Connected {
            external_id: ext.into(),
            name: "Demo".into(),
            handle: Some("demo".into()),
            secret: json!({ "access_token": "tok" }),
            meta: None,
            expires_at: None,
        }],
    )
    .unwrap();
    accs[0]["id"].as_str().unwrap().to_string()
}

#[test]
fn core_and_engage_flows() {
    let dir = std::env::temp_dir().join(format!("siar-rs-flows-{}", siar::util::new_id("t")));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = Arc::new(Fake::default());
    let port = serve(fake.clone());
    std::env::set_var("SIAR_DATA_DIR", &dir);
    std::env::set_var("SIAR_VAULT_KEY", "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=");
    std::env::set_var("SIAR_PUBLIC_URL", "http://localhost:9");
    std::env::set_var("INSTAGRAM_CLIENT_SECRET", "igsecret");
    std::env::set_var("SIAR_IG_BASE", format!("http://127.0.0.1:{port}/ig"));
    std::env::set_var("SIAR_THREADS_BASE", format!("http://127.0.0.1:{port}/th"));
    siar::db::init().unwrap();

    // ---------- core.test.ts: connect, stage, validate, schedule, publish, retry (Discord) ----------
    let acc = call(
        "connect_account",
        json!({ "platform": "discord", "credentials": { "webhook_url": format!("http://127.0.0.1:{port}/hook") } }),
    )
    .unwrap()["connected"][0]
        .clone();
    assert_eq!(acc["platform"], "discord");
    let acc_id = acc["id"].as_str().unwrap().to_string();
    assert!(
        !call("list_accounts", json!({})).unwrap().to_string().contains(&format!("127.0.0.1:{port}")),
        "secrets are never returned"
    );

    let png: Vec<u8> = (0..29)
        .map(|i| u8::from_str_radix(&"89504e470d0a1a0a0000000d4948445200000001000000010806000000"[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    std::fs::write(dir.join("dot.png"), png).unwrap();
    let media = call("stage_media", json!({ "path": dir.join("dot.png"), "alt": "a dot" })).unwrap();
    assert_eq!(media["mime"], "image/png");
    assert_eq!(media["storage"], "local");

    let too_long = call("validate_post", json!({ "content": "x".repeat(2001), "targets": [acc_id] })).unwrap();
    assert_eq!(too_long[0]["ok"], false);
    let e = call(
        "schedule_post",
        json!({ "content": "x".repeat(2001), "targets": [acc_id], "at": iso(now() + 60_000) }),
    )
    .unwrap_err();
    assert!(e.contains("validation"), "{e}");
    let e = call("schedule_post", json!({ "content": "hi", "targets": [acc_id], "at": "2026-11-17T08:00:00" })).unwrap_err();
    assert!(e.contains("timezone"), "{e}");

    let draft = call(
        "create_draft",
        json!({ "content": "Hello @everyone", "media_ids": [media["id"]], "targets": [acc_id], "options": { "discord": { "username": "siar" } } }),
    )
    .unwrap();
    assert_eq!(draft["post"]["status"], "draft");
    let post_id = draft["post"]["id"].as_str().unwrap().to_string();
    let sched = call("schedule_post", json!({ "post_id": post_id, "at": iso(now() + 1000) })).unwrap();
    assert_eq!(sched["post"]["status"], "scheduled");
    assert!(siar::posts::claim_due(10).unwrap().is_empty(), "not due yet");
    sql(&format!("UPDATE targets SET next_attempt_at = 0 WHERE post_id = '{post_id}'"));

    fake.fail_next.store(1, Ordering::SeqCst); // first attempt gets a 503 -> retried later
    for id in siar::posts::claim_due(10).unwrap() {
        siar::posts::publish_target(&id).unwrap();
    }
    let post = call("get_post", json!({ "post_id": post_id })).unwrap();
    assert_eq!(post["targets"][0]["status"], "pending");
    assert_eq!(post["targets"][0]["attempts"], 1);
    assert!(siar::posts::claim_due(10).unwrap().is_empty(), "backoff");

    sql(&format!("UPDATE targets SET next_attempt_at = 0 WHERE post_id = '{post_id}'"));
    let ids = siar::posts::claim_due(10).unwrap();
    assert!(siar::posts::claim_due(10).unwrap().is_empty(), "already claimed, no double post");
    for id in ids {
        siar::posts::publish_target(&id).unwrap();
    }
    let post = call("get_post", json!({ "post_id": post_id })).unwrap();
    assert_eq!(post["status"], "published");
    assert_eq!(post["targets"][0]["url"], "https://discord.com/channels/g1/c1/m1");
    {
        let d = fake.discord.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0["username"], "siar");
        assert_eq!(d[0].0["allowed_mentions"], json!({ "parse": [] }));
        assert_eq!(d[0].1, 1);
    }
    let published = call("publish_now", json!({ "content": "second", "targets": [acc_id] })).unwrap();
    assert_eq!(published["status"], "published");
    assert_eq!(fake.discord.lock().unwrap().len(), 2);
    assert_eq!(call("status", json!({})).unwrap()["accounts"], 1);

    // ---------- engage.test.ts ----------
    let m = |kw: &[&str], how: &str, t: &str| siar::automations::matches(&kw.iter().map(|s| s.to_string()).collect::<Vec<_>>(), how, Some(t));
    assert!(m(&["info"], "contains", "Info dong kak"));
    assert!(!m(&["info"], "contains", "informasi"));
    assert!(m(&["harga"], "exact", " Harga "));
    assert!(m(&[], "contains", "anything"));

    // Instagram reel with trial, AI label, cover and first comment
    let ig = connected("instagram", "ig1");
    fake_media("med_00000000000000a1", "video/mp4", "https://cdn.example.com/reel.mp4");
    fake_media("med_00000000000000a2", "image/jpeg", "https://cdn.example.com/cover.jpg");
    let r = call(
        "publish_now",
        json!({
            "content": "Reel caption",
            "media_ids": ["med_00000000000000a1"],
            "targets": [ig],
            "options": { "instagram": { "trial": "auto", "is_ai_generated": true, "cover_media_id": "med_00000000000000a2", "first_comment": "#tag", "collaborators": ["@friend"] } },
        }),
    )
    .unwrap();
    assert_eq!(r["status"], "published", "{r}");
    assert_eq!(r["targets"][0]["url"], "https://instagram.com/p/m100");
    let create = sent(&fake, "/ig1/media")[0].body.clone();
    assert_eq!(create["media_type"], "REELS");
    assert_eq!(create["trial_params"], json!({ "graduation_strategy": "SS_PERFORMANCE" }));
    assert_eq!(create["is_ai_generated"], true);
    assert_eq!(create["cover_url"], "https://cdn.example.com/cover.jpg");
    assert_eq!(create["collaborators"], json!(["friend"]));
    assert_eq!(sent(&fake, "/m100/comments")[0].body["message"], "#tag");

    // Threads poll with AI label
    let th = connected("threads", "th1");
    let bad = call(
        "validate_post",
        json!({ "content": "Pick", "targets": [th], "options": { "threads": { "poll": { "options": ["only one"] } } } }),
    )
    .unwrap();
    assert_eq!(bad[0]["ok"], false);
    let r = call("publish_now", json!({ "content": "Pick one", "targets": [th], "options": { "threads": { "poll": { "options": ["A", "B", "C"] }, "is_made_with_ai": true, "topic_tag": "diet" } } })).unwrap();
    assert_eq!(r["status"], "published", "{r}");
    let body = sent(&fake, "/th/me/threads")[0].body.clone();
    assert_eq!(body["media_type"], "TEXT");
    assert_eq!(body["poll_attachment"], json!({ "option_a": "A", "option_b": "B", "option_c": "C" }));
    assert_eq!(body["is_made_with_ai"], true);

    // comment_to_dm: public reply, private DM with buttons, once per person
    let ig_id = call("list_accounts", json!({ "platform": "instagram" })).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let auto = call(
        "create_automation",
        json!({
            "account_id": ig_id, "kind": "comment_to_dm", "keywords": ["info"],
            "public_replies": ["Cek DM ya!", "Sudah dikirim ke DM"], "dm_text": "Ini linknya",
            "buttons": [{ "title": "Buka", "url": "https://example.com" }],
        }),
    )
    .unwrap();
    let e = call(
        "create_automation",
        json!({ "account_id": auto["account_id"], "kind": "comment_reply", "public_replies": ["a", "b", "c"], "timing": "fast" }),
    )
    .unwrap_err();
    assert!(e.contains("fast"), "{e}");
    fake.comment_ts.store(now() + 1000, Ordering::SeqCst);
    siar::automations::poll_account(&ig_id, true, false).unwrap();
    let pending: Vec<String> =
        siar::db::with(|c| siar::db::rows(c, "SELECT type FROM jobs WHERE status = 'pending' ORDER BY run_at", [], |r| r.get(0))).unwrap();
    assert_eq!(pending, vec!["public_reply", "dm"]);
    run_due();
    let reply = sent(&fake, "/cm1/replies")[0].body["message"].as_str().unwrap().to_string();
    assert!(["Cek DM ya!", "Sudah dikirim ke DM"].contains(&reply.as_str()));
    let dm = sent(&fake, "/ig1/messages").last().unwrap().body.clone();
    assert_eq!(dm["recipient"], json!({ "comment_id": "cm1" }));
    assert_eq!(dm["message"]["attachment"]["payload"]["template_type"], "button");
    assert_eq!(
        dm["message"]["attachment"]["payload"]["buttons"][0],
        json!({ "type": "web_url", "url": "https://example.com", "title": "Buka" })
    );
    fake.comment_ts.fetch_add(10, Ordering::SeqCst);
    siar::automations::poll_account(&ig_id, true, false).unwrap();
    assert_eq!(count("SELECT COUNT(*) FROM jobs WHERE status = 'pending'"), 0, "once per person");
    let comments = call("list_comments", json!({ "account_id": ig_id, "status": "all" })).unwrap();
    assert_eq!(comments["comments"][0]["text"], json!({ "untrusted": "info lagi" }));
    let list = call("list_automations", json!({ "account_id": ig_id })).unwrap();
    assert_eq!(list[0]["stats"]["dms"], 1);
    assert_eq!(list[0]["stats"]["public_replies"], 1);

    // dm_keyword with follow gate through webhooks
    let auto = call(
        "create_automation",
        json!({
            "account_id": ig_id, "kind": "dm_keyword", "keywords": ["harga"], "dm_text": "Harganya Rp99rb",
            "follow_gate": { "ask_text": "Follow dulu ya", "button_title": "Sudah follow", "retry_text": "Belum kelihatan follow" },
            "react": true,
        }),
    )
    .unwrap();
    let auto_id = auto["id"].as_str().unwrap().to_string();
    let hook = |messaging: Value| json!({ "object": "instagram", "entry": [{ "id": "ig1", "time": now(), "messaging": [messaging] }] });
    let raw = hook(json!({})).to_string();
    let sig = {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"igsecret").unwrap();
        mac.update(raw.as_bytes());
        siar::util::hex(&mac.finalize().into_bytes())
    };
    assert!(siar::automations::verify_meta_signature(raw.as_bytes(), Some(&format!("sha256={sig}"))));
    assert!(!siar::automations::verify_meta_signature(raw.as_bytes(), Some("sha256=00")));

    siar::automations::handle_meta_webhook(&hook(
        json!({ "sender": { "id": "u1" }, "recipient": { "id": "ig1" }, "timestamp": now() + 1000, "message": { "mid": "x1", "text": "berapa harga?" } }),
    ))
    .unwrap();
    run_due();
    let ask = sent(&fake, "/ig1/messages").last().unwrap().body.clone();
    assert_eq!(ask["message"]["attachment"]["payload"]["text"], "Follow dulu ya");
    assert_eq!(
        ask["message"]["attachment"]["payload"]["buttons"][0],
        json!({ "type": "postback", "payload": format!("fg:{auto_id}"), "title": "Sudah follow" })
    );
    assert!(sent(&fake, "/ig1/messages").iter().any(|c| c.body["sender_action"] == "react"));

    fake.follows.store(false, Ordering::SeqCst);
    siar::automations::handle_meta_webhook(&hook(json!({ "sender": { "id": "u1" }, "recipient": { "id": "ig1" }, "timestamp": now(), "postback": { "mid": "p1", "payload": format!("fg:{auto_id}"), "title": "Sudah follow" } }))).unwrap();
    run_due();
    assert_eq!(
        sent(&fake, "/ig1/messages").last().unwrap().body["message"]["attachment"]["payload"]["text"],
        "Belum kelihatan follow"
    );

    fake.follows.store(true, Ordering::SeqCst);
    siar::automations::handle_meta_webhook(&hook(json!({ "sender": { "id": "u1" }, "recipient": { "id": "ig1" }, "timestamp": now(), "postback": { "mid": "p2", "payload": format!("fg:{auto_id}"), "title": "Sudah follow" } }))).unwrap();
    run_due();
    let fin = sent(&fake, "/ig1/messages").last().unwrap().body.clone();
    assert_eq!(fin["recipient"], json!({ "id": "u1" }));
    assert_eq!(fin["message"], json!({ "text": "Harganya Rp99rb" }));

    // best times from cached account posts: six evening posts get 3x the views of six morning ones
    sql(&format!("DELETE FROM account_posts WHERE account_id = '{ig_id}'"));
    let base = siar::util::parse_iso("2026-11-02T00:00:00+07:00").unwrap();
    for i in 0..12i64 {
        let evening = i % 2 == 0;
        let at = base + (i / 2) * 86_400_000 + (if evening { 19 } else { 7 }) * 3_600_000;
        siar::db::with(|c| {
            c.execute(
                "INSERT INTO account_posts (account_id, remote_id, created_at, type, metrics, fetched_at) VALUES (?, ?, ?, 'reel', ?, ?)",
                rusqlite::params![
                    ig_id,
                    format!("p{i}"),
                    at,
                    json!({ "views": if evening { 3000 } else { 1000 } }).to_string(),
                    now()
                ],
            )?;
            Ok(())
        })
        .unwrap();
    }
    sql(&format!(
        "UPDATE account_posts SET created_at = created_at + {} WHERE account_id = '{ig_id}'",
        now() - base - 7 * 86_400_000
    ));
    let r = call("best_times", json!({ "account_id": ig_id, "tz": "Asia/Jakarta" })).unwrap();
    assert_eq!(r["posts_scored"], 12);
    assert_eq!(r["enough_data"], true);
    assert!(r["recommended"][0]["x_usual"].as_f64().unwrap() > 1.0, "{r}");

    // ideas board links to posts
    let idea = call("create_idea", json!({ "title": "Before/after carousel", "tags": ["shop"] })).unwrap();
    let th_id = call("list_accounts", json!({ "platform": "threads" })).unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    call("create_draft", json!({ "content": "From the idea", "targets": [th_id], "idea_id": idea["id"] })).unwrap();
    let got = call("list_ideas", json!({ "tag": "shop" })).unwrap();
    assert_eq!(got[0]["posts"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
