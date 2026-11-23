//! One SQLite connection behind a mutex. Same schema and migrations as the former Bun
//! version, so it opens that version's database as is. Never hold the lock across network calls.

use rusqlite::{Connection, OptionalExtension, Row};
use std::sync::{Mutex, OnceLock};

use crate::util::Result;

const MIGRATIONS: [&str; 2] = [
    "CREATE TABLE accounts (
     id TEXT PRIMARY KEY,
     platform TEXT NOT NULL,
     name TEXT NOT NULL,
     handle TEXT,
     external_id TEXT NOT NULL,
     secret BLOB NOT NULL,
     meta TEXT NOT NULL DEFAULT '{}',
     expires_at INTEGER,
     status TEXT NOT NULL DEFAULT 'active',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL,
     UNIQUE (platform, external_id)
   );
   CREATE TABLE media (
     id TEXT PRIMARY KEY,
     filename TEXT NOT NULL,
     mime TEXT NOT NULL,
     size INTEGER NOT NULL,
     storage TEXT NOT NULL,
     key TEXT NOT NULL,
     url TEXT NOT NULL,
     alt TEXT,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE posts (
     id TEXT PRIMARY KEY,
     status TEXT NOT NULL,
     content TEXT NOT NULL,
     media TEXT NOT NULL DEFAULT '[]',
     options TEXT NOT NULL DEFAULT '{}',
     scheduled_at INTEGER,
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   CREATE TABLE targets (
     id TEXT PRIMARY KEY,
     post_id TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     content TEXT,
     media TEXT,
     options TEXT NOT NULL DEFAULT '{}',
     status TEXT NOT NULL DEFAULT 'pending',
     attempts INTEGER NOT NULL DEFAULT 0,
     next_attempt_at INTEGER,
     remote_id TEXT,
     remote_url TEXT,
     error TEXT,
     published_at INTEGER
   );
   CREATE INDEX targets_due ON targets (status, next_attempt_at);
   CREATE INDEX posts_sched ON posts (status, scheduled_at);
   CREATE TABLE oauth_states (
     state TEXT PRIMARY KEY,
     platform TEXT NOT NULL,
     verifier TEXT NOT NULL,
     data TEXT NOT NULL DEFAULT '{}',
     created_at INTEGER NOT NULL
   );
   CREATE TABLE metrics (
     target_id TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
     fetched_at INTEGER NOT NULL,
     data TEXT NOT NULL,
     PRIMARY KEY (target_id, fetched_at)
   );",
    // 2: engagement, automations, ideas, insights cache, click links, job queue
    "CREATE TABLE comments (
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     remote_id TEXT NOT NULL,
     remote_post_id TEXT NOT NULL,
     parent_id TEXT,
     author_id TEXT,
     username TEXT,
     text TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     hidden INTEGER NOT NULL DEFAULT 0,
     status TEXT NOT NULL DEFAULT 'unreplied',
     reply_text TEXT,
     replied_at INTEGER,
     fetched_at INTEGER NOT NULL,
     PRIMARY KEY (account_id, remote_id)
   );
   CREATE INDEX comments_status ON comments (status, created_at);
   CREATE TABLE account_posts (
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     remote_id TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     type TEXT,
     url TEXT,
     text TEXT,
     metrics TEXT NOT NULL DEFAULT '{}',
     fetched_at INTEGER NOT NULL,
     PRIMARY KEY (account_id, remote_id)
   );
   CREATE TABLE ideas (
     id TEXT PRIMARY KEY,
     title TEXT NOT NULL,
     body TEXT NOT NULL DEFAULT '',
     tags TEXT NOT NULL DEFAULT '[]',
     media TEXT NOT NULL DEFAULT '[]',
     status TEXT NOT NULL DEFAULT 'open',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   ALTER TABLE posts ADD COLUMN idea_id TEXT REFERENCES ideas(id) ON DELETE SET NULL;
   CREATE TABLE automations (
     id TEXT PRIMARY KEY,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     name TEXT NOT NULL,
     kind TEXT NOT NULL,
     enabled INTEGER NOT NULL DEFAULT 1,
     config TEXT NOT NULL,
     stats TEXT NOT NULL DEFAULT '{}',
     created_at INTEGER NOT NULL,
     updated_at INTEGER NOT NULL
   );
   CREATE TABLE automation_marks (
     automation_id TEXT NOT NULL REFERENCES automations(id) ON DELETE CASCADE,
     key TEXT NOT NULL,
     created_at INTEGER NOT NULL,
     PRIMARY KEY (automation_id, key)
   );
   CREATE TABLE jobs (
     id TEXT PRIMARY KEY,
     run_at INTEGER NOT NULL,
     type TEXT NOT NULL,
     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
     automation_id TEXT,
     payload TEXT NOT NULL,
     status TEXT NOT NULL DEFAULT 'pending',
     attempts INTEGER NOT NULL DEFAULT 0,
     error TEXT,
     done_at INTEGER
   );
   CREATE INDEX jobs_due ON jobs (status, run_at);
   CREATE TABLE links (
     code TEXT PRIMARY KEY,
     url TEXT NOT NULL,
     automation_id TEXT,
     recipient TEXT,
     clicks INTEGER NOT NULL DEFAULT 0,
     first_click_at INTEGER,
     created_at INTEGER NOT NULL
   );
   CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
];

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

pub fn open_at(path: &std::path::Path) -> Result<Connection> {
    let mut c = Connection::open(path)?;
    c.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
    migrate(&mut c)?;
    c.set_prepared_statement_cache_capacity(64);
    Ok(c)
}

pub fn migrate(c: &mut Connection) -> Result<()> {
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, m) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = c.transaction()?;
        tx.execute_batch(m)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

pub fn init() -> Result<()> {
    if DB.get().is_none() {
        let c = open_at(&crate::config::config().db_path)?;
        let _ = DB.set(Mutex::new(c));
    }
    Ok(())
}

/// Run `f` with the connection. Keep it short: no network calls inside.
pub fn with<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    // Hosted mode: the current tenant's database, and never a shared one.
    if let Some(t) = crate::tenant::current() {
        let guard = t.conn();
        return f(&guard);
    }
    if crate::tenant::hosted() {
        return Err(crate::util::Error::Msg("No tenant context".into()));
    }
    init()?;
    let guard = DB.get().expect("db initialised").lock().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

/// Run `f` in a transaction. `immediate` takes the write lock up front (BEGIN IMMEDIATE),
/// so two processes on one database never claim the same work.
pub fn tx<T>(immediate: bool, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    with(|c| {
        c.execute_batch(if immediate { "BEGIN IMMEDIATE" } else { "BEGIN" })?;
        match f(c) {
            Ok(v) => {
                c.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = c.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })
}

pub fn opt_row<T>(c: &Connection, sql: &str, params: impl rusqlite::Params, f: impl FnOnce(&Row) -> rusqlite::Result<T>) -> Result<Option<T>> {
    Ok(c.prepare_cached(sql)?.query_row(params, f).optional()?)
}

pub fn rows<T>(c: &Connection, sql: &str, params: impl rusqlite::Params, f: impl FnMut(&Row) -> rusqlite::Result<T>) -> Result<Vec<T>> {
    let mut st = c.prepare_cached(sql)?;
    let out = st.query_map(params, f)?.collect::<rusqlite::Result<Vec<T>>>()?;
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct AccountRow {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub handle: Option<String>,
    pub external_id: String,
    pub secret: Vec<u8>,
    pub meta: String,
    pub expires_at: Option<i64>,
    pub status: String,
}

impl AccountRow {
    pub const COLS: &'static str = "id, platform, name, handle, external_id, secret, meta, expires_at, status";
    pub fn from(r: &Row) -> rusqlite::Result<Self> {
        Ok(AccountRow {
            id: r.get(0)?,
            platform: r.get(1)?,
            name: r.get(2)?,
            handle: r.get(3)?,
            external_id: r.get(4)?,
            secret: r.get(5)?,
            meta: r.get(6)?,
            expires_at: r.get(7)?,
            status: r.get(8)?,
        })
    }
    /// `platform:handle` or `platform:name`, as the TS version labels accounts.
    pub fn label(&self) -> String {
        format!("{}:{}", self.platform, self.handle.as_deref().unwrap_or(&self.name))
    }
}

#[derive(Clone, Debug)]
pub struct MediaRow {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: i64,
    pub storage: String,
    pub key: String,
    pub url: String,
    pub alt: Option<String>,
    pub created_at: i64,
}

impl MediaRow {
    pub const COLS: &'static str = "id, filename, mime, size, storage, key, url, alt, created_at";
    pub fn from(r: &Row) -> rusqlite::Result<Self> {
        Ok(MediaRow {
            id: r.get(0)?,
            filename: r.get(1)?,
            mime: r.get(2)?,
            size: r.get(3)?,
            storage: r.get(4)?,
            key: r.get(5)?,
            url: r.get(6)?,
            alt: r.get(7)?,
            created_at: r.get(8)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PostRow {
    pub id: String,
    pub status: String,
    pub content: String,
    pub media: String,
    pub options: String,
    pub scheduled_at: Option<i64>,
    pub created_at: i64,
    pub idea_id: Option<String>,
}

impl PostRow {
    pub const COLS: &'static str = "id, status, content, media, options, scheduled_at, created_at, idea_id";
    pub fn from(r: &Row) -> rusqlite::Result<Self> {
        Ok(PostRow {
            id: r.get(0)?,
            status: r.get(1)?,
            content: r.get(2)?,
            media: r.get(3)?,
            options: r.get(4)?,
            scheduled_at: r.get(5)?,
            created_at: r.get(6)?,
            idea_id: r.get(7)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TargetRow {
    pub id: String,
    pub post_id: String,
    pub account_id: String,
    pub content: Option<String>,
    pub media: Option<String>,
    pub options: String,
    pub status: String,
    pub attempts: i64,
    pub next_attempt_at: Option<i64>,
    pub remote_id: Option<String>,
    pub remote_url: Option<String>,
    pub error: Option<String>,
    pub published_at: Option<i64>,
}

impl TargetRow {
    pub const COLS: &'static str =
        "t.id, t.post_id, t.account_id, t.content, t.media, t.options, t.status, t.attempts, t.next_attempt_at, t.remote_id, t.remote_url, t.error, t.published_at";
    pub fn from(r: &Row) -> rusqlite::Result<Self> {
        Ok(TargetRow {
            id: r.get(0)?,
            post_id: r.get(1)?,
            account_id: r.get(2)?,
            content: r.get(3)?,
            media: r.get(4)?,
            options: r.get(5)?,
            status: r.get(6)?,
            attempts: r.get(7)?,
            next_attempt_at: r.get(8)?,
            remote_id: r.get(9)?,
            remote_url: r.get(10)?,
            error: r.get(11)?,
            published_at: r.get(12)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_the_ts_schema() {
        let dir = std::env::temp_dir().join(format!("siar-rs-test-{}", crate::util::new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let c = open_at(&dir.join("siar.sqlite")).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 2);
        let want: serde_json::Value = serde_json::from_str(include_str!("meta/schema.json")).unwrap();
        let got = rows(&c, "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name", [], |r| {
            Ok(serde_json::json!({ "type": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "sql": r.get::<_, String>(2)? }))
        })
        .unwrap();
        assert_eq!(serde_json::Value::Array(got), want);
        drop(c);
        let _ = std::fs::remove_dir_all(dir);
    }
}
