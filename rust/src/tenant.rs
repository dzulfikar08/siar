//! Hosted mode: the tenant context. Every request and job runs "inside" one tenant: db::with uses
//! that tenant's SQLite file, the vault uses its data key, media go to its directory, and OAuth
//! uses its own app credentials (or the operator's shared apps). The context is thread-local; code
//! that spawns threads carries it over with `current()` + `enter()`.
//!
//! Tenant databases are opened lazily and kept in a small LRU; idle ones are closed, so memory
//! follows active tenants, not the total.

use base64::Engine;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::config::{config, env};
use crate::control;
use crate::util::{bail, now, random_bytes, Error, Result};

pub struct Tenant {
    pub id: String,
    conn: Mutex<Connection>,
    key: [u8; 32],
    last_used: AtomicI64,
}

impl Tenant {
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }
}

pub fn hosted() -> bool {
    env("SIAR_MODE").as_deref() == Some("hosted")
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Tenant>>> = const { RefCell::new(None) };
}

pub fn current() -> Option<Arc<Tenant>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub fn current_id() -> Option<String> {
    current().map(|t| t.id.clone())
}

/// Leaves the tenant context (restoring the previous one) when dropped.
pub struct Guard(Option<Arc<Tenant>>);

impl Drop for Guard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

pub fn enter(t: Arc<Tenant>) -> Guard {
    t.last_used.store(now(), Ordering::Relaxed);
    let prev = CURRENT.with(|c| c.borrow_mut().replace(t));
    Guard(prev)
}

/// Enter `t` if given (for threads spawned from a tenant context).
pub fn enter_opt(t: Option<Arc<Tenant>>) -> Option<Guard> {
    t.map(enter)
}

// ---------- envelope encryption ----------

pub fn new_data_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(&random_bytes(32));
    k
}

fn master() -> Result<[u8; 32]> {
    match &config().vault_key {
        Some(raw) => crate::vault::parse_key(raw),
        None => bail!("SIAR_VAULT_KEY is not set. Generate one with: siar keygen"),
    }
}

pub fn seal_data_key_with(master: &[u8; 32], key: &[u8; 32]) -> Result<Vec<u8>> {
    crate::vault::seal_with(master, &json!({ "k": base64::engine::general_purpose::STANDARD.encode(key) }))
}

pub fn open_data_key_with(master: &[u8; 32], blob: &[u8]) -> Result<[u8; 32]> {
    let v = crate::vault::open_with(master, blob)?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(v.get("k").and_then(|k| k.as_str()).unwrap_or(""))
        .map_err(|_| Error::Msg("Bad tenant key".into()))?;
    <[u8; 32]>::try_from(raw.as_slice()).map_err(|_| Error::Msg("Bad tenant key".into()))
}

pub fn seal_data_key(key: &[u8; 32]) -> Result<Vec<u8>> {
    seal_data_key_with(&master()?, key)
}

// ---------- paths ----------

fn tenants_dir() -> PathBuf {
    config().data_dir.join("tenants")
}

pub fn db_path(id: &str) -> PathBuf {
    tenants_dir().join(format!("{id}.sqlite"))
}

pub fn media_dir_of(id: &str) -> PathBuf {
    tenants_dir().join(id).join("media")
}

/// Media directory of the current tenant, or the single-tenant one.
pub fn media_dir() -> PathBuf {
    match current() {
        Some(t) => media_dir_of(&t.id),
        None => config().media_dir.clone(),
    }
}

/// URL path segment for local media: `/media/<key>` or `/media/t/<tenant>/<key>`.
pub fn media_url_prefix() -> String {
    match current() {
        Some(t) => format!("{}/media/t/{}", config().public_url, t.id),
        None => format!("{}/media", config().public_url),
    }
}

/// R2 object prefix: tenants keep their objects under `tenants/<id>/`.
pub fn r2_prefix() -> String {
    match current() {
        Some(t) => format!("tenants/{}/", t.id),
        None => String::new(),
    }
}

// ---------- cache ----------

fn cache() -> &'static Mutex<HashMap<String, Arc<Tenant>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<Tenant>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn capacity() -> usize {
    env("SIAR_TENANT_CACHE").and_then(|v| v.parse().ok()).unwrap_or(64)
}

/// Open (or reuse) a tenant. Unknown or deleted tenants are an error.
pub fn get(id: &str) -> Result<Arc<Tenant>> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) || id.is_empty() || id.len() > 40 {
        bail!("Unknown tenant");
    }
    if let Some(t) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(id) {
        t.last_used.store(now(), Ordering::Relaxed);
        return Ok(t.clone());
    }
    let Some(row) = control::tenant(id)? else { bail!("Unknown tenant") };
    if row.status != "active" {
        bail!("This account is {}", row.status);
    }
    let key = open_data_key_with(&master()?, &row.data_key)?;
    std::fs::create_dir_all(tenants_dir())?;
    let conn = crate::db::open_at(&db_path(id))?;
    // Bound each tenant's page cache (default is 2 MB) so many open tenants stay small.
    let kib = env("SIAR_TENANT_CACHE_KIB").and_then(|v| v.parse::<i64>().ok()).unwrap_or(512);
    conn.execute_batch(&format!("PRAGMA cache_size = -{kib};"))?;
    // Fewer cached prepared statements per tenant: they are most of an open tenant's memory.
    conn.set_prepared_statement_cache_capacity(env("SIAR_TENANT_STMT_CACHE").and_then(|v| v.parse().ok()).unwrap_or(16));
    let t = Arc::new(Tenant {
        id: id.to_string(),
        conn: Mutex::new(conn),
        key,
        last_used: AtomicI64::new(now()),
    });
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    let t = c.entry(id.to_string()).or_insert(t).clone();
    if c.len() > capacity() {
        evict(&mut c, capacity());
    }
    Ok(t)
}

/// Drop least recently used tenants that no request or job is using, down to `keep`.
fn evict(c: &mut HashMap<String, Arc<Tenant>>, keep: usize) {
    let mut idle: Vec<(i64, String)> = c
        .iter()
        .filter(|(_, t)| Arc::strong_count(t) == 1)
        .map(|(k, t)| (t.last_used.load(Ordering::Relaxed), k.clone()))
        .collect();
    idle.sort();
    let excess = c.len().saturating_sub(keep);
    for (_, k) in idle.into_iter().take(excess) {
        c.remove(&k);
    }
}

/// Close tenants idle for longer than SIAR_TENANT_IDLE_SECS (default 600).
pub fn close_idle() {
    let max_idle = env("SIAR_TENANT_IDLE_SECS").and_then(|v| v.parse::<i64>().ok()).unwrap_or(600) * 1000;
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = now() - max_idle;
    c.retain(|_, t| Arc::strong_count(t) > 1 || t.last_used.load(Ordering::Relaxed) > cutoff);
}

pub fn open_count() -> usize {
    cache().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Forget a tenant (account deletion): close its connection.
pub fn forget(id: &str) {
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(id);
}

// ---------- per-tenant app credentials (bring your own app) ----------

/// The tenant's own OAuth app for an env prefix (e.g. "THREADS"), decrypted with its data key.
pub fn byo_creds(prefix: &str) -> Result<Option<Value>> {
    let Some(t) = current() else { return Ok(None) };
    let sealed: Option<Vec<u8>> = control::with(|c| {
        Ok(rusqlite::OptionalExtension::optional(c.query_row(
            "SELECT sealed FROM app_credentials WHERE tenant_id = ? AND prefix = ?",
            rusqlite::params![t.id, prefix],
            |r| r.get(0),
        ))?)
    })?;
    match sealed {
        Some(b) => Ok(Some(crate::vault::open_with(t.key(), &b)?)),
        None => Ok(None),
    }
}

pub fn save_byo_creds(prefix: &str, v: &Value) -> Result<()> {
    let Some(t) = current() else { bail!("No tenant") };
    let sealed = crate::vault::seal_with(t.key(), v)?;
    control::with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO app_credentials (tenant_id, prefix, sealed, updated_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![t.id, prefix, sealed, now()],
        )?;
        Ok(())
    })
}

pub fn delete_byo_creds(prefix: &str) -> Result<()> {
    let Some(t) = current() else { bail!("No tenant") };
    control::with(|c| {
        Ok(c.execute(
            "DELETE FROM app_credentials WHERE tenant_id = ? AND prefix = ?",
            rusqlite::params![t.id, prefix],
        )
        .map(|_| ())?)
    })
}

/// Shared operator apps switched on for everyone: SIAR_SHARED_APPS=THREADS,INSTAGRAM,...
pub fn shared_app_enabled(prefix: &str) -> bool {
    env("SIAR_SHARED_APPS")
        .map(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case(prefix)))
        .unwrap_or(false)
}

/// Whether OAuth for this prefix uses the tenant's own app (then the callback carries the tenant).
pub fn uses_byo(prefix: &str) -> bool {
    current().is_some() && byo_creds(prefix).ok().flatten().is_some()
}

// ---------- scheduler index ----------

/// Recompute this tenant's entry in the control "due" index (call after changes and runs).
pub fn refresh_due() -> Result<()> {
    let Some(t) = current() else { return Ok(()) };
    let (post, job, engage, published): (Option<i64>, Option<i64>, i64, Option<i64>) = crate::db::with(|c| {
        let post = c.query_row(
            "SELECT MIN(t.next_attempt_at) FROM targets t JOIN posts p ON p.id = t.post_id WHERE t.status = 'pending' AND p.status NOT IN ('draft', 'cancelled') AND t.next_attempt_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let job = c.query_row("SELECT MIN(run_at) FROM jobs WHERE status = 'pending'", [], |r| r.get(0))?;
        let engage = c.query_row("SELECT COUNT(*) FROM automations WHERE enabled = 1", [], |r| r.get(0))?;
        let published = c.query_row("SELECT MAX(published_at) FROM targets WHERE status = 'published'", [], |r| r.get(0))?;
        Ok((post, job, engage, published))
    })?;
    control::set_due(&t.id, post, job, engage > 0 || env("SIAR_COMMENT_SYNC").as_deref() == Some("all"), published)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let master = [9u8; 32];
        let key = new_data_key();
        let sealed = seal_data_key_with(&master, &key).unwrap();
        assert_eq!(open_data_key_with(&master, &sealed).unwrap(), key);
        assert!(open_data_key_with(&[8u8; 32], &sealed).is_err(), "another master key cannot open it");
        // A tenant's secret sealed with its data key is unreadable with another tenant's key.
        let secret = json!({ "access_token": "t" });
        let blob = crate::vault::seal_with(&key, &secret).unwrap();
        assert_eq!(crate::vault::open_with(&key, &blob).unwrap(), secret);
        assert!(crate::vault::open_with(&new_data_key(), &blob).is_err());
    }
}
