//! Hosted mode end to end over HTTP: tenant isolation, API keys, envelope encryption at rest,
//! plan limits, payment webhooks (signatures and idempotency), CSRF on dashboard forms, and the
//! guards against reading server files or internal URLs. One test function: the config, control
//! database and server are process-wide.

use base64::Engine;
use hmac::Mac;
use serde_json::{json, Value};

use siar::util::{now, Error};

fn base() -> String {
    format!("http://127.0.0.1:{}", std::env::var("SIAR_PORT").unwrap())
}

/// (HTTP status, body)
fn http(r: siar::net::Req) -> (u16, String) {
    match r.send() {
        Ok(res) => (res.status, res.text().unwrap_or_default()),
        Err(Error::Http { status, body, .. }) => (status, body),
        Err(e) => panic!("request failed: {e}"),
    }
}

/// Tool result: Ok(value) or Err(error text).
fn tool(key: &str, name: &str, args: Value) -> Result<Value, String> {
    let (status, body) = http(
        siar::net::post(format!("{}/mcp", base()))
            .bearer(key)
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })),
    );
    assert_eq!(status, 200, "{body}");
    let r: Value = serde_json::from_str(&body).unwrap();
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    if r["result"]["isError"] == true {
        return Err(text);
    }
    Ok(serde_json::from_str(&text).unwrap())
}

fn connect(tenant: &str, platform: &str, ext: &str) -> Result<Vec<Value>, Error> {
    let t = siar::tenant::get(tenant)?;
    let _g = siar::tenant::enter(t);
    siar::accounts::save_accounts(
        platform,
        &[siar::accounts::Connected {
            external_id: ext.into(),
            name: format!("Account {ext}"),
            handle: Some(format!("h{ext}")),
            secret: json!({ "access_token": format!("secret-token-{ext}") }),
            meta: None,
            expires_at: None,
        }],
    )
}

fn polar_post(secret: &str, id: &str, body: &str) -> (u16, String) {
    let ts = (now() / 1000).to_string();
    let key = base64::engine::general_purpose::STANDARD.decode(secret.trim_start_matches("whsec_")).unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(format!("{id}.{ts}.{body}").as_bytes());
    let sig = format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()));
    http(
        siar::net::post(format!("{}/billing/polar/webhook", base()))
            .header("webhook-id", id)
            .header("webhook-timestamp", ts)
            .header("webhook-signature", sig)
            .header("content-type", "application/json")
            .body(siar::net::Body::Bytes(body.as_bytes().to_vec())),
    )
}

fn doku_post(request_id: &str, body: &str, secret: &str) -> (u16, String) {
    let ts = siar::util::iso(now())[..19].to_string() + "Z";
    let comp = siar::billing::doku_component("MCH-TEST", request_id, &ts, "/billing/doku/notify", Some(body.as_bytes()));
    http(
        siar::net::post(format!("{}/billing/doku/notify", base()))
            .header("Client-Id", "MCH-TEST")
            .header("Request-Id", request_id)
            .header("Request-Timestamp", ts)
            .header("Signature", siar::billing::doku_signature(secret, &comp))
            .header("content-type", "application/json")
            .body(siar::net::Body::Bytes(body.as_bytes().to_vec())),
    )
}

/// Fake Polar API (DELETE /v1/subscriptions/<id>, failing the first time) and Cloudflare Email
/// endpoint; records (method, path, body).
fn fake_apis() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let rec = calls.clone();
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let mut raw = vec![];
            let _ = req.as_reader().read_to_end(&mut raw);
            let path = req.url().to_string();
            let method = req.method().to_string();
            let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            let n = {
                let mut c = rec.lock().unwrap();
                c.push((method.clone(), path.clone(), body));
                c.iter().filter(|x| x.1 == path).count()
            };
            let (status, out) = if path.starts_with("/v1/subscriptions/") && n == 1 {
                (500, json!({ "error": "temporarily unavailable" }))
            } else if path.starts_with("/v1/subscriptions/") {
                (200, json!({ "id": path.rsplit('/').next(), "status": "canceled" }))
            } else {
                (200, json!({ "success": true, "errors": [], "result": { "delivered": ["x"] } }))
            };
            let _ = req.respond(tiny_http::Response::from_string(out.to_string()).with_status_code(status));
        }
    });
    (port, calls)
}

#[test]
fn hosted_mode() {
    let (fake_port, fake_calls) = fake_apis();
    let dir = std::env::temp_dir().join(format!("siar-hosted-{}", siar::util::new_id("t")));
    std::fs::create_dir_all(&dir).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let polar_secret = format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode(b"0123456789abcdef0123456789abcdef")
    );
    for (k, v) in [
        ("SIAR_MODE", "hosted".to_string()),
        ("SIAR_DATA_DIR", dir.display().to_string()),
        ("SIAR_VAULT_KEY", base64::engine::general_purpose::STANDARD.encode([3u8; 32])),
        ("SIAR_PUBLIC_URL", "http://localhost:9".into()),
        ("SIAR_PORT", port.to_string()),
        ("SIAR_CONTACT", "privacy@example.com".into()),
        ("POLAR_WEBHOOK_SECRET", polar_secret.clone()),
        ("DOKU_CLIENT_ID", "MCH-TEST".into()),
        ("DOKU_SECRET_KEY", "SK-TEST".into()),
        ("POLAR_ACCESS_TOKEN", "polar_oat_test".into()),
        ("SIAR_POLAR_API_BASE", format!("http://127.0.0.1:{fake_port}")),
        ("SIAR_MAIL_FROM", "siar <billing@diatur.com>".into()),
        ("SIAR_MAIL_CF_ACCOUNT_ID", "acct123".into()),
        ("SIAR_MAIL_CF_API_TOKEN", "cf-token".into()),
        ("SIAR_MAIL_API_BASE", format!("http://127.0.0.1:{fake_port}/client/v4")),
    ] {
        std::env::set_var(k, v);
    }
    siar::control::init().unwrap();
    siar::server::start().unwrap();
    for _ in 0..50 {
        if siar::net::get(format!("{}/health", base())).send().is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // ---------- two tenants ----------
    let a = siar::control::create_tenant("Tenant A", None).unwrap();
    let b = siar::control::create_tenant("Tenant B", None).unwrap();
    let (_, key_a) = siar::control::create_api_key(&a, "a").unwrap();
    let (key_b_id, key_b) = siar::control::create_api_key(&b, "b").unwrap();
    let acc_a = connect(&a, "discord", "111").unwrap()[0]["id"].as_str().unwrap().to_string();

    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let media = tool(&key_a, "stage_media", json!({ "base64": png, "filename": "dot.png" })).unwrap();
    let media_id = media["id"].as_str().unwrap().to_string();
    assert!(
        media["url"].as_str().unwrap().starts_with(&format!("http://localhost:9/media/t/{a}/med_")),
        "{media}"
    );
    let draft = tool(
        &key_a,
        "create_draft",
        json!({ "content": "secret plans of A", "targets": [acc_a], "media_ids": [media_id] }),
    )
    .unwrap();
    let post_a = draft["post"]["id"].as_str().unwrap().to_string();
    assert_eq!(tool(&key_a, "list_posts", json!({})).unwrap().as_array().unwrap().len(), 1);

    // B sees none of it, by listing or by id.
    assert_eq!(tool(&key_b, "list_posts", json!({})).unwrap(), json!([]));
    assert_eq!(tool(&key_b, "list_accounts", json!({})).unwrap(), json!([]));
    assert_eq!(tool(&key_b, "list_media", json!({})).unwrap(), json!([]));
    assert!(tool(&key_b, "get_post", json!({ "post_id": post_a })).unwrap_err().contains("Unknown post"));
    assert!(tool(&key_b, "delete_post", json!({ "post_id": post_a })).unwrap_err().contains("Unknown post"));
    assert!(tool(&key_b, "delete_media", json!({ "media_id": media_id }))
        .unwrap_err()
        .contains("Unknown media"));
    assert!(tool(&key_b, "check_account", json!({ "account_id": acc_a }))
        .unwrap_err()
        .contains("Unknown account"));
    assert!(tool(&key_b, "create_draft", json!({ "content": "x", "targets": [acc_a] }))
        .unwrap_err()
        .contains("Unknown account"));
    assert!(tool(&key_b, "create_draft", json!({ "content": "x", "targets": ["h111"] }))
        .unwrap_err()
        .contains("Unknown account"));
    assert!(tool(&key_b, "status", json!({})).unwrap()["accounts"] == 0);
    // A still has everything.
    assert_eq!(tool(&key_a, "get_post", json!({ "post_id": post_a })).unwrap()["content"], "secret plans of A");

    // Keys: unknown, malformed and revoked keys are refused.
    let (s, _) = http(
        siar::net::post(format!("{}/mcp", base()))
            .bearer("siar_0000")
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
    );
    assert_eq!(s, 401);
    let (s, _) = http(siar::net::post(format!("{}/mcp", base())).json(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })));
    assert_eq!(s, 401);
    siar::control::revoke_api_key(&b, &key_b_id).unwrap();
    let (s, _) = http(
        siar::net::post(format!("{}/mcp", base()))
            .bearer(&key_b)
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
    );
    assert_eq!(s, 401);
    let (_, key_b) = siar::control::create_api_key(&b, "b2").unwrap();
    assert!(siar::control::revoke_api_key(&b, "key_of_someone_else").is_err());

    // Envelope encryption at rest: A's token is sealed with A's data key, not the master key.
    let blob: Vec<u8> = rusqlite::Connection::open(siar::tenant::db_path(&a))
        .unwrap()
        .query_row("SELECT secret FROM accounts", [], |r| r.get(0))
        .unwrap();
    assert!(siar::vault::open_with(&[3u8; 32], &blob).is_err(), "master key must not open tenant secrets");
    let tb = siar::tenant::get(&b).unwrap();
    assert!(siar::vault::open_with(tb.key(), &blob).is_err(), "B's key must not open A's secrets");
    let ta = siar::tenant::get(&a).unwrap();
    assert_eq!(siar::vault::open_with(ta.key(), &blob).unwrap()["access_token"], "secret-token-111");
    drop((ta, tb));
    // Tokens never appear in tool output.
    assert!(!tool(&key_a, "list_accounts", json!({})).unwrap().to_string().contains("secret-token"));

    // Media files are served from the tenant's own directory.
    let path = media["url"].as_str().unwrap().replace("http://localhost:9", &base());
    assert_eq!(http(siar::net::get(&path)).0, 200);
    assert_eq!(http(siar::net::get(path.replace(&a, &b))).0, 404);

    // ---------- guards ----------
    let e = tool(&key_a, "stage_media", json!({ "path": "/etc/passwd" })).unwrap_err();
    assert!(e.contains("not available on the hosted service"), "{e}");
    let e = tool(&key_a, "stage_media", json!({ "url": format!("{}/health", base()) })).unwrap_err();
    assert!(e.contains("private or local"), "{e}");
    let e = tool(&key_a, "stage_media", json!({ "url": "http://169.254.169.254/latest/meta-data/" })).unwrap_err();
    assert!(e.contains("private or local"), "{e}");

    // ---------- plan limits ----------
    siar::control::put_plan(&json!({ "id": "tiny", "name": "Tiny", "max_accounts": 1, "max_posts_month": 1, "max_storage_mb": 1, "max_upload_mb": 1 }))
        .unwrap();
    siar::control::put_plan(&json!({ "id": "starter", "name": "Starter", "price_usd_cents": 100, "polar_product_id": "prod_starter", "max_accounts": 10, "max_posts_month": 100, "max_storage_mb": 100, "max_upload_mb": 10, "prices": [{ "provider": "doku", "months": 1, "amount": 16000, "currency": "IDR" }] })).unwrap();
    siar::control::set_setting("default_plan", "tiny").unwrap();
    let e = connect(&a, "discord", "222").unwrap_err().to_string();
    assert!(e.contains("Plan limit") && e.contains("1 connected account"), "{e}");
    connect(&a, "discord", "111").expect("reconnecting an existing account is not a new one");
    let at = siar::util::iso(now() + 86_400_000);
    tool(&key_a, "schedule_post", json!({ "post_id": post_a, "at": at })).unwrap();
    tool(&key_a, "schedule_post", json!({ "post_id": post_a, "at": at })).expect("rescheduling the same post counts once");
    let e = tool(&key_a, "schedule_post", json!({ "content": "second", "targets": [acc_a], "at": at })).unwrap_err();
    assert!(e.contains("Plan limit") && e.contains("posts per month"), "{e}");
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 1_200_000]);
    let e = tool(&key_a, "stage_media", json!({ "base64": big, "filename": "big.png" })).unwrap_err();
    assert!(e.contains("Plan limit"), "{e}");
    assert_eq!(tool(&key_a, "status", json!({})).unwrap()["plan"]["usage"]["posts_this_month"], 1);
    // The scheduler index knows A has a post due.
    let due: Option<i64> = siar::control::with(|c| Ok(c.query_row("SELECT next_post_at FROM due WHERE tenant_id = ?", [&a], |r| r.get(0))?)).unwrap();
    assert!(due.is_some());

    // ---------- Polar webhooks: signature, idempotency, amount with fees, plan changes ----------
    let starter = siar::control::plan("starter").unwrap().unwrap();
    let gross = siar::billing::polar_price(&starter).total;
    assert_eq!(gross, 161, "$1.00 net at 5% + 1.5% + 50 cents");
    let short = json!({ "type": "subscription.active", "data": { "id": "sub_0", "status": "active", "amount": 100, "product_id": "prod_starter", "current_period_end": siar::util::iso(now() + 30 * 86_400_000), "metadata": { "tenant_id": a } } }).to_string();
    assert_eq!(
        polar_post(&polar_secret, "evt_0", &short),
        (200, "ignored\n".to_string()),
        "the net price alone does not activate"
    );
    assert_eq!(siar::control::effective_plan(&a).unwrap().id, "tiny");
    let ev = json!({ "type": "subscription.active", "data": { "id": "sub_1", "status": "active", "amount": gross, "product_id": "prod_starter", "current_period_end": siar::util::iso(now() + 30 * 86_400_000), "metadata": { "tenant_id": a }, "customer": { "external_id": a } } }).to_string();
    assert_eq!(polar_post(&polar_secret, "evt_1", &ev), (200, "applied\n".to_string()));
    assert_eq!(polar_post(&polar_secret, "evt_1", &ev), (200, "duplicate\n".to_string()));
    assert_eq!(polar_post("whsec_d3JvbmcgZW5vdWdoIGtleSBmb3IgdGVzdA==", "evt_2", &ev).0, 401);
    assert_eq!(siar::control::effective_plan(&a).unwrap().id, "starter");
    assert_eq!(siar::control::effective_plan(&b).unwrap().id, "tiny");
    tool(&key_a, "schedule_post", json!({ "content": "second", "targets": [acc_a], "at": at })).expect("pro allows more posts");
    let revoked =
        json!({ "type": "subscription.revoked", "data": { "id": "sub_1", "status": "canceled", "product_id": "prod_starter", "metadata": { "tenant_id": a } } })
            .to_string();
    assert_eq!(polar_post(&polar_secret, "evt_3", &revoked).0, 200);
    assert_eq!(siar::control::effective_plan(&a).unwrap().id, "tiny");

    // ---------- plan features: auto-replies and analytics are on Pro only ----------
    let e = tool(&key_a, "post_performance", json!({ "post_id": post_a })).unwrap_err();
    assert!(e.contains("Plan limit") && e.contains("Analytics") && e.contains("the Pro plan"), "{e}");
    let e = tool(&key_a, "create_automation", json!({ "account_id": acc_a })).unwrap_err();
    assert!(e.contains("Plan limit") && e.contains("Auto-replies"), "{e}");
    if let Err(e) = tool(&key_a, "best_times", json!({ "account_id": acc_a })) {
        assert!(!e.contains("Plan limit"), "best_times is on every plan: {e}");
    }
    siar::control::put_plan(&json!({ "id": "tiny", "name": "Tiny", "max_accounts": 1, "max_posts_month": 1, "max_storage_mb": 1, "max_upload_mb": 1, "features": ["analytics", "automations"] }))
        .unwrap();
    if let Err(e) = tool(&key_a, "post_performance", json!({ "post_id": post_a })) {
        assert!(!e.contains("Plan limit"), "the plan has analytics now: {e}");
    }
    siar::control::put_plan(&json!({ "id": "tiny", "name": "Tiny", "max_accounts": 1, "max_posts_month": 1, "max_storage_mb": 1, "max_upload_mb": 1 }))
        .unwrap();

    // ---------- DOKU notifications: signature, amount, idempotency, prepaid period ----------
    // QRIS on Rp16.000: Rp1.000 + 11% VAT = Rp1.110 admin fee.
    let q = siar::billing::doku_quote(&starter, 1, "QRIS").unwrap();
    assert_eq!((q.net, q.fee, q.total, q.label.as_str()), (16000, 1110, 17110, "QRIS"));
    let va = siar::billing::doku_quote(&starter, 1, "VIRTUAL_ACCOUNT_BCA").unwrap();
    assert_eq!(va.fee, 4995, "Rp4.500 + VAT");
    let ovo = siar::billing::doku_quote(&starter, 1, "EMONEY_OVO").unwrap();
    assert!(ovo.total as f64 - ovo.total as f64 * 0.0318 * 1.11 >= 16000.0, "a percentage fee is grossed up");
    assert!(siar::billing::doku_quote(&starter, 1, "NOPE").is_err());
    siar::control::with(|c| {
        c.execute(
            "INSERT INTO invoices (invoice_number, tenant_id, plan_id, months, amount, currency, status, created_at, net_amount, fee, method) VALUES ('SIAR-INV-1', ?, 'starter', 1, ?, 'IDR', 'pending', ?, 16000, ?, 'QRIS')",
            rusqlite::params![b, q.total, now(), q.fee],
        )?;
        Ok(())
    })
    .unwrap();
    let paid = json!({ "order": { "invoice_number": "SIAR-INV-1", "amount": q.total }, "transaction": { "status": "SUCCESS" } }).to_string();
    assert_eq!(doku_post("req-1", &paid, "wrong-secret").0, 401);
    let wrong_amount = json!({ "order": { "invoice_number": "SIAR-INV-1", "amount": 16000 }, "transaction": { "status": "SUCCESS" } }).to_string();
    assert_eq!(doku_post("req-0", &wrong_amount, "SK-TEST").0, 400);
    assert_eq!(doku_post("req-1", &paid, "SK-TEST").0, 200);
    assert_eq!(doku_post("req-1", &paid, "SK-TEST").0, 200, "duplicate is acknowledged");
    let sub = siar::control::subscription(&b).unwrap().unwrap();
    assert_eq!(
        (sub.plan_id.as_str(), sub.provider.as_str(), sub.status.as_str()),
        ("starter", "doku", "active")
    );
    let end = sub.current_period_end.unwrap();
    assert!(end > now() + 27 * 86_400_000 && end < now() + 32 * 86_400_000, "one month");
    assert_eq!(doku_post("req-2", &paid, "SK-TEST").0, 200);
    assert_eq!(
        siar::control::subscription(&b).unwrap().unwrap().current_period_end,
        Some(end),
        "a paid invoice extends only once"
    );
    assert_eq!(siar::control::effective_plan(&b).unwrap().id, "starter");
    let _ = key_b;

    // ---------- web pages and CSRF ----------
    for p in ["/", "/docs", "/pricing", "/privacy", "/terms", "/login"] {
        let (s, body) = http(siar::net::get(format!("{}{p}", base())));
        assert_eq!(s, 200, "{p}");
        assert!(body.contains("PT Joyo Digitama Solusi") && body.contains("privacy@example.com"), "{p}");
    }
    let (_, privacy) = http(siar::net::get(format!("{}/privacy", base())));
    assert!(privacy.contains("Limited Use") && privacy.contains("YouTube API Services") && privacy.contains("within 30 days"));
    let (_, pricing) = http(siar::net::get(format!("{}/pricing", base())));
    assert!(
        pricing.contains("Pro") && pricing.contains("Rp16.000") && pricing.contains("Biaya admin") && pricing.contains("Rp17.110"),
        "fees shown"
    );
    assert!(pricing.contains("$1.61") && pricing.contains("$0.61 card processing fee"));
    assert!(
        pricing.contains("Basic") && pricing.contains("Pro") && pricing.contains("Rp15.000") && pricing.contains("Rp60.000") && pricing.contains("Rp180.000"),
        "seeded plans"
    );
    assert!(pricing.contains("$3.75") && pricing.contains("$13.37"), "Basic: $3 / $12 net");
    assert!(pricing.contains("$10.17") && pricing.contains("$39.04"), "Pro: $9 / $36 net");
    assert!(pricing.contains("$83.00 less than 12 monthly payments"));
    assert!(pricing.contains("Compare plans") && pricing.contains("plan paid featured"));
    let pro = siar::control::plan("pro").unwrap().unwrap();
    assert!(pro.has_feature("automations") && pro.has_feature("analytics") && pro.max_accounts == 30);
    let basic = siar::control::plan("basic").unwrap().unwrap();
    assert!(!basic.has_feature("analytics") && basic.max_accounts == 10);
    assert!(siar::control::plan("monthly").unwrap().is_none(), "the old single plan became Basic");
    let q = siar::billing::doku_quote(&pro, 12, "QRIS").unwrap();
    assert_eq!(q.net, 180000);
    let (_, landing) = http(siar::net::get(format!("{}/", base())));
    assert!(landing.contains("class=\"session play\""), "the transcript while there is no demo video");
    std::fs::create_dir_all(dir.join("public")).unwrap();
    std::fs::write(dir.join("public/demo.mp4"), b"0123456789").unwrap();
    let (_, landing) = http(siar::net::get(format!("{}/", base())));
    assert!(landing.contains("<video autoplay muted loop playsinline") && landing.contains("/static/demo.mp4"));
    let res = siar::net::get(format!("{}/static/demo.mp4", base()))
        .header("range", "bytes=2-5")
        .send()
        .unwrap();
    assert_eq!(res.status, 206);
    assert_eq!(res.text().unwrap(), "2345");
    assert_eq!(http(siar::net::get(format!("{}/static/demo.mp4", base()))).1, "0123456789");
    assert_eq!(http(siar::net::get(format!("{}/static/control.sqlite", base()))).0, 404);
    assert_eq!(http(siar::net::get(format!("{}/static/..%2fcontrol.sqlite", base()))).0, 404);
    let (s, _) = http(siar::net::get(format!("{}/dashboard", base())).no_redirects());
    assert_eq!(s, 303, "dashboard needs a session");

    let user = siar::web::user_for("github", "42", Some("a@example.com"), "A").unwrap();
    assert_eq!(
        siar::web::user_for("google", "g-42", Some("A@example.com"), "A").unwrap(),
        user,
        "same verified email, same user"
    );
    // Unverified emails never link: the provider helpers drop them, and a sign-in without a
    // verified email is its own account.
    let gh_unverified = siar::web::github_verified_email(&json!([{ "email": "a@example.com", "primary": true, "verified": false }]));
    assert_eq!(gh_unverified, None);
    assert_ne!(siar::web::user_for("github", "43", gh_unverified.as_deref(), "Mallory").unwrap(), user);
    let g_unverified = siar::web::google_verified_email(&json!({ "sub": "g-43", "email": "a@example.com", "email_verified": false }));
    assert_eq!(g_unverified, None);
    assert_ne!(siar::web::user_for("google", "g-43", g_unverified.as_deref(), "Mallory").unwrap(), user);
    let g_verified = siar::web::google_verified_email(&json!({ "sub": "g-44", "email": "A@Example.com", "email_verified": true }));
    assert_eq!(
        siar::web::user_for("google", "g-44", g_verified.as_deref(), "A").unwrap(),
        user,
        "verified, case-insensitive"
    );
    let raw = "ab".repeat(32);
    siar::control::with(|c| {
        c.execute(
            "INSERT INTO sessions (hash, user_id, csrf, created_at, expires_at) VALUES (?, ?, 'csrf-token-1', ?, ?)",
            rusqlite::params![siar::control::sha256_hex(&raw), user, now(), now() + 3_600_000],
        )?;
        Ok(())
    })
    .unwrap();
    let cookie = format!("siar_session={raw}");
    let (s, body) = http(siar::net::get(format!("{}/dashboard", base())).header("cookie", cookie.clone()));
    assert_eq!(s, 200);
    assert!(body.contains("API keys") && body.contains("/oauth/t/") && body.contains("Delete account"));
    let (s, _) = http(
        siar::net::post(format!("{}/dashboard/keys", base()))
            .header("cookie", cookie.clone())
            .form(&[("name", Some("x".into()))]),
    );
    assert_eq!(s, 403, "no CSRF token");
    let (s, _) = http(
        siar::net::post(format!("{}/dashboard/keys", base()))
            .header("cookie", cookie.clone())
            .header("origin", "https://evil.example")
            .form(&[("name", Some("x".into())), ("csrf", Some("csrf-token-1".into()))]),
    );
    assert_eq!(s, 403, "foreign origin");
    let (s, body) = http(
        siar::net::post(format!("{}/dashboard/keys", base()))
            .header("cookie", cookie.clone())
            .form(&[("name", Some("laptop".into())), ("csrf", Some("csrf-token-1".into()))]),
    );
    assert_eq!(s, 200);
    let key_start = body.find("siar_").expect("the new key is shown once");
    let new_key: String = body[key_start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    let mine = siar::control::tenant_of_user(&user).unwrap().unwrap();
    assert_eq!(tool(&new_key, "list_posts", json!({})).unwrap(), json!([]), "the new workspace is empty");
    // Saving a bring-your-own app changes that tenant's redirect URI to its own callback.
    let (s, _) = http(
        siar::net::post(format!("{}/dashboard/apps/THREADS", base()))
            .header("cookie", cookie.clone())
            .no_redirects()
            .form(&[
                ("client_id", Some("my-threads-app".into())),
                ("client_secret", Some("shh".into())),
                ("csrf", Some("csrf-token-1".into())),
            ]),
    );
    assert_eq!(s, 303);
    let c = tool(&new_key, "connect_account", json!({ "platform": "threads" })).unwrap();
    assert!(c["open_this_url"].as_str().unwrap().contains("client_id=my-threads-app"), "{c}");
    assert_eq!(c["callback"], format!("http://localhost:9/oauth/t/{mine}/threads/callback"));
    assert!(
        tool(&key_a, "connect_account", json!({ "platform": "threads" }))
            .unwrap_err()
            .contains("own THREADS app"),
        "A has no app and no shared app"
    );

    // ---------- OAuth for /mcp: metadata, registration, consent, tokens, revocation ----------
    let (s, prm) = http(siar::net::get(format!("{}/.well-known/oauth-protected-resource/mcp", base())));
    assert_eq!(s, 200);
    let prm: Value = serde_json::from_str(&prm).unwrap();
    assert_eq!(prm["resource"], "http://localhost:9/mcp");
    assert_eq!(prm["authorization_servers"][0], "http://localhost:9");
    let (_, asm) = http(siar::net::get(format!("{}/.well-known/oauth-authorization-server", base())));
    let asm: Value = serde_json::from_str(&asm).unwrap();
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(asm["registration_endpoint"], "http://localhost:9/oauth2/register");
    let (s, _) = http(siar::net::post(format!("{}/oauth2/register", base())).json(json!({ "redirect_uris": ["http://evil.example/cb"] })));
    assert_eq!(s, 400, "plain http outside localhost is refused");
    let cb = "https://claude.ai/api/mcp/auth_callback";
    let (s, reg) = http(siar::net::post(format!("{}/oauth2/register", base())).json(json!({ "redirect_uris": [cb], "client_name": "Claude <b>" })));
    assert_eq!(s, 201, "{reg}");
    let reg: Value = serde_json::from_str(&reg).unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    assert_eq!(reg["token_endpoint_auth_method"], "none");
    let verifier = "siar-test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz";
    let challenge = "3gg9_IUZwoQvg_dMdQ5tFis5A_Y6WoIhZ_XIqmVDea4";
    let q = siar::util::query_string(&[
        ("response_type", "code".into()),
        ("client_id", client_id.clone()),
        ("redirect_uri", cb.into()),
        ("code_challenge", challenge.into()),
        ("code_challenge_method", "S256".into()),
        ("state", "st-1".into()),
        ("resource", "http://localhost:9/mcp".into()),
    ]);
    let res = siar::net::get(format!("{}/oauth2/authorize?{q}", base())).no_redirects().send().unwrap();
    assert_eq!(res.status, 303);
    assert!(
        res.header("location").unwrap().starts_with("/login?next=%2Foauth2%2Fauthorize"),
        "sign in first"
    );
    let (s, consent) = http(siar::net::get(format!("{}/oauth2/authorize?{q}", base())).header("cookie", cookie.clone()));
    assert_eq!(s, 200);
    assert!(
        consent.contains("Allow Claude &lt;b&gt; to use your siar account") && consent.contains("claude.ai"),
        "escaped name, redirect host"
    );
    let (s, _) = http(
        siar::net::get(format!("{}/oauth2/authorize?{}", base(), q.replace("auth_callback", "other")))
            .header("cookie", cookie.clone())
            .no_redirects(),
    );
    assert_eq!(s, 400, "an unregistered redirect_uri is shown as an error, not followed");
    let mut fields: Vec<(&str, Option<String>)> = vec![
        ("response_type", Some("code".into())),
        ("client_id", Some(client_id.clone())),
        ("redirect_uri", Some(cb.into())),
        ("code_challenge", Some(challenge.into())),
        ("code_challenge_method", Some("S256".into())),
        ("state", Some("st-1".into())),
        ("decision", Some("allow".into())),
    ];
    let (s, _) = http(
        siar::net::post(format!("{}/oauth2/authorize", base()))
            .header("cookie", cookie.clone())
            .no_redirects()
            .form(&fields),
    );
    assert_eq!(s, 403, "consent needs the CSRF token");
    fields.push(("csrf", Some("csrf-token-1".into())));
    let res = siar::net::post(format!("{}/oauth2/authorize", base()))
        .header("cookie", cookie.clone())
        .no_redirects()
        .form(&fields)
        .send()
        .unwrap();
    let loc = res.header("location").unwrap();
    assert!(loc.starts_with(&format!("{cb}?code=siar_ac_")) && loc.ends_with("&state=st-1"), "{loc}");
    let code: String = loc.split("code=").nth(1).unwrap().chars().take_while(|c| *c != '&').collect();
    let token = |form: Vec<(&str, Option<String>)>| -> (u16, Value) {
        let (s, b) = http(siar::net::post(format!("{}/oauth2/token", base())).form(&form));
        (s, serde_json::from_str(&b).unwrap_or(Value::Null))
    };
    let (s, t) = token(vec![
        ("grant_type", Some("authorization_code".into())),
        ("code", Some(code.clone())),
        ("redirect_uri", Some(cb.into())),
        ("client_id", Some(client_id.clone())),
        ("code_verifier", Some("wrong-verifier-0123456789-abcdefghijklmnopqrstuvwxyz".into())),
    ]);
    assert_eq!((s, t["error"].as_str()), (400, Some("invalid_grant")));
    let (s, t) = token(vec![
        ("grant_type", Some("authorization_code".into())),
        ("code", Some(code.clone())),
        ("redirect_uri", Some(cb.into())),
        ("client_id", Some(client_id.clone())),
        ("code_verifier", Some(verifier.into())),
    ]);
    assert_eq!(s, 200, "{t}");
    let access = t["access_token"].as_str().unwrap().to_string();
    assert_eq!(t["expires_in"], 3600);
    // The token acts as an API key of the workspace that allowed it.
    assert_eq!(tool(&access, "list_posts", json!({})).unwrap(), json!([]));
    assert_eq!(tool(&access, "status", json!({})).unwrap()["plan"]["id"], "tiny");
    let (s, t2) = token(vec![
        ("grant_type", Some("refresh_token".into())),
        ("refresh_token", Some(t["refresh_token"].as_str().unwrap().into())),
        ("client_id", Some(client_id.clone())),
    ]);
    assert_eq!(s, 200);
    let (s, _) = token(vec![
        ("grant_type", Some("refresh_token".into())),
        ("refresh_token", Some(t["refresh_token"].as_str().unwrap().into())),
        ("client_id", Some(client_id.clone())),
    ]);
    assert_eq!(s, 400, "rotated refresh tokens work once");
    let access2 = t2["access_token"].as_str().unwrap().to_string();
    let (_, dash) = http(siar::net::get(format!("{}/dashboard", base())).header("cookie", cookie.clone()));
    assert!(dash.contains("Connected apps") && dash.contains("Claude &lt;b&gt;"));
    let grant = siar::control::with(|c| siar::authz::grants(c, &mine)).unwrap()[0].id.clone();
    let (s, _) = http(
        siar::net::post(format!("{}/dashboard/connected/{grant}/revoke", base()))
            .header("cookie", cookie.clone())
            .no_redirects()
            .form(&[("csrf", Some("csrf-token-1".into()))]),
    );
    assert_eq!(s, 303);
    let (s, _) = http(
        siar::net::post(format!("{}/mcp", base()))
            .bearer(&access2)
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
    );
    assert_eq!(s, 401, "revoked apps are signed out");

    // Prepaid renewal reminders by e-mail: 7 days before, 1 day before, once per period.
    let mails = |calls: &std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>| {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.1 == "/client/v4/accounts/acct123/email/sending/send")
            .cloned()
            .collect::<Vec<_>>()
    };
    let set_end = |end: i64| {
        siar::control::with(|c| {
            c.execute(
                "INSERT INTO subscriptions (tenant_id, plan_id, provider, status, external_id, current_period_end, cancel_at_period_end, updated_at) VALUES (?, 'starter', 'doku', 'active', 'INV', ?, 1, ?)
                 ON CONFLICT (tenant_id) DO UPDATE SET provider = 'doku', status = 'active', current_period_end = excluded.current_period_end",
                rusqlite::params![mine, end, now()],
            )?;
            Ok(())
        })
        .unwrap()
    };
    set_end(now() + 20 * 86_400_000);
    siar::mailer::send_renewal_reminders().unwrap();
    assert!(mails(&fake_calls).is_empty(), "nothing due yet");
    let end = now() + 6 * 86_400_000;
    set_end(end);
    assert_eq!(siar::mailer::send_renewal_reminders().unwrap(), 1);
    assert_eq!(siar::mailer::send_renewal_reminders().unwrap(), 0, "once per period");
    let m = mails(&fake_calls);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].2["to"], "a@example.com");
    assert_eq!(m[0].2["from"], json!({ "name": "siar", "address": "billing@diatur.com" }));
    assert!(m[0].2["subject"].as_str().unwrap().contains("7 days"));
    // A day before the end: the 1-day reminder.
    let end1 = now() + 12 * 3_600_000;
    set_end(end1);
    assert_eq!(siar::mailer::send_renewal_reminders().unwrap(), 1);
    assert!(mails(&fake_calls)[1].2["subject"].as_str().unwrap().contains("tomorrow"));

    // A card subscription is revoked when the account is deleted; a failed call is queued.
    siar::control::with(|c| {
        c.execute(
            "UPDATE subscriptions SET provider = 'polar', status = 'active', external_id = 'sub_del' WHERE tenant_id = ?",
            [&mine],
        )?;
        Ok(())
    })
    .unwrap();
    let (_, dash) = http(siar::net::get(format!("{}/dashboard", base())).header("cookie", cookie.clone()));
    assert!(dash.contains("cancelled immediately, with no refund"));

    // Deleting the account wipes the workspace.
    let (s, _) = http(
        siar::net::post(format!("{}/dashboard/delete", base()))
            .header("cookie", cookie.clone())
            .no_redirects()
            .form(&[("confirm", Some("DELETE".into())), ("csrf", Some("csrf-token-1".into()))]),
    );
    assert_eq!(s, 303);
    assert!(!siar::tenant::db_path(&mine).exists());
    assert!(siar::control::tenant(&mine).unwrap().is_none());
    let (s, _) = http(
        siar::net::post(format!("{}/mcp", base()))
            .bearer(&new_key)
            .json(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
    );
    assert_eq!(s, 401, "keys die with the workspace");
    let revokes = |calls: &std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>| {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == "DELETE" && c.1 == "/v1/subscriptions/sub_del")
            .count()
    };
    assert_eq!(revokes(&fake_calls), 1, "revoked at deletion");
    let queued: i64 =
        siar::control::with(|c| Ok(c.query_row("SELECT COUNT(*) FROM polar_cancellations WHERE subscription_id = 'sub_del'", [], |r| r.get(0))?)).unwrap();
    assert_eq!(queued, 1, "the first call failed (500), so it is queued");
    siar::control::with(|c| Ok(c.execute("UPDATE polar_cancellations SET next_at = 0", [])?)).unwrap();
    siar::billing::retry_polar_revocations().unwrap();
    assert_eq!(revokes(&fake_calls), 2);
    let queued: i64 = siar::control::with(|c| Ok(c.query_row("SELECT COUNT(*) FROM polar_cancellations", [], |r| r.get(0))?)).unwrap();
    assert_eq!(queued, 0, "retried and done");
    let (_, terms) = http(siar::net::get(format!("{}/terms", base())));
    assert!(terms.contains("cancels a card subscription (Polar) immediately"));

    let _ = std::fs::remove_dir_all(&dir);
}
