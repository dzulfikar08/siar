//! Connected accounts and their sealed tokens (src/accounts.ts).

use rusqlite::params;
use serde_json::Value;

use crate::db::{self, AccountRow, MediaRow};
use crate::media::get_media;
use crate::platforms;
use crate::providers;
use crate::util::{bail, iso, json_or, new_id, now, str_of, Error, Obj, Result};
use crate::vault;

pub fn public_account(a: &AccountRow) -> Value {
    Obj::new()
        .set("id", a.id.clone())
        .set("platform", a.platform.clone())
        .set("name", a.name.clone())
        .set("handle", a.handle.clone())
        .set("status", a.status.clone())
        .set("expires_at", a.expires_at.filter(|x| *x != 0).map(iso))
        .set("meta", json_or(Some(&a.meta), Value::Object(Default::default())))
        .build()
}

/// An account the platform returned after OAuth.
pub struct Connected {
    pub external_id: String,
    pub name: String,
    pub handle: Option<String>,
    pub secret: Value,
    pub meta: Option<Value>,
    pub expires_at: Option<i64>,
}

pub fn save_accounts(platform: &str, accounts: &[Connected]) -> Result<Vec<Value>> {
    if accounts.is_empty() {
        bail!("The platform returned no accounts to connect");
    }
    let mut sealed = vec![];
    for a in accounts {
        sealed.push(vault::seal(&a.secret)?);
    }
    db::tx(false, |c| {
        for (a, secret) in accounts.iter().zip(sealed.iter()) {
            let ts = now();
            c.execute(
                "INSERT INTO accounts (id, platform, name, handle, external_id, secret, meta, expires_at, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?9)
                 ON CONFLICT (platform, external_id) DO UPDATE SET
                   name = excluded.name, handle = excluded.handle, secret = excluded.secret, meta = excluded.meta,
                   expires_at = excluded.expires_at, status = 'active', updated_at = excluded.updated_at",
                params![
                    new_id("acc"),
                    platform,
                    a.name,
                    a.handle,
                    a.external_id,
                    secret,
                    serde_json::to_string(a.meta.as_ref().unwrap_or(&Value::Object(Default::default())))?,
                    a.expires_at,
                    ts
                ],
            )?;
        }
        Ok(())
    })?;
    let mut out = vec![];
    for a in accounts {
        let row = db::with(|c| {
            db::opt_row(
                c,
                &format!("SELECT {} FROM accounts WHERE platform = ? AND external_id = ?", AccountRow::COLS),
                params![platform, a.external_id],
                AccountRow::from,
            )
        })?;
        if let Some(r) = row {
            out.push(public_account(&r));
        }
    }
    Ok(out)
}

/// Find an account by id, "platform:handle", or a handle that is unique across platforms.
pub fn get_account(id: &str) -> Result<AccountRow> {
    db::with(|c| {
        let sel = format!("SELECT {} FROM accounts", AccountRow::COLS);
        if let Some(a) = db::opt_row(c, &format!("{sel} WHERE id = ?"), [id], AccountRow::from)? {
            return Ok(a);
        }
        if let Some((p, h)) = split_platform_handle(id) {
            let q = format!("{sel} WHERE platform = ? AND ltrim(handle, '@') = ? COLLATE NOCASE");
            if let Some(a) = db::opt_row(c, &q, params![p, h], AccountRow::from)? {
                return Ok(a);
            }
        }
        let bare = id.strip_prefix('@').unwrap_or(id);
        let matches = db::rows(c, &format!("{sel} WHERE ltrim(handle, '@') = ? COLLATE NOCASE"), [bare], AccountRow::from)?;
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("one")),
            0 => bail!("Unknown account {id}. Call list_accounts."),
            _ => {
                let opts: Vec<String> = matches
                    .iter()
                    .map(|a| format!("{}:{}", a.platform, a.handle.as_deref().unwrap_or("null")))
                    .collect();
                bail!("\"{id}\" is on several platforms; use {}, or the account id", opts.join(" or "))
            }
        }
    })
}

/// `/^([\w-]+):@?(.+)$/`
fn split_platform_handle(id: &str) -> Option<(&str, &str)> {
    let (p, rest) = id.split_once(':')?;
    if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let h = rest.strip_prefix('@').unwrap_or(rest);
    if h.is_empty() {
        return None;
    }
    Some((p, h))
}

pub fn list_account_rows(platform: Option<&str>) -> Result<Vec<AccountRow>> {
    db::with(|c| match platform {
        Some(p) => db::rows(
            c,
            &format!("SELECT {} FROM accounts WHERE platform = ? ORDER BY platform, name", AccountRow::COLS),
            [p],
            AccountRow::from,
        ),
        None => db::rows(
            c,
            &format!("SELECT {} FROM accounts ORDER BY platform, name", AccountRow::COLS),
            [],
            AccountRow::from,
        ),
    })
}

pub fn list_accounts(platform: Option<&str>) -> Result<Vec<Value>> {
    Ok(list_account_rows(platform)?.iter().map(public_account).collect())
}

pub fn save_secret(account_id: &str, secret: &Value, expires_at: Option<i64>) -> Result<()> {
    let blob = vault::seal(secret)?;
    db::with(|c| {
        c.execute(
            "UPDATE accounts SET secret = ?, expires_at = COALESCE(?, expires_at), status = 'active', updated_at = ? WHERE id = ?",
            params![blob, expires_at, now(), account_id],
        )?;
        Ok(())
    })
}

pub fn mark_account(account_id: &str, status: &str) -> Result<()> {
    db::with(|c| {
        c.execute(
            "UPDATE accounts SET status = ?, updated_at = ? WHERE id = ?",
            params![status, now(), account_id],
        )?;
        Ok(())
    })
}

/// Refresh when the token expires within 10 minutes (or `force`).
pub fn ensure_fresh(a: &AccountRow, force: bool) -> Result<Value> {
    let mut secret = vault::open(&a.secret)?;
    let p = platforms::get(&a.platform)?;
    let expiring = a.expires_at.map(|e| e != 0 && e - now() < 10 * 60_000).unwrap_or(false);
    if p.has("refresh") && p.ported() && (force || expiring) {
        match providers::refresh(&a.platform, &secret) {
            Ok(Some((s, exp))) => {
                secret = s;
                save_secret(&a.id, &secret, exp)?;
            }
            Ok(None) => {}
            Err(e) => {
                mark_account(&a.id, "reauth_required")?;
                bail!("Refreshing {} token for {} failed; reconnect the account. {e}", p.name, a.name);
            }
        }
    }
    Ok(secret)
}

/// What a provider call gets: the account, its fresh secret, and the post's text, media and options.
pub struct Ctx {
    pub account_id: String,
    pub name: String,
    pub handle: Option<String>,
    pub external_id: String,
    pub meta: Value,
    pub secret: Value,
    pub text: String,
    pub media: Vec<MediaRow>,
    pub options: Value,
}

impl Ctx {
    pub fn token(&self) -> String {
        str_of(&self.secret, "access_token").unwrap_or_default().to_string()
    }
    pub fn opt(&self, k: &str) -> Option<&Value> {
        self.options.get(k).filter(|v| !v.is_null())
    }
    pub fn opt_str(&self, k: &str) -> Option<String> {
        self.opt(k).map(crate::util::js_string)
    }
    pub fn meta_str(&self, k: &str) -> String {
        str_of(&self.meta, k).unwrap_or_default().to_string()
    }
}

pub fn to_media(ids: &[String]) -> Result<Vec<MediaRow>> {
    ids.iter().map(|id| get_media(id)).collect()
}

pub fn build_ctx(a: &AccountRow, text: &str, media_ids: &[String], options: Value, force_refresh: bool) -> Result<Ctx> {
    let secret = ensure_fresh(a, force_refresh)?;
    Ok(Ctx {
        account_id: a.id.clone(),
        name: a.name.clone(),
        handle: a.handle.clone(),
        external_id: a.external_id.clone(),
        meta: json_or(Some(&a.meta), Value::Object(Default::default())),
        secret,
        text: text.to_string(),
        media: to_media(media_ids)?,
        options,
    })
}

/// An account, its platform, and a context without post content (reads, comments, metrics).
pub fn ctx_for(account_id: &str) -> Result<(AccountRow, &'static platforms::Platform, Ctx)> {
    let a = get_account(account_id)?;
    let p = platforms::get(&a.platform)?;
    let ctx = build_ctx(&a, "", &[], Value::Object(Default::default()), false)?;
    Ok((a, p, ctx))
}

pub fn disconnect(account_id: &str) -> Result<Value> {
    let a = get_account(account_id)?;
    db::with(|c| {
        c.execute("DELETE FROM accounts WHERE id = ?", [&a.id])?;
        Ok(())
    })?;
    Ok(Obj::new().set("disconnected", a.label()).build())
}

pub fn not_found(id: &str) -> Error {
    Error::Msg(format!("Unknown account {id}. Call list_accounts."))
}
