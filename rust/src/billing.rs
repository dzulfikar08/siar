//! Hosted mode payments: Polar (USD subscriptions, merchant of record) and DOKU Checkout (IDR,
//! prepaid periods). Both only change a subscription from a verified webhook; the redirect back
//! from checkout is never trusted. Webhook event ids are stored so retries are applied once.

use base64::Engine;
use hmac::{Hmac, Mac};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{config, env};
use crate::control::{self, Plan};
use crate::net;
use crate::util::{bail, hex, iso, js_string, now, parse_iso, random_bytes, Error, Result};

type HmacSha256 = Hmac<Sha256>;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ---------- Polar ----------

pub fn polar_enabled() -> bool {
    env("POLAR_ACCESS_TOKEN").is_some_and(|v| !v.is_empty())
}

fn polar_api() -> &'static str {
    if env("POLAR_SERVER").as_deref() == Some("production") {
        "https://api.polar.sh"
    } else {
        "https://sandbox-api.polar.sh"
    }
}

/// A Polar checkout for a plan; returns the URL to send the customer to.
pub fn polar_checkout(tenant_id: &str, plan: &Plan, email: Option<&str>) -> Result<String> {
    let Some(product) = plan.polar_product_id.as_deref().filter(|p| !p.is_empty()) else {
        bail!("The {} plan has no Polar product yet", plan.name);
    };
    let token = env("POLAR_ACCESS_TOKEN").unwrap_or_default();
    let mut body = json!({
        "products": [product],
        "external_customer_id": tenant_id,
        "metadata": { "tenant_id": tenant_id, "plan_id": plan.id },
        "success_url": format!("{}/dashboard?checkout=polar#billing", config().public_url),
    });
    if let Some(e) = email {
        body["customer_email"] = Value::from(e);
    }
    let r = net::post(format!("{}/v1/checkouts/", polar_api())).bearer(&token).json(body).get_json()?;
    match r.get("url").and_then(|u| u.as_str()) {
        Some(u) => Ok(u.to_string()),
        None => bail!("Polar returned no checkout URL"),
    }
}

/// The HMAC keys a Polar secret may stand for: Standard Webhooks secrets (`whsec_` + base64 key,
/// created on or after 8 Sept 2026) and the older Polar HMAC scheme (key = UTF-8 bytes of the
/// whole secret). Polar's SDKs try both, so this does too.
fn polar_keys(secret: &str) -> Vec<Vec<u8>> {
    let mut keys = vec![];
    if let Some(rest) = secret.strip_prefix("whsec_") {
        if let Ok(k) = b64().decode(rest) {
            keys.push(k);
        }
    }
    keys.push(secret.as_bytes().to_vec());
    keys
}

/// Standard Webhooks verification: HMAC-SHA256 over `id.timestamp.body`, base64, in a
/// space-separated `v1,<sig>` list; timestamp within 5 minutes of `now_s`.
pub fn verify_standard_webhook(secret: &str, id: &str, timestamp: &str, body: &[u8], signature_header: &str, now_s: i64) -> bool {
    let Ok(ts) = timestamp.trim().parse::<i64>() else { return false };
    if (now_s - ts).abs() > 300 {
        return false;
    }
    let mut content = format!("{id}.{timestamp}.").into_bytes();
    content.extend_from_slice(body);
    for key in polar_keys(secret) {
        let Ok(mut mac) = HmacSha256::new_from_slice(&key) else { continue };
        mac.update(&content);
        let want = mac.finalize().into_bytes();
        for sig in signature_header.split(' ') {
            let Some(s) = sig.strip_prefix("v1,") else { continue };
            if let Ok(got) = b64().decode(s) {
                if got.len() == want.len() && got.iter().zip(want.iter()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0 {
                    return true;
                }
            }
        }
    }
    false
}

/// Outcome of a webhook, for the HTTP answer.
#[derive(Debug, PartialEq)]
pub enum Hook {
    Applied,
    Duplicate,
    Ignored,
}

/// POST /billing/polar/webhook
pub fn handle_polar(id: &str, timestamp: &str, signature: &str, body: &[u8]) -> Result<Hook> {
    let secret = env("POLAR_WEBHOOK_SECRET").unwrap_or_default();
    if secret.is_empty() || !verify_standard_webhook(&secret, id, timestamp, body, signature, now() / 1000) {
        bail!("Bad signature");
    }
    let ev: Value = serde_json::from_slice(body)?;
    let kind = ev.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if !control::first_event("polar", id, &kind)? {
        return Ok(Hook::Duplicate);
    }
    let r = apply_polar(&kind, ev.get("data").unwrap_or(&Value::Null));
    if r.is_err() {
        // Let Polar retry.
        control::forget_event("polar", id)?;
    }
    r
}

fn apply_polar(kind: &str, d: &Value) -> Result<Hook> {
    if !kind.starts_with("subscription.") {
        return Ok(Hook::Ignored);
    }
    let s = |p: &str| d.pointer(p).and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(|v| v.to_string());
    let Some(tenant) = s("/metadata/tenant_id").or_else(|| s("/customer/external_id")) else {
        bail!("No tenant on the subscription")
    };
    if control::tenant(&tenant)?.is_none() {
        return Ok(Hook::Ignored);
    }
    let product = s("/product_id").or_else(|| s("/product/id"));
    let plan_id = match product {
        Some(p) => control::with(|c| {
            Ok(c.query_row("SELECT id FROM plans WHERE polar_product_id = ?", [&p], |r| r.get::<_, String>(0))
                .optional()?)
        })?,
        None => None,
    }
    .or_else(|| s("/metadata/plan_id"));
    let Some(plan_id) = plan_id else { bail!("Unknown Polar product") };
    let status = match kind {
        "subscription.revoked" => "revoked".to_string(),
        _ => s("/status").unwrap_or_else(|| "active".into()),
    };
    let end = s("/current_period_end").and_then(|t| parse_iso(&t));
    let cancel = d.get("cancel_at_period_end").and_then(|v| v.as_bool()).unwrap_or(false) || kind == "subscription.canceled";
    let status = if cancel && status == "active" { "canceled".to_string() } else { status };
    control::with(|c| {
        c.execute(
            "INSERT INTO subscriptions (tenant_id, plan_id, provider, status, external_id, current_period_end, cancel_at_period_end, updated_at)
             VALUES (?, ?, 'polar', ?, ?, ?, ?, ?)
             ON CONFLICT (tenant_id) DO UPDATE SET plan_id = excluded.plan_id, provider = 'polar', status = excluded.status,
               external_id = excluded.external_id, current_period_end = excluded.current_period_end,
               cancel_at_period_end = excluded.cancel_at_period_end, updated_at = excluded.updated_at",
            params![tenant, plan_id, status, s("/id"), end, cancel as i64, now()],
        )?;
        Ok(())
    })?;
    Ok(Hook::Applied)
}

// ---------- DOKU Checkout (non-SNAP signature) ----------

pub fn doku_enabled() -> bool {
    env("DOKU_CLIENT_ID").is_some_and(|v| !v.is_empty()) && env("DOKU_SECRET_KEY").is_some_and(|v| !v.is_empty())
}

fn doku_api() -> &'static str {
    if env("DOKU_SERVER").as_deref() == Some("production") {
        "https://api.doku.com"
    } else {
        "https://api-sandbox.doku.com"
    }
}

/// base64(SHA-256(body)), the Digest component.
pub fn doku_digest(body: &[u8]) -> String {
    b64().encode(Sha256::digest(body))
}

/// The string DOKU signs: Client-Id, Request-Id, Request-Timestamp, Request-Target and (for
/// requests with a body) Digest, as `Name:value` lines joined by `\n`.
pub fn doku_component(client_id: &str, request_id: &str, timestamp: &str, target: &str, body: Option<&[u8]>) -> String {
    let mut s = format!("Client-Id:{client_id}\nRequest-Id:{request_id}\nRequest-Timestamp:{timestamp}\nRequest-Target:{target}");
    if let Some(b) = body {
        s.push_str(&format!("\nDigest:{}", doku_digest(b)));
    }
    s
}

/// `HMACSHA256=` + base64(HMAC-SHA256(secret, component)).
pub fn doku_signature(secret: &str, component: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(component.as_bytes());
    format!("HMACSHA256={}", b64().encode(mac.finalize().into_bytes()))
}

/// Compare a received Signature header in constant time; padding on the base64 is optional
/// (DOKU's own samples show both).
pub fn doku_verify(secret: &str, component: &str, header: &str) -> bool {
    let want = doku_signature(secret, component);
    let norm = |s: &str| s.trim().trim_end_matches('=').to_string();
    control::ct_eq(&norm(&want), &norm(header))
}

/// A DOKU Checkout for `months` of a plan in IDR; returns the payment page URL.
pub fn doku_checkout(tenant_id: &str, plan: &Plan, months: i64, email: Option<&str>) -> Result<String> {
    let Some((_, amount, currency)) = control::plan_prices(&plan.id, "doku")?.into_iter().find(|p| p.0 == months) else {
        bail!("No IDR price for {} month(s) of the {} plan", months, plan.name);
    };
    let invoice = format!("SIAR-{tenant_id}-{}", &hex(&random_bytes(6)));
    let body = json!({
        "order": {
            "amount": amount,
            "invoice_number": invoice,
            "currency": currency,
            "callback_url": format!("{}/dashboard?checkout=doku#billing", config().public_url),
            "line_items": [{ "name": format!("siar {} ({} month{})", plan.name, months, if months == 1 { "" } else { "s" }), "price": amount, "quantity": 1 }],
        },
        "payment": { "payment_due_date": 60 },
        "customer": { "id": tenant_id, "email": email.unwrap_or("") },
    })
    .to_string();
    let client_id = env("DOKU_CLIENT_ID").unwrap_or_default();
    let request_id = uuid4();
    let ts = iso(now())[..19].to_string() + "Z";
    let target = "/checkout/v1/payment";
    let sig = doku_signature(
        &env("DOKU_SECRET_KEY").unwrap_or_default(),
        &doku_component(&client_id, &request_id, &ts, target, Some(body.as_bytes())),
    );
    control::with(|c| {
        c.execute(
            "INSERT INTO invoices (invoice_number, tenant_id, plan_id, months, amount, currency, status, created_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
            params![invoice, tenant_id, plan.id, months, amount, currency, now()],
        )?;
        Ok(())
    })?;
    let r = net::post(format!("{}{target}", doku_api()))
        .header("Client-Id", client_id)
        .header("Request-Id", request_id)
        .header("Request-Timestamp", ts)
        .header("Signature", sig)
        .header("content-type", "application/json")
        .body(net::Body::Bytes(body.into_bytes()))
        .get_json()?;
    match r.pointer("/response/payment/url").and_then(|u| u.as_str()) {
        Some(u) => Ok(u.to_string()),
        None => bail!("DOKU returned no payment URL"),
    }
}

fn uuid4() -> String {
    let h = hex(&random_bytes(16));
    format!("{}-{}-4{}-a{}-{}", &h[..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

/// POST /billing/doku/notify. `target` is the path DOKU called (the Request-Target it signed).
pub fn handle_doku(client_id: &str, request_id: &str, timestamp: &str, signature: &str, target: &str, body: &[u8]) -> Result<Hook> {
    let secret = env("DOKU_SECRET_KEY").unwrap_or_default();
    let mine = env("DOKU_CLIENT_ID").unwrap_or_default();
    if secret.is_empty() || client_id != mine || !doku_verify(&secret, &doku_component(client_id, request_id, timestamp, target, Some(body)), signature) {
        bail!("Bad signature");
    }
    let ev: Value = serde_json::from_slice(body)?;
    let invoice = ev.pointer("/order/invoice_number").map(js_string).unwrap_or_default();
    let status = ev.pointer("/transaction/status").map(js_string).unwrap_or_default();
    // Request-Id identifies the notification; DOKU resends it with the same id.
    if !control::first_event("doku", &format!("{request_id}:{invoice}:{status}"), &status)? {
        return Ok(Hook::Duplicate);
    }
    let r = apply_doku(&invoice, &status, ev.pointer("/order/amount"));
    if r.is_err() {
        control::forget_event("doku", &format!("{request_id}:{invoice}:{status}"))?;
    }
    r
}

/// `months` calendar months after `from` (UTC).
pub fn add_months(from: i64, months: i64) -> i64 {
    let ts = jiff::Timestamp::from_millisecond(from).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let z = ts.to_zoned(jiff::tz::TimeZone::UTC);
    z.checked_add(jiff::Span::new().months(months))
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or(from + months * 30 * 86_400_000)
}

fn apply_doku(invoice: &str, status: &str, amount: Option<&Value>) -> Result<Hook> {
    if status != "SUCCESS" {
        if !invoice.is_empty() {
            control::with(|c| {
                Ok(c.execute(
                    "UPDATE invoices SET status = ? WHERE invoice_number = ? AND status = 'pending'",
                    params![status.to_lowercase(), invoice],
                )
                .map(|_| ())?)
            })?;
        }
        return Ok(Hook::Ignored);
    }
    let row: Option<(String, String, i64, i64, String)> = control::with(|c| {
        Ok(c.query_row(
            "SELECT tenant_id, plan_id, months, amount, status FROM invoices WHERE invoice_number = ?",
            [invoice],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?)
    })?;
    let Some((tenant, plan_id, months, want, inv_status)) = row else {
        bail!("Unknown invoice {invoice}")
    };
    if inv_status == "paid" {
        return Ok(Hook::Duplicate);
    }
    let paid = amount.map(|a| crate::util::to_num(Some(a)) as i64).unwrap_or(-1);
    if paid != want {
        bail!("Amount {paid} does not match invoice {invoice} ({want})");
    }
    let current = control::subscription(&tenant)?;
    let base = match &current {
        Some(s) if s.provider == "doku" && s.plan_id == plan_id && s.status == "active" => s.current_period_end.unwrap_or(0).max(now()),
        _ => now(),
    };
    let end = add_months(base, months);
    control::with(|c| {
        c.execute(
            "UPDATE invoices SET status = 'paid', paid_at = ? WHERE invoice_number = ?",
            params![now(), invoice],
        )?;
        c.execute(
            "INSERT INTO subscriptions (tenant_id, plan_id, provider, status, external_id, current_period_end, cancel_at_period_end, updated_at)
             VALUES (?, ?, 'doku', 'active', ?, ?, 1, ?)
             ON CONFLICT (tenant_id) DO UPDATE SET plan_id = excluded.plan_id, provider = 'doku', status = 'active', external_id = excluded.external_id,
               current_period_end = excluded.current_period_end, cancel_at_period_end = 1, updated_at = excluded.updated_at",
            params![tenant, plan_id, invoice, end, now()],
        )?;
        Ok(())
    })?;
    Ok(Hook::Applied)
}

/// Prepaid (DOKU) subscriptions ending within `days`: (tenant, plan, end). Shown in the dashboard
/// and logged daily as renewal reminders.
pub fn renewals_due(days: i64) -> Result<Vec<(String, String, i64)>> {
    control::with(|c| {
        crate::db::rows(
            c,
            "SELECT tenant_id, plan_id, current_period_end FROM subscriptions WHERE provider = 'doku' AND status = 'active' AND current_period_end BETWEEN ? AND ?",
            params![now(), now() + days * 86_400_000],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    })
}

pub fn error_status(e: &Error) -> u16 {
    if e.to_string() == "Bad signature" {
        401
    } else {
        400
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_webhooks_documented_vector() {
        // From the Standard Webhooks specification's examples.
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let body = br#"{"test": 2432232314}"#;
        let sig = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
        assert!(verify_standard_webhook(
            secret,
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            "1614265330",
            body,
            sig,
            1614265330
        ));
        assert!(verify_standard_webhook(
            secret,
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            "1614265330",
            body,
            &format!("v1,bogus {sig}"),
            1614265330 + 60
        ));
        assert!(
            !verify_standard_webhook(secret, "msg_p5jXN8AQM9LWM0D4loKWxJek", "1614265330", body, sig, 1614265330 + 301),
            "too old"
        );
        assert!(
            !verify_standard_webhook(
                secret,
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                "1614265330",
                br#"{"test": 2432232315}"#,
                sig,
                1614265330
            ),
            "tampered"
        );
        assert!(!verify_standard_webhook(secret, "msg_other", "1614265330", body, sig, 1614265330), "other id");
    }

    #[test]
    fn polar_legacy_secret_uses_utf8_bytes() {
        // Older Polar secrets: the HMAC key is the UTF-8 bytes of the whole secret string.
        let secret = "polar_whs_legacy_example";
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"evt_1.1700000000.{}");
        let sig = format!("v1,{}", b64().encode(mac.finalize().into_bytes()));
        assert!(verify_standard_webhook(secret, "evt_1", "1700000000", b"{}", &sig, 1700000000));
    }

    #[test]
    fn doku_component_matches_the_documented_layout() {
        // The component string shown in DOKU's "Generate Signature" docs (non-SNAP).
        let body = br#"{"order":{"invoice_number":"INV-1","amount":10000}}"#;
        let c = doku_component(
            "MCH-0001-10791114622547",
            "cc682442-6c22-493e-8121-b9ef6b3fa728",
            "2020-08-11T08:45:42Z",
            "/doku-virtual-account/v2/payment-code",
            Some(body),
        );
        let lines: Vec<&str> = c.split('\n').collect();
        assert_eq!(lines[0], "Client-Id:MCH-0001-10791114622547");
        assert_eq!(lines[1], "Request-Id:cc682442-6c22-493e-8121-b9ef6b3fa728");
        assert_eq!(lines[2], "Request-Timestamp:2020-08-11T08:45:42Z");
        assert_eq!(lines[3], "Request-Target:/doku-virtual-account/v2/payment-code");
        assert_eq!(lines[4], format!("Digest:{}", doku_digest(body)));
        assert_eq!(lines.len(), 5);
        // Without a body (GET) there is no Digest line.
        assert_eq!(doku_component("a", "b", "c", "/d", None).split('\n').count(), 4);
        // SHA-256 of "" in base64, as DOKU's Digest would be for an empty body.
        assert_eq!(doku_digest(b""), "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
    }

    #[test]
    fn doku_signature_round_trip_and_padding() {
        let c = doku_component("MCH-1", "req-1", "2020-10-21T03:38:28Z", "/billing/doku/notify", Some(b"{}"));
        let sig = doku_signature("secret-key-from-jokul-back-office", &c);
        assert!(sig.starts_with("HMACSHA256=") && sig.ends_with('='));
        assert!(doku_verify("secret-key-from-jokul-back-office", &c, &sig));
        assert!(
            doku_verify("secret-key-from-jokul-back-office", &c, sig.trim_end_matches('=')),
            "unpadded, as in DOKU's QRIS sample"
        );
        assert!(!doku_verify("another-secret", &c, &sig));
    }

    #[test]
    fn months_are_calendar_months() {
        let jan31 = parse_iso("2026-01-31T00:00:00Z").unwrap();
        assert_eq!(iso(add_months(jan31, 1)), "2026-02-28T00:00:00.000Z");
        assert_eq!(iso(add_months(jan31, 12)), "2027-01-31T00:00:00.000Z");
    }
}
