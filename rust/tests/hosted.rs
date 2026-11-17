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

#[test]
fn hosted_mode() {
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
    siar::control::put_plan(&json!({ "id": "pro", "name": "Pro", "price_usd_cents": 100, "polar_product_id": "prod_pro", "max_accounts": 10, "max_posts_month": 100, "max_storage_mb": 100, "max_upload_mb": 10, "prices": [{ "provider": "doku", "months": 1, "amount": 16000, "currency": "IDR" }] })).unwrap();
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

    // ---------- Polar webhooks: signature, idempotency, plan changes ----------
    let ev = json!({ "type": "subscription.active", "data": { "id": "sub_1", "status": "active", "product_id": "prod_pro", "current_period_end": siar::util::iso(now() + 30 * 86_400_000), "metadata": { "tenant_id": a }, "customer": { "external_id": a } } }).to_string();
    assert_eq!(polar_post(&polar_secret, "evt_1", &ev), (200, "applied\n".to_string()));
    assert_eq!(polar_post(&polar_secret, "evt_1", &ev), (200, "duplicate\n".to_string()));
    assert_eq!(polar_post("whsec_d3JvbmcgZW5vdWdoIGtleSBmb3IgdGVzdA==", "evt_2", &ev).0, 401);
    assert_eq!(siar::control::effective_plan(&a).unwrap().id, "pro");
    assert_eq!(siar::control::effective_plan(&b).unwrap().id, "tiny");
    tool(&key_a, "schedule_post", json!({ "content": "second", "targets": [acc_a], "at": at })).expect("pro allows more posts");
    let revoked =
        json!({ "type": "subscription.revoked", "data": { "id": "sub_1", "status": "canceled", "product_id": "prod_pro", "metadata": { "tenant_id": a } } })
            .to_string();
    assert_eq!(polar_post(&polar_secret, "evt_3", &revoked).0, 200);
    assert_eq!(siar::control::effective_plan(&a).unwrap().id, "tiny");

    // ---------- DOKU notifications: signature, amount, idempotency, prepaid period ----------
    siar::control::with(|c| {
        c.execute("INSERT INTO invoices (invoice_number, tenant_id, plan_id, months, amount, currency, status, created_at) VALUES ('SIAR-INV-1', ?, 'pro', 1, 16000, 'IDR', 'pending', ?)", rusqlite::params![b, now()])?;
        Ok(())
    })
    .unwrap();
    let paid = json!({ "order": { "invoice_number": "SIAR-INV-1", "amount": 16000 }, "transaction": { "status": "SUCCESS" } }).to_string();
    assert_eq!(doku_post("req-1", &paid, "wrong-secret").0, 401);
    let wrong_amount = json!({ "order": { "invoice_number": "SIAR-INV-1", "amount": 1 }, "transaction": { "status": "SUCCESS" } }).to_string();
    assert_eq!(doku_post("req-0", &wrong_amount, "SK-TEST").0, 400);
    assert_eq!(doku_post("req-1", &paid, "SK-TEST").0, 200);
    assert_eq!(doku_post("req-1", &paid, "SK-TEST").0, 200, "duplicate is acknowledged");
    let sub = siar::control::subscription(&b).unwrap().unwrap();
    assert_eq!((sub.plan_id.as_str(), sub.provider.as_str(), sub.status.as_str()), ("pro", "doku", "active"));
    let end = sub.current_period_end.unwrap();
    assert!(end > now() + 27 * 86_400_000 && end < now() + 32 * 86_400_000, "one month");
    assert_eq!(doku_post("req-2", &paid, "SK-TEST").0, 200);
    assert_eq!(
        siar::control::subscription(&b).unwrap().unwrap().current_period_end,
        Some(end),
        "a paid invoice extends only once"
    );
    assert_eq!(siar::control::effective_plan(&b).unwrap().id, "pro");
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
    assert!(pricing.contains("Pro") && pricing.contains("IDR 16.000"));
    let (s, _) = http(siar::net::get(format!("{}/dashboard", base())).no_redirects());
    assert_eq!(s, 303, "dashboard needs a session");

    let user = siar::web::user_for("github", "42", Some("a@example.com"), "A").unwrap();
    assert_eq!(
        siar::web::user_for("google", "g-42", Some("A@example.com"), "A").unwrap(),
        user,
        "same verified email, same user"
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

    let _ = std::fs::remove_dir_all(&dir);
}
