//! Outgoing e-mail through Cloudflare Email Service (REST: POST
//! /client/v4/accounts/{account_id}/email/sending/send with an API token that has Email Sending:
//! Edit). Disabled when unset: messages are only logged.
//!
//! Env: SIAR_MAIL_FROM ("siar <noreply@siar.diatur.com>" or an address), SIAR_MAIL_CF_ACCOUNT_ID,
//! SIAR_MAIL_CF_API_TOKEN, optional SIAR_MAIL_REPLY_TO, SIAR_MAIL_API_BASE (tests).

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::config::env;
use crate::control;
use crate::util::{iso, now, Result};

pub fn enabled() -> bool {
    ["SIAR_MAIL_FROM", "SIAR_MAIL_CF_ACCOUNT_ID", "SIAR_MAIL_CF_API_TOKEN"]
        .iter()
        .all(|k| env(k).is_some_and(|v| !v.is_empty()))
}

/// `Name <addr>` -> {name, address}; a bare address stays a string.
fn from_field(raw: &str) -> Value {
    match (raw.find('<'), raw.rfind('>')) {
        (Some(a), Some(b)) if a < b => json!({ "name": raw[..a].trim().trim_matches('"'), "address": raw[a + 1..b].trim() }),
        _ => Value::from(raw.trim()),
    }
}

/// The JSON body Cloudflare's send endpoint takes.
pub fn message(from: &str, to: &str, subject: &str, text: &str, html: &str, reply_to: Option<&str>) -> Value {
    let mut m = json!({ "from": from_field(from), "to": to, "subject": subject, "text": text, "html": html });
    if let Some(r) = reply_to.filter(|r| !r.is_empty()) {
        m["reply_to"] = Value::from(r);
    }
    m
}

/// Send, or log only when the mailer is not configured. Ok(true) = handed to Cloudflare.
pub fn send(to: &str, subject: &str, text: &str, html: &str) -> Result<bool> {
    if !enabled() {
        eprintln!("{} [mail] (not configured, log only) to {to}: {subject}", iso(now()));
        return Ok(false);
    }
    let base = env("SIAR_MAIL_API_BASE").unwrap_or_else(|| "https://api.cloudflare.com/client/v4".into());
    let account = env("SIAR_MAIL_CF_ACCOUNT_ID").unwrap_or_default();
    let body = message(
        &env("SIAR_MAIL_FROM").unwrap_or_default(),
        to,
        subject,
        text,
        html,
        env("SIAR_MAIL_REPLY_TO").as_deref(),
    );
    let r = crate::net::post(format!("{base}/accounts/{account}/email/sending/send"))
        .bearer(&env("SIAR_MAIL_CF_API_TOKEN").unwrap_or_default())
        .json(body)
        .get_json()?;
    if r.get("success") == Some(&Value::Bool(false)) {
        return Err(crate::util::Error::Msg(format!(
            "Cloudflare Email: {}",
            r.get("errors").map(|e| e.to_string()).unwrap_or_default()
        )));
    }
    // result: {delivered, queued, permanent_bounces}, each a list of addresses.
    let res = r.get("result").cloned().unwrap_or(Value::Null);
    if res["permanent_bounces"].as_array().is_some_and(|b| !b.is_empty()) {
        return Err(crate::util::Error::Msg(format!("Cloudflare Email: permanent bounce for {to}")));
    }
    eprintln!("{} [mail] sent to {to}: {subject} (cloudflare: {res})", iso(now()));
    Ok(true)
}

// ---------- prepaid renewal reminders (DOKU) ----------

/// Which reminder is due for a period ending at `end`: 7 days before, 1 day before, on expiry
/// (sent up to 3 days after). None outside those windows.
pub fn reminder_kind(end: i64, at: i64) -> Option<&'static str> {
    const DAY: i64 = 86_400_000;
    if at >= end && at < end + 3 * DAY {
        Some("expired")
    } else if at >= end - DAY && at < end {
        Some("1d")
    } else if at >= end - 7 * DAY && at < end - DAY {
        Some("7d")
    } else {
        None
    }
}

fn reminder_text(kind: &str, plan: &str, end: i64) -> (String, String) {
    let date = &iso(end)[..10];
    let url = format!("{}/dashboard#billing", crate::config::config().public_url);
    match kind {
        "expired" => (
            format!("Your siar {plan} plan has ended"),
            format!("Your prepaid siar {plan} plan ended on {date} (UTC). Scheduled posts above the free plan's limits will not be published until you renew.\n\nRenew: {url}\n"),
        ),
        "1d" => (
            format!("Your siar {plan} plan ends tomorrow"),
            format!("Your prepaid siar {plan} plan ends on {date} (UTC). Pay for another period to keep it:\n{url}\n"),
        ),
        _ => (
            format!("Your siar {plan} plan ends in 7 days"),
            format!("Your prepaid siar {plan} plan ends on {date} (UTC). Pay for another period (QRIS, virtual account or e-wallet) to keep it:\n{url}\n"),
        ),
    }
}

/// Send the reminders that are due, once per tenant, kind and period. Run hourly.
pub fn send_renewal_reminders() -> Result<usize> {
    let subs: Vec<(String, String, i64, Option<String>)> = control::with(|c| {
        crate::db::rows(
            c,
            "SELECT s.tenant_id, s.plan_id, s.current_period_end, u.email FROM subscriptions s JOIN tenants t ON t.id = s.tenant_id
             LEFT JOIN users u ON u.id = t.owner_user_id
             WHERE s.provider = 'doku' AND s.status = 'active' AND s.current_period_end BETWEEN ? AND ?",
            params![now() - 3 * 86_400_000, now() + 7 * 86_400_000],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
    })?;
    let mut sent = 0;
    for (tenant, plan_id, end, email) in subs {
        let Some(kind) = reminder_kind(end, now()) else { continue };
        let done: Option<i64> = control::with(|c| {
            Ok(c.query_row(
                "SELECT sent_at FROM mail_log WHERE tenant_id = ? AND kind = ? AND period_end = ?",
                params![tenant, kind, end],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        if done.is_some() {
            continue;
        }
        let plan = control::plan(&plan_id)?.map(|p| p.name).unwrap_or(plan_id);
        let (subject, text) = reminder_text(kind, &plan, end);
        let html = format!("<p>{}</p>", crate::web::esc(&text).replace('\n', "<br>"));
        let r = match email.as_deref().filter(|e| !e.is_empty()) {
            Some(to) => send(to, &subject, &text, &html),
            None => {
                eprintln!("{} [mail] renewal reminder for {tenant} has no email address: {subject}", iso(now()));
                Ok(false)
            }
        };
        match r {
            Ok(delivered) => {
                control::with(|c| {
                    Ok(c.execute(
                        "INSERT OR IGNORE INTO mail_log (tenant_id, kind, period_end, sent_at) VALUES (?, ?, ?, ?)",
                        params![tenant, kind, end, now()],
                    )
                    .map(|_| ())?)
                })?;
                if delivered {
                    sent += 1;
                }
            }
            // Not recorded: retried next run.
            Err(e) => eprintln!("{} [mail] renewal reminder for {tenant} failed: {e}", iso(now())),
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_windows() {
        let end = 100 * 86_400_000;
        let d = 86_400_000;
        assert_eq!(reminder_kind(end, end - 8 * d), None);
        assert_eq!(reminder_kind(end, end - 7 * d), Some("7d"));
        assert_eq!(reminder_kind(end, end - 2 * d), Some("7d"));
        assert_eq!(reminder_kind(end, end - d), Some("1d"));
        assert_eq!(reminder_kind(end, end - 1), Some("1d"));
        assert_eq!(reminder_kind(end, end), Some("expired"));
        assert_eq!(reminder_kind(end, end + 3 * d), None);
    }

    #[test]
    fn cloudflare_message_shape() {
        let m = message("siar <billing@diatur.com>", "a@example.com", "Hi", "t", "<p>t</p>", Some("help@diatur.com"));
        assert_eq!(m["from"], json!({ "name": "siar", "address": "billing@diatur.com" }));
        assert_eq!(m["to"], "a@example.com");
        assert_eq!(m["reply_to"], "help@diatur.com");
        assert_eq!(message("billing@diatur.com", "a@b.c", "s", "t", "h", None)["from"], "billing@diatur.com");
    }
}
